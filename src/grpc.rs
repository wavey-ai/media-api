use crate::pool::{DecoderPool, PooledDecoder};
use bytes::Bytes;
use futures_util::StreamExt;
use soundkit_decoder::DecodeOptions;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, info, warn};

pub mod decode {
    tonic::include_proto!("decode");
}

use decode::decode_response::Content;
use decode::decode_service_server::{DecodeService, DecodeServiceServer};
use decode::{DecodeRequest, DecodeResponse};

pub struct GrpcDecodeService {
    decoder_pool: Arc<DecoderPool>,
    default_options: DecodeOptions,
    shutdown_token: CancellationToken,
}

impl GrpcDecodeService {
    pub fn new(
        decoder_pool: Arc<DecoderPool>,
        default_options: DecodeOptions,
        shutdown_token: CancellationToken,
    ) -> Self {
        Self {
            decoder_pool,
            default_options,
            shutdown_token,
        }
    }

    pub fn into_server(self) -> DecodeServiceServer<Self> {
        DecodeServiceServer::new(self)
    }
}

/// Collect all remaining output from a decoder synchronously
fn drain_decoder_sync(decoder: &PooledDecoder) -> Vec<Vec<u8>> {
    // Signal EOF
    let _ = decoder.send(Bytes::new());

    // Collect remaining output
    let mut pcm_chunks = Vec::new();
    let mut tries = 0;
    loop {
        match decoder.try_recv() {
            Ok(Some(Ok(audio_data))) => {
                pcm_chunks.push(audio_data.data().to_vec());
                tries = 0;
            }
            Ok(Some(Err(e))) => {
                warn!("Decode error during flush: {}", e);
            }
            Ok(None) => {
                tries += 1;
                if tries > 10 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_micros(100));
            }
            Err(()) => break,
        }
    }
    pcm_chunks
}

#[tonic::async_trait]
impl DecodeService for GrpcDecodeService {
    type DecodeStream =
        Pin<Box<dyn futures_util::Stream<Item = Result<DecodeResponse, Status>> + Send>>;

    async fn decode(
        &self,
        request: Request<Streaming<DecodeRequest>>,
    ) -> Result<Response<Self::DecodeStream>, Status> {
        let mut input_stream = request.into_inner();

        // Create output channel
        let (tx, rx) = mpsc::channel::<Result<DecodeResponse, Status>>(32);

        let decoder_pool = self.decoder_pool.clone();
        let default_options = self.default_options.clone();
        let shutdown_token = self.shutdown_token.clone();

        // Spawn task to handle decoding with support for multiple segments
        tokio::spawn(async move {
            let mut current_decoder: Option<PooledDecoder> = None;
            let mut current_options = default_options.clone();

            loop {
                tokio::select! {
                    _ = shutdown_token.cancelled() => {
                        info!("gRPC decode stream cancelled due to shutdown");
                        return;
                    }
                    chunk_result = input_stream.next() => {
                        match chunk_result {
                            Some(Ok(msg)) => {
                                // Check if this starts a new segment
                                if msg.new_segment || current_decoder.is_none() {
                                    // Drain current decoder if exists (sync collect, then async send)
                                    let chunks = if let Some(decoder) = current_decoder.take() {
                                        drain_decoder_sync(&decoder)
                                    } else {
                                        Vec::new()
                                    };
                                    for pcm in chunks {
                                        if tx.send(Ok(DecodeResponse {
                                            content: Some(Content::PcmData(pcm)),
                                        })).await.is_err() {
                                            return;
                                        }
                                    }

                                    // Update options if provided
                                    if msg.options.is_some() {
                                        current_options = match msg.options {
                                            Some(o) => DecodeOptions {
                                                output_sample_rate: if o.output_sample_rate > 0 {
                                                    Some(o.output_sample_rate)
                                                } else {
                                                    default_options.output_sample_rate
                                                },
                                                output_bits_per_sample: if o.output_bits_per_sample > 0 {
                                                    Some(o.output_bits_per_sample as u8)
                                                } else {
                                                    default_options.output_bits_per_sample
                                                },
                                                output_channels: if o.output_channels > 0 {
                                                    Some(o.output_channels as u8)
                                                } else {
                                                    default_options.output_channels
                                                },
                                            },
                                            None => default_options.clone(),
                                        };
                                    }

                                    // Acquire new decoder
                                    match decoder_pool.acquire(current_options.clone()) {
                                        Ok(decoder) => {
                                            current_decoder = Some(decoder);
                                        }
                                        Err(e) => {
                                            error!("Failed to acquire decoder: {}", e);
                                            let _ = tx.send(Err(Status::resource_exhausted(
                                                format!("Failed to acquire decoder: {}", e)
                                            ))).await;
                                            return;
                                        }
                                    }
                                }

                                // Send data to decoder and collect output
                                if current_decoder.is_some() {
                                    let decoder = current_decoder.as_ref().unwrap();

                                    // Send input
                                    if !msg.data.is_empty() {
                                        if let Err(e) = decoder.send(Bytes::from(msg.data)) {
                                            warn!("Decoder send error: {}", e);
                                            current_decoder = None;
                                            continue;
                                        }
                                    }

                                    // Collect available output synchronously
                                    let mut pcm_chunks = Vec::new();
                                    let mut decoder_closed = false;
                                    loop {
                                        match decoder.try_recv() {
                                            Ok(Some(Ok(audio_data))) => {
                                                pcm_chunks.push(audio_data.data().to_vec());
                                            }
                                            Ok(Some(Err(e))) => {
                                                warn!("Decode error: {}", e);
                                            }
                                            Ok(None) => break,
                                            Err(()) => {
                                                decoder_closed = true;
                                                break;
                                            }
                                        }
                                    }

                                    // Send collected chunks
                                    for pcm in pcm_chunks {
                                        if tx.send(Ok(DecodeResponse {
                                            content: Some(Content::PcmData(pcm)),
                                        })).await.is_err() {
                                            return;
                                        }
                                    }

                                    if decoder_closed {
                                        current_decoder = None;
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                error!("Input stream error: {}", e);
                                break;
                            }
                            None => {
                                // Stream ended - drain final decoder
                                break;
                            }
                        }
                    }
                }
            }

            // Drain final decoder
            if current_decoder.is_some() {
                let chunks = drain_decoder_sync(current_decoder.as_ref().unwrap());
                for pcm in chunks {
                    if tx.send(Ok(DecodeResponse {
                        content: Some(Content::PcmData(pcm)),
                    })).await.is_err() {
                        return;
                    }
                }
            }
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream)))
    }
}
