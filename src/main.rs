use media_api::{DecoderPool, GrpcDecodeService, MediaApiConfig, MediaRouter, TcpDecodeServer};
use soundkit_decoder::DecodeOptions;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use structopt::StructOpt;
use tls_helpers::tls_acceptor_from_base64;
use tonic::transport::Server as TonicServer;
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

    #[structopt(long, env = "ENABLE_RAW_TCP", parse(try_from_str), default_value = "true")]
    enable_raw_tcp: bool,

    #[structopt(long, env = "RAW_TCP_TLS")]
    raw_tcp_tls: bool,

    #[structopt(long, env = "GRPC_PORT", default_value = "50051")]
    grpc_port: u16,

    #[structopt(long, env = "ENABLE_GRPC", parse(try_from_str), default_value = "true")]
    enable_grpc: bool,

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
            cert_pem_base64: args.tls_cert_base64.clone(),
            privkey_pem_base64: args.tls_key_base64.clone(),
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

    // Create router for HTTP
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

    // Start TCP decode server if enabled
    if args.enable_raw_tcp {
        let tcp_shutdown_rx = shutdown_rx.clone();
        let tcp_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), args.raw_tcp_port);

        // Create decoder pool for TCP server
        let pool_size = num_cpus::get() * 2;
        let decoder_pool = Arc::new(DecoderPool::new(pool_size));

        let decode_options = DecodeOptions {
            output_sample_rate: args.default_output_sample_rate,
            output_bits_per_sample: args.default_output_bits,
            output_channels: args.default_output_channels,
        };

        // Create TLS acceptor if TLS is enabled
        let tls_acceptor = if args.raw_tcp_tls {
            match tls_acceptor_from_base64(
                &args.tls_cert_base64,
                &args.tls_key_base64,
                false,
                false,
            ) {
                Ok(acceptor) => Some(acceptor),
                Err(e) => {
                    tracing::error!("Failed to create TLS acceptor for TCP: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let tcp_server = Arc::new(TcpDecodeServer::new(decoder_pool, decode_options, tls_acceptor));

        tokio::spawn(async move {
            if let Err(e) = tcp_server.start(tcp_addr, tcp_shutdown_rx).await {
                tracing::error!("TCP server error: {}", e);
            }
        });

        info!(
            "TCP decode server started on port {} (TLS: {})",
            args.raw_tcp_port, args.raw_tcp_tls
        );
    }

    // Start gRPC server if enabled
    if args.enable_grpc {
        let grpc_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), args.grpc_port);

        // Create decoder pool for gRPC server
        let pool_size = num_cpus::get() * 2;
        let grpc_decoder_pool = Arc::new(DecoderPool::new(pool_size));

        let grpc_decode_options = DecodeOptions {
            output_sample_rate: args.default_output_sample_rate,
            output_bits_per_sample: args.default_output_bits,
            output_channels: args.default_output_channels,
        };

        let grpc_service = GrpcDecodeService::new(grpc_decoder_pool, grpc_decode_options);

        let mut grpc_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            info!("gRPC server starting on port {}", grpc_addr.port());
            if let Err(e) = TonicServer::builder()
                .add_service(grpc_service.into_server())
                .serve_with_shutdown(grpc_addr, async move {
                    let _ = grpc_shutdown_rx.changed().await;
                })
                .await
            {
                tracing::error!("gRPC server error: {}", e);
            }
        });

        info!("gRPC decode server started on port {}", args.grpc_port);
    }

    // Start HTTP/1.1 + HTTP/2 server
    let server = Http2Server::new(config.server, router);
    server.start(shutdown_rx).await?;

    info!("Server stopped");
    Ok(())
}
