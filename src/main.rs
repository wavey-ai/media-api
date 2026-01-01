use media_api::{MediaApiConfig, MediaRouter};
use std::sync::Arc;
use structopt::StructOpt;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use web_service::h2::Http2Server;
use web_service::ServerConfig;

#[derive(Debug, StructOpt)]
#[structopt(name = "media-api", about = "Streaming audio decode API service")]
struct Command {
    #[structopt(long, default_value = "8443", env = "PORT")]
    port: u16,

    #[structopt(long, env = "TLS_CERT_BASE64", default_value = "")]
    tls_cert_base64: String,

    #[structopt(long, env = "TLS_KEY_BASE64", default_value = "")]
    tls_key_base64: String,

    #[structopt(long, env = "RAW_TCP_PORT", default_value = "9000")]
    raw_tcp_port: u16,

    #[structopt(long, env = "ENABLE_RAW_TCP")]
    enable_raw_tcp: bool,

    #[structopt(long, env = "RAW_TCP_TLS")]
    raw_tcp_tls: bool,

    #[structopt(long, env = "DEFAULT_OUTPUT_SAMPLE_RATE")]
    default_output_sample_rate: Option<u32>,

    #[structopt(long, env = "DEFAULT_OUTPUT_BITS")]
    default_output_bits: Option<u8>,

    #[structopt(long, env = "DEFAULT_OUTPUT_CHANNELS")]
    default_output_channels: Option<u8>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load .env file
    let _ = dotenvy::dotenv();

    // Initialize tracing
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Command::from_args();

    info!("Starting media-api on port {}", args.port);

    let config = MediaApiConfig {
        server: ServerConfig {
            port: args.port,
            cert_pem_base64: args.tls_cert_base64,
            privkey_pem_base64: args.tls_key_base64,
            enable_h2: true,
            enable_h3: true,
            enable_webtransport: false,
            enable_websocket: false,
            enable_raw_tcp: args.enable_raw_tcp,
            raw_tcp_port: args.raw_tcp_port,
            raw_tcp_tls: args.raw_tcp_tls,
        },
        default_output_sample_rate: args.default_output_sample_rate,
        default_output_bits: args.default_output_bits,
        default_output_channels: args.default_output_channels,
    };

    // Create router
    let router = Arc::new(MediaRouter::new(config.clone()));

    // Create and start server
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());

    // Handle shutdown signals
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for ctrl+c");
        info!("Received shutdown signal");
        let _ = shutdown_tx_clone.send(());
    });

    // Start HTTP/1.1 + HTTP/2 server
    let server = Http2Server::new(config.server, router);
    server.start(shutdown_rx).await?;

    info!("Server stopped");
    Ok(())
}
