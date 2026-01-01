//! Integration tests for media-api decode service
//!
//! Tests HTTP protocol with waveform visualization for decoded output verification.

use bytes::Bytes;
use frame_header::{Endianness, FrameHeader};
use media_api::{MediaApiConfig, MediaRouter};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use web_service::h2::Http2Server;

// Load TLS certs from local files
fn load_tls_certs() -> (String, String) {
    use base64::{Engine, engine::general_purpose::STANDARD};

    let certs_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("certs");
    let cert_path = certs_dir.join("fullchain.pem");
    let key_path = certs_dir.join("privkey.pem");

    let cert = std::fs::read_to_string(&cert_path)
        .map(|s| STANDARD.encode(s))
        .unwrap_or_default();
    let key = std::fs::read_to_string(&key_path)
        .map(|s| STANDARD.encode(s))
        .unwrap_or_default();

    (cert, key)
}

// =============================================================================
// Test Helpers
// =============================================================================

fn testdata_path(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(file)
}

fn golden_path(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join("golden")
        .join(file)
}

/// Start a test server on a random port, returns the port
async fn start_test_server() -> (u16, tokio::sync::watch::Sender<()>) {
    let port = portpicker::pick_unused_port().expect("No ports available");
    let (cert, key) = load_tls_certs();

    let config = MediaApiConfig {
        server: web_service::ServerConfig {
            port,
            cert_pem_base64: cert,
            privkey_pem_base64: key,
            enable_h2: true,
            enable_websocket: false,
            enable_raw_tcp: false,
            raw_tcp_port: 0,
            raw_tcp_tls: false,
        },
        default_output_sample_rate: Some(16_000),
        default_output_bits: Some(16),
        default_output_channels: Some(1),
    };

    let router = Arc::new(MediaRouter::new(config.clone()));
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());

    let server = Http2Server::new(config.server, router);

    tokio::spawn(async move {
        let _ = server.start(shutdown_rx).await;
    });

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(200)).await;

    (port, shutdown_tx)
}

// =============================================================================
// Waveform Visualization
// =============================================================================

const WAVEFORM_WIDTH: usize = 60;
const WAVEFORM_HEIGHT: usize = 8;

/// Result from decoding
#[derive(Debug)]
struct DecodeResult {
    bytes: usize,
    sample_rate: u32,
    channels: u8,
    bits_per_sample: u8,
    rms: f64,
    waveform: Vec<f32>,
}

/// Compute waveform peaks from samples for visualization
fn compute_waveform_peaks(samples: &[i16], num_bins: usize) -> Vec<f32> {
    if samples.is_empty() || num_bins == 0 {
        return Vec::new();
    }

    let bin_size = (samples.len() + num_bins - 1) / num_bins;

    samples
        .chunks(bin_size)
        .map(|chunk| {
            let max_abs = chunk
                .iter()
                .map(|&s| (s as f32).abs())
                .fold(0.0f32, f32::max);
            max_abs / 32768.0
        })
        .collect()
}

/// Analyze PCM data and compute RMS and waveform
fn analyze_pcm(data: &[u8], sample_rate: u32, channels: u8, bits_per_sample: u8) -> DecodeResult {
    let samples_i16: Vec<i16> = data
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();

    let mut sum_of_squares = 0.0f64;
    for &sample in &samples_i16 {
        let normalized = sample as f64 / 32768.0;
        sum_of_squares += normalized * normalized;
    }

    let rms = if !samples_i16.is_empty() {
        (sum_of_squares / samples_i16.len() as f64).sqrt()
    } else {
        0.0
    };

    let waveform = compute_waveform_peaks(&samples_i16, WAVEFORM_WIDTH * 2);

    DecodeResult {
        bytes: data.len(),
        sample_rate,
        channels,
        bits_per_sample,
        rms,
        waveform,
    }
}

/// Print a single ASCII waveform
fn print_waveform(peaks: &[f32]) {
    if peaks.is_empty() {
        println!("  (no audio data)");
        return;
    }

    let chars = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    let display_peaks: Vec<f32> = if peaks.len() > WAVEFORM_WIDTH {
        (0..WAVEFORM_WIDTH)
            .map(|i| {
                let start = i * peaks.len() / WAVEFORM_WIDTH;
                let end = ((i + 1) * peaks.len() / WAVEFORM_WIDTH).min(peaks.len());
                peaks[start..end]
                    .iter()
                    .map(|x| x.abs())
                    .fold(0.0f32, f32::max)
            })
            .collect()
    } else {
        peaks.iter().map(|x| x.abs()).collect()
    };

    let max_peak = display_peaks
        .iter()
        .fold(0.0f32, |a, &b| a.max(b))
        .max(0.001);
    let half_height = WAVEFORM_HEIGHT / 2;

    // Top half
    for row in (0..half_height).rev() {
        let threshold = (row as f32 + 0.5) / half_height as f32;
        let line: String = display_peaks
            .iter()
            .map(|&p| {
                let normalized = p / max_peak;
                if normalized >= threshold {
                    let level = ((normalized - threshold) * half_height as f32
                        * (chars.len() - 1) as f32) as usize;
                    chars[level.min(chars.len() - 1)]
                } else {
                    ' '
                }
            })
            .collect();
        println!("  │{}│", line);
    }

    println!("  ├{}┤", "─".repeat(display_peaks.len()));

    // Bottom half (mirrored)
    for row in 0..half_height {
        let threshold = (row as f32 + 0.5) / half_height as f32;
        let line: String = display_peaks
            .iter()
            .map(|&p| {
                let normalized = p / max_peak;
                if normalized >= threshold {
                    let level = ((normalized - threshold) * half_height as f32
                        * (chars.len() - 1) as f32) as usize;
                    chars[level.min(chars.len() - 1)]
                } else {
                    ' '
                }
            })
            .collect();
        println!("  │{}│", line);
    }
}

/// Print waveform chart for all results
fn print_waveform_chart(protocol: &str, results: &[(&str, DecodeResult)]) {
    if results.is_empty() {
        return;
    }

    println!();
    println!("  {} Protocol - Decoded Audio Waveforms", protocol);
    println!("  {}", "═".repeat(70));
    println!();

    for (name, result) in results {
        let duration = result.bytes as f64 / 2.0 / result.sample_rate as f64;
        let db = if result.rms > 0.0 {
            20.0 * result.rms.log10()
        } else {
            -96.0
        };

        println!(
            "  {} ({}Hz {}ch {}bit, {:.2}s, {:.1} dB)",
            name, result.sample_rate, result.channels, result.bits_per_sample, duration, db
        );
        print_waveform(&result.waveform);
        println!();
    }
}

// =============================================================================
// HTTP Tests
// =============================================================================

async fn http_decode(port: u16, data: Bytes) -> Result<(Vec<u8>, u32, u8, u8), String> {
    use std::net::SocketAddr;

    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let client = reqwest::Client::builder()
        .resolve("local.wavey.ai", addr)
        .build()
        .map_err(|e| format!("Failed to build client: {}", e))?;

    let response = client
        .post(format!("https://local.wavey.ai:{}/decode", port))
        .body(data.to_vec())
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP error: {}", response.status()));
    }

    let sample_rate: u32 = response
        .headers()
        .get("X-Sample-Rate")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(16000);

    let channels: u8 = response
        .headers()
        .get("X-Channels")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let bits: u8 = response
        .headers()
        .get("X-Bits-Per-Sample")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);

    let body = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read body: {}", e))?;

    Ok((body.to_vec(), sample_rate, channels, bits))
}

#[tokio::test]
async fn test_http_decode_all_formats() {
    let (port, shutdown) = start_test_server().await;

    let formats = [
        ("mp3", "mp3/A_Tusk_is_used_to_make_costly_gifts.mp3"),
        ("flac", "flac/A_Tusk_is_used_to_make_costly_gifts.flac"),
        ("aac", "aac/A_Tusk_is_used_to_make_costly_gifts.aac"),
        ("m4a", "m4a/A_Tusk_is_used_to_make_costly_gifts.m4a"),
        ("m4a_slow", "m4a_slow/A_Tusk_is_used_to_make_costly_gifts.m4a"),
        ("opus", "opus/A_Tusk_is_used_to_make_costly_gifts.opus"),
        (
            "ogg_opus",
            "ogg_opus/A_Tusk_is_used_to_make_costly_gifts.ogg",
        ),
        ("webm", "webm/A_Tusk_is_used_to_make_costly_gifts.webm"),
    ];

    let mut results: Vec<(&str, DecodeResult)> = Vec::new();
    let out_dir = golden_path("");
    fs::create_dir_all(&out_dir).ok();

    for (name, path) in formats {
        let input_path = testdata_path(path);
        if !input_path.exists() {
            eprintln!("  {} - skipped (file not found)", name);
            continue;
        }

        let data = Bytes::from(fs::read(&input_path).unwrap());

        match http_decode(port, data).await {
            Ok((pcm_data, sample_rate, channels, bits)) => {
                if pcm_data.is_empty() {
                    eprintln!("  {} - empty response", name);
                    continue;
                }

                // Write golden output
                let output_path = out_dir.join(format!("http_{}.s16le", name));
                let mut f = fs::File::create(&output_path).unwrap();
                f.write_all(&pcm_data).unwrap();

                let result = analyze_pcm(&pcm_data, sample_rate, channels, bits);
                results.push((name, result));
            }
            Err(e) => {
                eprintln!("  {} - {}", name, e);
            }
        }
    }

    let _ = shutdown.send(());

    assert!(!results.is_empty(), "No formats decoded successfully");
    print_waveform_chart("HTTP", &results);
}

// =============================================================================
// TCP Tests (with frame-header framing)
// =============================================================================

#[allow(dead_code)]
async fn tcp_decode(port: u16, data: Bytes) -> Result<(Vec<u8>, u32, u8, u8), String> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))
        .await
        .map_err(|e| format!("TCP connect failed: {}", e))?;

    // For raw TCP, we need to frame the input with frame-header
    // Create a simple PCM frame header for the encoded data (treating it as raw bytes)
    // Actually, for the decoder, we send raw encoded bytes and it auto-detects format
    // But our TCP handler expects frame-header framed input

    // Let's create a simple frame with the data
    // Using PCMSigned as a placeholder encoding flag for raw bytes
    let sample_count = data.len().min(0xFFF) as u16;
    let header = FrameHeader::new(
        frame_header::EncodingFlag::PCMSigned,
        sample_count,
        48000,
        1,
        16,
        Endianness::LittleEndian,
        None,
        None,
    )
    .map_err(|e| format!("Failed to create frame header: {}", e))?;

    let mut frame = Vec::with_capacity(header.size() + data.len());
    header
        .encode(&mut frame)
        .map_err(|e| format!("Failed to encode header: {}", e))?;
    frame.extend_from_slice(&data);

    stream
        .write_all(&frame)
        .await
        .map_err(|e| format!("TCP write failed: {}", e))?;

    // Send EOF frame (zero size)
    let eof_header = FrameHeader::new(
        frame_header::EncodingFlag::PCMSigned,
        0,
        48000,
        1,
        16,
        Endianness::LittleEndian,
        None,
        None,
    )
    .unwrap();
    let mut eof_frame = Vec::new();
    eof_header.encode(&mut eof_frame).unwrap();
    stream.write_all(&eof_frame).await.ok();
    stream.flush().await.ok();

    // Read responses
    let mut pcm_data = Vec::new();
    let mut sample_rate = 16000u32;
    let mut channels = 1u8;
    let mut bits = 16u8;

    let mut header_buf = [0u8; 20];
    let timeout = Duration::from_secs(5);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > timeout {
            break;
        }

        // Read base header (4 bytes)
        match tokio::time::timeout(Duration::from_millis(100), stream.read_exact(&mut header_buf[..4])).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break,
        }

        // Decode header
        let header = match FrameHeader::decode(&mut &header_buf[..4]) {
            Ok(h) => h,
            Err(_) => break,
        };

        if header.sample_size() == 0 {
            // EOF frame
            break;
        }

        sample_rate = header.sample_rate();
        channels = header.channels();
        bits = header.bits_per_sample();

        // Calculate payload size
        let bytes_per_sample = header.bits_per_sample() as usize / 8;
        let payload_size = header.sample_size() as usize * bytes_per_sample * header.channels() as usize;

        if payload_size > 0 {
            let mut payload = vec![0u8; payload_size];
            match stream.read_exact(&mut payload).await {
                Ok(_) => pcm_data.extend_from_slice(&payload),
                Err(_) => break,
            }
        }
    }

    Ok((pcm_data, sample_rate, channels, bits))
}

// Note: TCP test requires the RawTcpHandler to be started separately
// For now we'll skip this test as the main server only starts HTTP
#[tokio::test]
#[ignore = "TCP handler requires separate raw TCP server setup"]
async fn test_tcp_decode_all_formats() {
    // This test would need the RawTcpServer to be started alongside Http2Server
    println!("TCP tests require separate server setup - skipped");
}

// =============================================================================
// Streaming Tests
// =============================================================================

#[tokio::test]
async fn test_http_chunked_streaming() {
    let (port, shutdown) = start_test_server().await;

    let input_path = testdata_path("mp3/A_Tusk_is_used_to_make_costly_gifts.mp3");
    let data = fs::read(&input_path).unwrap();

    use std::net::SocketAddr;

    // Send data in chunks
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let client = reqwest::Client::builder()
        .resolve("local.wavey.ai", addr)
        .build()
        .expect("Failed to build client");
    let response = client
        .post(format!("https://local.wavey.ai:{}/decode", port))
        .body(data)
        .send()
        .await
        .expect("Request failed");

    assert!(response.status().is_success());

    let body = response.bytes().await.unwrap();
    assert!(!body.is_empty(), "Expected decoded PCM data");

    let result = analyze_pcm(&body, 16000, 1, 16);
    println!("\n  HTTP Chunked Streaming Test");
    println!("  {}", "═".repeat(50));
    println!(
        "  mp3 -> PCM: {} bytes, {:.1} dB RMS",
        result.bytes,
        if result.rms > 0.0 {
            20.0 * result.rms.log10()
        } else {
            -96.0
        }
    );
    print_waveform(&result.waveform);

    let _ = shutdown.send(());
}

// =============================================================================
// Health Check Tests
// =============================================================================

#[tokio::test]
async fn test_health_endpoint() {
    use std::net::SocketAddr;

    let (port, shutdown) = start_test_server().await;

    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let client = reqwest::Client::builder()
        .resolve("local.wavey.ai", addr)
        .build()
        .expect("Failed to build client");
    let response = client
        .get(format!("https://local.wavey.ai:{}/health", port))
        .send()
        .await
        .expect("Request failed");

    assert!(response.status().is_success());

    let body: serde_json::Value = response.json().await.expect("JSON parse failed");
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "media-api");

    let _ = shutdown.send(());
}

#[tokio::test]
async fn test_status_endpoint() {
    use std::net::SocketAddr;

    let (port, shutdown) = start_test_server().await;

    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let client = reqwest::Client::builder()
        .resolve("local.wavey.ai", addr)
        .build()
        .expect("Failed to build client");
    let response = client
        .get(format!("https://local.wavey.ai:{}/status", port))
        .send()
        .await
        .expect("Request failed");

    assert!(response.status().is_success());

    let body: serde_json::Value = response.json().await.expect("JSON parse failed");
    assert_eq!(body["status"], "ok");

    let _ = shutdown.send(());
}
