use std::env;
use web_service::ServerConfig;

#[derive(Debug, Clone)]
pub struct MediaApiConfig {
    pub server: ServerConfig,
    pub default_output_sample_rate: Option<u32>,
    pub default_output_bits: Option<u8>,
    pub default_output_channels: Option<u8>,
}

impl MediaApiConfig {
    pub fn from_env() -> Self {
        let _ = dotenvy::dotenv();

        let cert_pem_base64 = env::var("TLS_CERT_BASE64").unwrap_or_default();
        let privkey_pem_base64 = env::var("TLS_KEY_BASE64").unwrap_or_default();

        let port = env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8443);

        let raw_tcp_port = env::var("RAW_TCP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(9000);

        let enable_raw_tcp = env::var("ENABLE_RAW_TCP")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(true);

        let raw_tcp_tls = env::var("RAW_TCP_TLS")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);

        let default_output_sample_rate = env::var("DEFAULT_OUTPUT_SAMPLE_RATE")
            .ok()
            .and_then(|v| v.parse().ok());

        let default_output_bits = env::var("DEFAULT_OUTPUT_BITS")
            .ok()
            .and_then(|v| v.parse().ok());

        let default_output_channels = env::var("DEFAULT_OUTPUT_CHANNELS")
            .ok()
            .and_then(|v| v.parse().ok());

        Self {
            server: ServerConfig {
                cert_pem_base64,
                privkey_pem_base64,
                port,
                enable_h2: true,
                enable_h3: true,
                enable_webtransport: false,
                enable_websocket: true,
                enable_raw_tcp,
                raw_tcp_port,
                raw_tcp_tls,
            },
            default_output_sample_rate,
            default_output_bits,
            default_output_channels,
        }
    }
}
