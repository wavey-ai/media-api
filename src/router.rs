use crate::config::MediaApiConfig;
use crate::decode::{create_pipeline, parse_decode_options};
use async_trait::async_trait;
use bytes::Bytes;
use frame_header::EncodingFlag;
use futures_util::StreamExt;
use http::{Request, StatusCode};
use serde_json::json;
use soundkit_decoder::DecodeOptions;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use web_service::{
    BodyStream, HandlerResponse, HandlerResult, Router, ServerError, StreamWriter,
    WebSocketHandler, WebTransportHandler,
};

/// Maximum concurrent decode operations to prevent resource exhaustion
const MAX_CONCURRENT_DECODES: usize = 200;

pub struct MediaRouter {
    config: Arc<MediaApiConfig>,
    /// Semaphore to limit concurrent decode operations
    /// Each decode gets a fresh decoder - the semaphore ensures we don't
    /// overwhelm the system with too many concurrent decoder threads
    decode_semaphore: Arc<Semaphore>,
}

impl MediaRouter {
    pub fn new(config: MediaApiConfig) -> Self {
        let config = Arc::new(config);
        Self {
            config,
            decode_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_DECODES)),
        }
    }

    fn merge_options(&self, query_options: DecodeOptions) -> DecodeOptions {
        DecodeOptions {
            output_sample_rate: query_options
                .output_sample_rate
                .or(self.config.default_output_sample_rate),
            output_bits_per_sample: query_options
                .output_bits_per_sample
                .or(self.config.default_output_bits),
            output_channels: query_options
                .output_channels
                .or(self.config.default_output_channels),
        }
    }

    async fn handle_status(&self) -> HandlerResult<HandlerResponse> {
        let body = json!({
            "status": "ok",
            "service": "media-api",
            "version": env!("CARGO_PKG_VERSION"),
        });

        Ok(HandlerResponse {
            status: StatusCode::OK,
            body: Some(Bytes::from(body.to_string())),
            content_type: Some("application/json".to_string()),
            headers: vec![],
            etag: None,
        })
    }

    async fn handle_decode(
        &self,
        req: Request<()>,
        mut body: BodyStream,
    ) -> HandlerResult<HandlerResponse> {
        // Acquire a decode slot from the pool - this ensures we don't create
        // too many concurrent decoder threads which can exhaust system resources
        let _permit = self.decode_semaphore.acquire().await.map_err(|_| {
            ServerError::Handler(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Decoder pool exhausted",
            )))
        })?;

        let query = req.uri().query();
        let query_options = parse_decode_options(query);
        let options = self.merge_options(query_options);

        // Create a fresh pipeline for this request - each request gets its own
        // decoder instance to ensure complete state isolation
        let mut pipeline = create_pipeline(options);

        // Collect all decoded output
        let mut output_bytes = Vec::new();
        let mut first_frame_info: Option<(u32, u8, u8, EncodingFlag)> = None;

        // Feed input chunks to pipeline
        while let Some(chunk_result) = body.next().await {
            match chunk_result {
                Ok(chunk) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    if let Err(e) = pipeline.send(chunk) {
                        tracing::warn!("Pipeline send error: {}", e);
                        break;
                    }
                    // Drain any available output while we're feeding
                    while let Some(output) = pipeline.try_recv() {
                        match output {
                            Ok(audio_data) => {
                                if first_frame_info.is_none() {
                                    first_frame_info = Some((
                                        audio_data.sampling_rate(),
                                        audio_data.channel_count(),
                                        audio_data.bits_per_sample(),
                                        audio_data.audio_format(),
                                    ));
                                }
                                output_bytes.extend_from_slice(audio_data.data());
                            }
                            Err(e) => {
                                tracing::warn!("Decode error: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Body stream error: {}", e);
                    break;
                }
            }
        }

        // Signal EOF and drain remaining output
        let _ = pipeline.send(Bytes::new());

        // Wait for remaining output with timeout
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(5);
        let mut idle_count = 0;
        loop {
            if start.elapsed() > timeout {
                break;
            }
            match pipeline.try_recv() {
                Some(Ok(audio_data)) => {
                    idle_count = 0;
                    if first_frame_info.is_none() {
                        first_frame_info = Some((
                            audio_data.sampling_rate(),
                            audio_data.channel_count(),
                            audio_data.bits_per_sample(),
                            audio_data.audio_format(),
                        ));
                    }
                    output_bytes.extend_from_slice(audio_data.data());
                }
                Some(Err(e)) => {
                    idle_count = 0;
                    tracing::warn!("Decode error during flush: {}", e);
                }
                None => {
                    idle_count += 1;
                    // Wait longer for decoder to process
                    if idle_count > 100 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }

        if output_bytes.is_empty() {
            return Ok(HandlerResponse {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                body: Some(Bytes::from(
                    json!({"error": "Failed to decode audio"}).to_string(),
                )),
                content_type: Some("application/json".to_string()),
                headers: vec![],
                etag: None,
            });
        }

        let mut headers = vec![];
        if let Some((sample_rate, channels, bits, encoding)) = first_frame_info {
            headers.push(("X-Sample-Rate".to_string(), sample_rate.to_string()));
            headers.push(("X-Channels".to_string(), channels.to_string()));
            headers.push(("X-Bits-Per-Sample".to_string(), bits.to_string()));
            headers.push((
                "X-Encoding".to_string(),
                match encoding {
                    EncodingFlag::PCMFloat => "pcm-float",
                    _ => "pcm-signed",
                }
                .to_string(),
            ));
        }

        Ok(HandlerResponse {
            status: StatusCode::OK,
            body: Some(Bytes::from(output_bytes)),
            content_type: Some("application/octet-stream".to_string()),
            headers,
            etag: None,
        })
    }
}

#[async_trait]
impl Router for MediaRouter {
    async fn route(&self, req: Request<()>) -> HandlerResult<HandlerResponse> {
        let path = req.uri().path();

        match path {
            "/status" | "/health" => self.handle_status().await,
            _ => Ok(HandlerResponse {
                status: StatusCode::NOT_FOUND,
                body: Some(Bytes::from(
                    json!({"error": "Not found"}).to_string(),
                )),
                content_type: Some("application/json".to_string()),
                headers: vec![],
                etag: None,
            }),
        }
    }

    async fn route_body(
        &self,
        req: Request<()>,
        body: BodyStream,
    ) -> HandlerResult<HandlerResponse> {
        let path = req.uri().path();

        match path {
            "/decode" => self.handle_decode(req, body).await,
            _ => self.route(req).await,
        }
    }

    fn has_body_handler(&self, path: &str) -> bool {
        matches!(path, "/decode")
    }

    fn is_streaming(&self, _path: &str) -> bool {
        false
    }

    async fn route_stream(
        &self,
        _req: Request<()>,
        _stream_writer: Box<dyn StreamWriter>,
    ) -> HandlerResult<()> {
        Err(ServerError::Handler(Box::new(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Streaming not supported",
        ))))
    }

    fn webtransport_handler(&self) -> Option<&dyn WebTransportHandler> {
        None
    }

    fn websocket_handler(&self, _path: &str) -> Option<&dyn WebSocketHandler> {
        None
    }
}
