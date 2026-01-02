use crate::pool::DecoderPool;
use bytes::Bytes;
use frame_header::EncodingFlag;
use futures_util::StreamExt;
use soundkit_decoder::DecodeOptions;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, warn};

pub mod decode {
    tonic::include_proto!("decode");
}

use decode::decode_response::Content;
use decode::decode_service_server::{DecodeService, DecodeServiceServer};
use decode::{AudioMetadata, DecodeRequest, DecodeResponse, PcmEncoding};

pub struct GrpcDecodeService {
    decoder_pool: Arc<DecoderPool>,
    default_options: DecodeOptions,
}

impl GrpcDecodeService {
    pub fn new(decoder_pool: Arc<DecoderPool>, default_options: DecodeOptions) -> Self {
        Self {
            decoder_pool,
            default_options,
        }
    }

    pub fn into_server(self) -> DecodeServiceServer<Self> {
        DecodeServiceServer::new(self)
    }

    fn merge_options(&self, opts: Option<decode::DecodeOptions>) -> DecodeOptions {
        match opts {
            Some(o) => DecodeOptions {
                output_sample_rate: if o.output_sample_rate > 0 {
                    Some(o.output_sample_rate)
                } else {
                    self.default_options.output_sample_rate
                },
                output_bits_per_sample: if o.output_bits_per_sample > 0 {
                    Some(o.output_bits_per_sample as u8)
                } else {
                    self.default_options.output_bits_per_sample
                },
                output_channels: if o.output_channels > 0 {
                    Some(o.output_channels as u8)
                } else {
                    self.default_options.output_channels
                },
            },
            None => self.default_options.clone(),
        }
    }
}

#[tonic::async_trait]
impl DecodeService for GrpcDecodeService {
    type DecodeStream = Pin<Box<dyn futures_util::Stream<Item = Result<DecodeResponse, Status>> + Send>>;

    async fn decode(
        &self,
        request: Request<Streaming<DecodeRequest>>,
    ) -> Result<Response<Self::DecodeStream>, Status> {
        let mut input_stream = request.into_inner();

        // Read first message to get options
        let first_msg = input_stream
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("Empty request stream"))?
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

        let options = self.merge_options(first_msg.options);

        // Acquire decoder from pool
        let decoder = self
            .decoder_pool
            .acquire(options)
            .map_err(|e| Status::resource_exhausted(format!("Failed to acquire decoder: {}", e)))?;

        // Create output channel
        let (tx, rx) = mpsc::channel::<Result<DecodeResponse, Status>>(32);

        // Send first chunk if present
        if !first_msg.data.is_empty() {
            if let Err(e) = decoder.send(Bytes::from(first_msg.data)) {
                return Err(Status::internal(format!("Decoder send error: {}", e)));
            }
        }

        // Spawn task to handle decoding
        tokio::spawn(async move {
            let mut first_meta_sent = false;

            // Process input stream
            while let Some(chunk_result) = input_stream.next().await {
                match chunk_result {
                    Ok(chunk) => {
                        if !chunk.data.is_empty() {
                            if let Err(e) = decoder.send(Bytes::from(chunk.data)) {
                                warn!("Decoder send error: {}", e);
                                break;
                            }
                        }

                        // Drain available output
                        loop {
                            match decoder.try_recv() {
                                Ok(Some(Ok(audio_data))) => {
                                    // Send metadata before first PCM chunk
                                    if !first_meta_sent {
                                        let meta = AudioMetadata {
                                            sample_rate: audio_data.sampling_rate(),
                                            channels: audio_data.channel_count() as u32,
                                            bits_per_sample: audio_data.bits_per_sample() as u32,
                                            encoding: match audio_data.audio_format() {
                                                EncodingFlag::PCMFloat => PcmEncoding::PcmFloat as i32,
                                                _ => PcmEncoding::PcmSigned as i32,
                                            },
                                        };
                                        if tx
                                            .send(Ok(DecodeResponse {
                                                content: Some(Content::Metadata(meta)),
                                            }))
                                            .await
                                            .is_err()
                                        {
                                            return;
                                        }
                                        first_meta_sent = true;
                                    }

                                    // Send PCM data
                                    if tx
                                        .send(Ok(DecodeResponse {
                                            content: Some(Content::PcmData(
                                                audio_data.data().to_vec(),
                                            )),
                                        }))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                Ok(Some(Err(e))) => {
                                    warn!("Decode error: {}", e);
                                }
                                Ok(None) => break,
                                Err(()) => break,
                            }
                        }
                    }
                    Err(e) => {
                        error!("Input stream error: {}", e);
                        break;
                    }
                }
            }

            // Signal EOF and drain remaining output
            let _ = decoder.send(Bytes::new());

            loop {
                match decoder.recv() {
                    Some(Ok(audio_data)) => {
                        if !first_meta_sent {
                            let meta = AudioMetadata {
                                sample_rate: audio_data.sampling_rate(),
                                channels: audio_data.channel_count() as u32,
                                bits_per_sample: audio_data.bits_per_sample() as u32,
                                encoding: match audio_data.audio_format() {
                                    EncodingFlag::PCMFloat => PcmEncoding::PcmFloat as i32,
                                    _ => PcmEncoding::PcmSigned as i32,
                                },
                            };
                            if tx
                                .send(Ok(DecodeResponse {
                                    content: Some(Content::Metadata(meta)),
                                }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            first_meta_sent = true;
                        }

                        if tx
                            .send(Ok(DecodeResponse {
                                content: Some(Content::PcmData(audio_data.data().to_vec())),
                            }))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Some(Err(e)) => {
                        warn!("Decode error during flush: {}", e);
                    }
                    None => break,
                }
            }
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream)))
    }
}
