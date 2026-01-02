use crate::config::MediaApiConfig;
use crate::decode::parse_decode_options;
use crate::pool::DecoderPool;
use crate::stats::Stats;
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use http::{Request, Response, StatusCode};
use serde_json::json;
use soundkit_decoder::DecodeOptions;
use std::sync::Arc;
use web_service::{
    BodyStream, HandlerResponse, HandlerResult, Router, ServerError, StreamWriter,
    WebSocketHandler, WebTransportHandler,
};

pub struct MediaRouter {
    config: Arc<MediaApiConfig>,
    /// Pool of decoder worker threads
    /// Workers are pre-spawned and wait for jobs - no thread spawn per request
    decoder_pool: Arc<DecoderPool>,
    /// Connection statistics for monitoring
    stats: Arc<Stats>,
}

impl MediaRouter {
    pub fn new(config: MediaApiConfig) -> Self {
        let config = Arc::new(config);
        let stats = Arc::new(Stats::new());
        // Pool size = 2x CPUs: always have workers ready, one in reserve
        // Max concurrent = num_cpus (pool's bounded channel provides backpressure)
        let pool_size = stats.num_cpus() * 2;
        Self {
            config,
            decoder_pool: Arc::new(DecoderPool::new(pool_size)),
            stats,
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

    async fn handle_stats(&self) -> HandlerResult<HandlerResponse> {
        let body = self.stats.to_json();

        Ok(HandlerResponse {
            status: StatusCode::OK,
            body: Some(Bytes::from(body.to_string())),
            content_type: Some("application/json".to_string()),
            headers: vec![],
            etag: None,
        })
    }

    /// Streaming decode handler - streams PCM output as audio is decoded.
    /// Wire format:
    /// - First 7 bytes: metadata (sample_rate:4, channels:1, bits:1, encoding:1)
    /// - Remaining bytes: raw PCM data
    async fn handle_decode_stream(
        &self,
        req: Request<()>,
        mut body: BodyStream,
        mut stream_writer: Box<dyn StreamWriter>,
    ) -> HandlerResult<()> {
        // Track connection for stats
        self.stats.connection_start();
        let stats = self.stats.clone();

        // Ensure we track connection end even on early returns
        struct ConnectionGuard(Arc<Stats>);
        impl Drop for ConnectionGuard {
            fn drop(&mut self) {
                self.0.connection_end();
            }
        }
        let _guard = ConnectionGuard(stats);

        let query = req.uri().query();
        let query_options = parse_decode_options(query);
        let options = self.merge_options(query_options);

        // Acquire a decoder from the pool - blocks if all workers are busy
        let decoder = self.decoder_pool.acquire(options).map_err(|e| {
            ServerError::Handler(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                e,
            )))
        })?;

        // Send response headers immediately
        let response = Response::builder()
            .status(StatusCode::OK)
            .body(())
            .map_err(ServerError::Http)?;
        stream_writer.send_response(response).await?;

        // Feed input chunks to decoder and stream output
        while let Some(chunk_result) = body.next().await {
            match chunk_result {
                Ok(chunk) => {
                    if chunk.is_empty() {
                        continue;
                    }
                    if let Err(e) = decoder.send(chunk) {
                        tracing::warn!("Decoder send error: {}", e);
                        break;
                    }
                    // Drain any available output and stream it
                    loop {
                        match decoder.try_recv() {
                            Ok(Some(Ok(audio_data))) => {
                                stream_writer.send_data(Bytes::copy_from_slice(audio_data.data())).await?;
                            }
                            Ok(Some(Err(e))) => {
                                tracing::warn!("Decode error: {}", e);
                            }
                            Ok(None) => break, // No data ready
                            Err(()) => break,  // Channel closed
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
        let _ = decoder.send(Bytes::new());

        // Wait for decoder to finish - use try_recv with yields to avoid blocking
        loop {
            match decoder.try_recv() {
                Ok(Some(Ok(audio_data))) => {
                    stream_writer.send_data(Bytes::copy_from_slice(audio_data.data())).await?;
                }
                Ok(Some(Err(e))) => {
                    tracing::warn!("Decode error during flush: {}", e);
                }
                Ok(None) => {
                    // No data ready yet, yield to let other tasks run
                    tokio::task::yield_now().await;
                }
                Err(()) => break, // Channel closed, decoder finished
            }
        }

        stream_writer.finish().await
    }
}

#[async_trait]
impl Router for MediaRouter {
    async fn route(&self, req: Request<()>) -> HandlerResult<HandlerResponse> {
        let path = req.uri().path();

        match path {
            "/status" | "/health" => self.handle_status().await,
            "/stats" => self.handle_stats().await,
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

    fn has_body_handler(&self, _path: &str) -> bool {
        false
    }

    fn is_streaming(&self, _path: &str) -> bool {
        false
    }

    fn is_body_streaming(&self, path: &str) -> bool {
        // /decode uses bidirectional streaming
        path == "/decode"
    }

    async fn route_body_stream(
        &self,
        req: Request<()>,
        body: BodyStream,
        stream_writer: Box<dyn StreamWriter>,
    ) -> HandlerResult<()> {
        let path = req.uri().path();

        match path {
            "/decode" => self.handle_decode_stream(req, body, stream_writer).await,
            _ => Err(ServerError::Handler(Box::new(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Not found",
            )))),
        }
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
