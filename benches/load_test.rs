//! Load test benchmark for media-api decode service
//!
//! Measures throughput in audio seconds per wallclock second for each format.
//!
//! Run with: cargo run --release --bin load_test

use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use media_api::{MediaApiConfig, MediaRouter};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use web_service::h2::Http2Server;
use web_service::{Router, ServerConfig};

fn load_tls_certs() -> (String, String) {
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

const OUTPUT_DIR: &str = "load_test_outputs";

// =============================================================================
// Configuration
// =============================================================================

const MAX_FILES_PER_FORMAT: usize = 50;
const WARMUP_REQUESTS: usize = 3;
const THROUGHPUT_BUCKET_MS: u64 = 500;

fn get_concurrency_levels() -> Vec<usize> {
    let cpus = num_cpus::get();
    vec![cpus / 2, cpus, cpus * 2, cpus * 3]
}

// =============================================================================
// Test Data Loading
// =============================================================================

fn estimate_audio_duration(format: &str, file_size: usize) -> f64 {
    let bitrate_kbps = match format {
        "mp3" => 48.0,
        "flac" => 400.0,
        "opus" => 16.0,
        "ogg_opus" => 110.0,
        "webm" => 64.0,
        "mac_aac" => 64.0,
        "aac_adts" => 64.0,
        "wav_24" | "wav_32f" | "wav_stereo" => 768.0,
        "linear16" => 256.0,
        "linear16_48" => 768.0,
        "linear32" => 512.0,
        "linear32_48" => 1536.0,
        "g729" => 8.0,
        _ => 64.0,
    };
    (file_size as f64 * 8.0) / (bitrate_kbps * 1000.0)
}

fn benchmark_testdata_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmark_testdata")
}

#[derive(Clone)]
struct AudioFile {
    name: String,
    data: Bytes,
    estimated_duration: f64,
}

fn load_format_files(format: &str) -> Vec<AudioFile> {
    let base_path = benchmark_testdata_path();
    let format_path = base_path.join(format);

    if !format_path.exists() {
        return Vec::new();
    }

    let mut files: Vec<AudioFile> = fs::read_dir(&format_path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_string_lossy().to_string();
            let data = fs::read(&path).ok()?;
            let estimated_duration = estimate_audio_duration(format, data.len());
            Some(AudioFile {
                name,
                data: Bytes::from(data),
                estimated_duration,
            })
        })
        .collect();

    files.sort_by(|a, b| a.name.cmp(&b.name));
    files.truncate(MAX_FILES_PER_FORMAT);
    files
}

// =============================================================================
// HTTP Client
// =============================================================================

fn create_http_client(concurrency: usize) -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(concurrency)
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Failed to create HTTP client")
}

async fn decode_request(client: &reqwest::Client, port: u16, data: Bytes) -> Result<Vec<u8>, String> {
    let url = format!("https://local.wavey.ai:{}/decode?sample_rate=16000&bits=16&channels=1", port);
    let response = client
        .post(&url)
        .body(data.to_vec())
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("Body read failed: {}", e))
}

// =============================================================================
// Throughput Tracking
// =============================================================================

/// Tracks completion times for throughput graphing
struct ThroughputTracker {
    /// Completion timestamps in ms from start
    completions: std::sync::Mutex<Vec<u64>>,
    start: Instant,
}

impl ThroughputTracker {
    fn new(start: Instant) -> Self {
        Self {
            completions: std::sync::Mutex::new(Vec::new()),
            start,
        }
    }

    fn record_completion(&self) {
        let elapsed_ms = self.start.elapsed().as_millis() as u64;
        if let Ok(mut completions) = self.completions.lock() {
            completions.push(elapsed_ms);
        }
    }

    /// Get throughput in files/sec for each time bucket
    fn get_throughput_buckets(&self, bucket_ms: u64) -> Vec<(u64, f64)> {
        let completions = self.completions.lock().unwrap();
        if completions.is_empty() {
            return Vec::new();
        }

        let max_time = *completions.iter().max().unwrap_or(&0);
        let num_buckets = (max_time / bucket_ms) + 1;

        let mut buckets = vec![0u64; num_buckets as usize];
        for &t in completions.iter() {
            let bucket = (t / bucket_ms) as usize;
            if bucket < buckets.len() {
                buckets[bucket] += 1;
            }
        }

        // Convert to files/sec
        let bucket_secs = bucket_ms as f64 / 1000.0;
        buckets
            .into_iter()
            .enumerate()
            .map(|(i, count)| {
                let time_ms = (i as u64) * bucket_ms;
                let files_per_sec = count as f64 / bucket_secs;
                (time_ms, files_per_sec)
            })
            .collect()
    }
}

// =============================================================================
// Format Statistics
// =============================================================================

struct FormatStats {
    requests: AtomicUsize,
    successes: AtomicUsize,
    failures: AtomicUsize,
    total_audio_seconds: std::sync::Mutex<f64>,
    total_latency_ms: AtomicU64,
    min_latency_ms: AtomicU64,
    max_latency_ms: AtomicU64,
    total_output_bytes: AtomicU64,
    throughput: Arc<ThroughputTracker>,
}

impl FormatStats {
    fn new(start: Instant) -> Self {
        Self {
            requests: AtomicUsize::new(0),
            successes: AtomicUsize::new(0),
            failures: AtomicUsize::new(0),
            total_audio_seconds: std::sync::Mutex::new(0.0),
            total_latency_ms: AtomicU64::new(0),
            min_latency_ms: AtomicU64::new(u64::MAX),
            max_latency_ms: AtomicU64::new(0),
            total_output_bytes: AtomicU64::new(0),
            throughput: Arc::new(ThroughputTracker::new(start)),
        }
    }

    fn record_success(&self, output_bytes: usize, audio_seconds: f64, latency: Duration) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.successes.fetch_add(1, Ordering::Relaxed);
        self.total_output_bytes.fetch_add(output_bytes as u64, Ordering::Relaxed);
        self.throughput.record_completion();

        let latency_ms = latency.as_millis() as u64;
        self.total_latency_ms.fetch_add(latency_ms, Ordering::Relaxed);
        self.min_latency_ms.fetch_min(latency_ms, Ordering::Relaxed);
        self.max_latency_ms.fetch_max(latency_ms, Ordering::Relaxed);

        if let Ok(mut total) = self.total_audio_seconds.lock() {
            *total += audio_seconds;
        }
    }

    fn record_failure(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.failures.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct FormatResult {
    format: String,
    requests: usize,
    successes: usize,
    failures: usize,
    total_audio_seconds: f64,
    wallclock_seconds: f64,
    avg_latency_ms: f64,
    min_latency_ms: u64,
    max_latency_ms: u64,
    output_mb: f64,
    throughput_buckets: Vec<(u64, f64)>,
}

#[allow(dead_code)]
impl FormatResult {
    fn realtime_multiplier(&self) -> f64 {
        if self.wallclock_seconds > 0.0 {
            self.total_audio_seconds / self.wallclock_seconds
        } else {
            0.0
        }
    }

    fn throughput_mbps(&self) -> f64 {
        if self.wallclock_seconds > 0.0 {
            self.output_mb / self.wallclock_seconds
        } else {
            0.0
        }
    }

    fn files_per_second(&self) -> f64 {
        if self.wallclock_seconds > 0.0 {
            self.successes as f64 / self.wallclock_seconds
        } else {
            0.0
        }
    }
}

// =============================================================================
// Load Test Runner
// =============================================================================

async fn run_format_benchmark(
    client: &reqwest::Client,
    port: u16,
    format: &str,
    files: &[AudioFile],
    concurrency: usize,
) -> FormatResult {
    let start = Instant::now();
    let stats = Arc::new(FormatStats::new(start));
    let sample_saved = Arc::new(AtomicBool::new(false));
    let format_for_save = format.to_string();

    // Warmup
    let warmup_sem = Arc::new(Semaphore::new(concurrency));
    for file in files.iter().take(WARMUP_REQUESTS) {
        let _permit = warmup_sem.acquire().await.unwrap();
        let _ = decode_request(client, port, file.data.clone()).await;
    }

    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut handles = Vec::new();

    for file in files.iter() {
        let permit = semaphore.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        let file = file.clone();
        let stats = stats.clone();
        let sample_saved = sample_saved.clone();
        let format_for_save = format_for_save.clone();

        let handle = tokio::spawn(async move {
            let req_start = Instant::now();

            let result = decode_request(&client, port, file.data).await;
            let latency = req_start.elapsed();

            match result {
                Ok(output) => {
                    stats.record_success(output.len(), file.estimated_duration, latency);

                    // Save one sample output per format
                    if !sample_saved.swap(true, Ordering::SeqCst) && !output.is_empty() {
                        let out_dir = PathBuf::from(OUTPUT_DIR);
                        if let Err(e) = fs::create_dir_all(&out_dir) {
                            eprintln!("Failed to create output dir: {}", e);
                        } else {
                            let out_path = out_dir.join(format!("{}.s16le", format_for_save));
                            if let Ok(mut f) = fs::File::create(&out_path) {
                                let _ = f.write_all(&output);
                            }
                        }
                    }
                }
                Err(_) => {
                    stats.record_failure();
                }
            }

            drop(permit);
        });

        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.await;
    }

    let wallclock = start.elapsed();
    let successes = stats.successes.load(Ordering::Relaxed);
    let total_latency = stats.total_latency_ms.load(Ordering::Relaxed);
    let total_audio = *stats.total_audio_seconds.lock().unwrap();
    let output_bytes = stats.total_output_bytes.load(Ordering::Relaxed);
    let throughput_buckets = stats.throughput.get_throughput_buckets(THROUGHPUT_BUCKET_MS);

    FormatResult {
        format: format.to_string(),
        requests: stats.requests.load(Ordering::Relaxed),
        successes,
        failures: stats.failures.load(Ordering::Relaxed),
        total_audio_seconds: total_audio,
        wallclock_seconds: wallclock.as_secs_f64(),
        avg_latency_ms: if successes > 0 { total_latency as f64 / successes as f64 } else { 0.0 },
        min_latency_ms: stats.min_latency_ms.load(Ordering::Relaxed),
        max_latency_ms: stats.max_latency_ms.load(Ordering::Relaxed),
        output_mb: output_bytes as f64 / (1024.0 * 1024.0),
        throughput_buckets,
    }
}

// =============================================================================
// Throughput Graph
// =============================================================================

#[allow(dead_code)]
fn print_throughput_graph(results: &[FormatResult]) {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════════╗");
    println!("║                      THROUGHPUT OVER TIME (files/sec)                            ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════════╝");
    println!();

    // Find max time and max throughput across all formats
    let max_time_ms = results
        .iter()
        .flat_map(|r| r.throughput_buckets.iter().map(|(t, _)| *t))
        .max()
        .unwrap_or(0);

    let max_throughput = results
        .iter()
        .flat_map(|r| r.throughput_buckets.iter().map(|(_, v)| *v))
        .fold(0.0f64, |a, b| a.max(b));

    if max_throughput == 0.0 || max_time_ms == 0 {
        println!("  No throughput data available");
        return;
    }

    const GRAPH_HEIGHT: usize = 12;
    const GRAPH_WIDTH: usize = 60;

    // Print per-format graphs
    for result in results {
        if result.throughput_buckets.is_empty() {
            continue;
        }

        let format_max = result
            .throughput_buckets
            .iter()
            .map(|(_, v)| *v)
            .fold(0.0f64, |a, b| a.max(b));

        println!("  {:12} (peak: {:.1} files/sec, avg: {:.1} files/sec)",
                 result.format, format_max, result.files_per_second());

        // Create ASCII graph
        let time_scale = max_time_ms as f64 / GRAPH_WIDTH as f64;

        // Build graph rows
        for row in (0..GRAPH_HEIGHT).rev() {
            let threshold = (row as f64 / GRAPH_HEIGHT as f64) * format_max;

            print!("  {:5.0} │", threshold);

            for col in 0..GRAPH_WIDTH {
                let time_start = (col as f64 * time_scale) as u64;
                let time_end = ((col + 1) as f64 * time_scale) as u64;

                // Find max throughput in this time range
                let value = result
                    .throughput_buckets
                    .iter()
                    .filter(|(t, _)| *t >= time_start && *t < time_end)
                    .map(|(_, v)| *v)
                    .fold(0.0f64, |a, b| a.max(b));

                if value > threshold {
                    print!("█");
                } else if value > threshold - (format_max / GRAPH_HEIGHT as f64 / 2.0) {
                    print!("▄");
                } else {
                    print!(" ");
                }
            }
            println!();
        }

        // X-axis
        print!("        └");
        for _ in 0..GRAPH_WIDTH {
            print!("─");
        }
        println!();

        // Time labels
        print!("         0");
        let label_spacing = GRAPH_WIDTH / 4;
        for i in 1..=4 {
            let time_sec = (max_time_ms as f64 * i as f64 / 4.0) / 1000.0;
            let spaces = label_spacing - format!("{:.1}s", time_sec).len();
            print!("{:>width$}{:.1}s", "", time_sec, width = spaces);
        }
        println!();
        println!();
    }
}

// =============================================================================
// Ramp Summary
// =============================================================================

#[derive(Debug)]
struct RampResult {
    concurrency: usize,
    total_requests: usize,
    total_successes: usize,
    avg_latency_ms: f64,
    min_latency_ms: u64,
    max_latency_ms: u64,
    files_per_sec: f64,
    realtime_multiplier: f64,
}

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() {
    let num_cpus = num_cpus::get();
    let concurrency_levels = get_concurrency_levels();

    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════════╗");
    println!("║                    MEDIA-API RAMPED LOAD TEST                                    ║");
    println!("╠══════════════════════════════════════════════════════════════════════════════════╣");
    println!("║  CPUs: {:4}  |  Concurrency levels: {:?}              ║",
             num_cpus, concurrency_levels);
    println!("║  Max files/format: {:4}  |  Formats: mp3, opus, flac                            ║",
             MAX_FILES_PER_FORMAT);
    println!("╚══════════════════════════════════════════════════════════════════════════════════╝");
    println!();

    // Use subset of formats for faster ramp testing
    let formats: Vec<&str> = vec!["mp3", "opus", "flac"];

    // Clear and create output directory
    let output_dir = PathBuf::from(OUTPUT_DIR);
    if output_dir.exists() {
        let _ = fs::remove_dir_all(&output_dir);
    }
    fs::create_dir_all(&output_dir).expect("Failed to create output directory");

    // Load test data
    println!("Loading test data...");
    let mut format_files: HashMap<String, Vec<AudioFile>> = HashMap::new();
    for format in &formats {
        let files = load_format_files(format);
        if !files.is_empty() {
            let total_duration: f64 = files.iter().map(|f| f.estimated_duration).sum();
            println!("  {:12}: {:4} files, {:.1}s total audio", format, files.len(), total_duration);
            format_files.insert(format.to_string(), files);
        }
    }
    println!();

    // Start server
    let port = portpicker::pick_unused_port().expect("No free port");
    let (cert, key) = load_tls_certs();
    let server_config = ServerConfig {
        port,
        cert_pem_base64: cert,
        privkey_pem_base64: key,
        enable_h2: true,
        enable_h3: false,
        enable_webtransport: false,
        enable_websocket: false,
        enable_raw_tcp: false,
        raw_tcp_port: 0,
        raw_tcp_tls: false,
    };
    let media_config = MediaApiConfig {
        server: server_config.clone(),
        default_output_sample_rate: Some(16_000),
        default_output_bits: Some(16),
        default_output_channels: Some(1),
    };
    let router: Arc<dyn Router> = Arc::new(MediaRouter::new(media_config));

    println!("Starting server on port {}...", port);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    let server_handle = tokio::spawn(async move {
        let server = Http2Server::new(server_config, router);
        server.start(shutdown_rx).await
    });

    tokio::time::sleep(Duration::from_millis(200)).await;
    println!();

    // Run tests at each concurrency level
    let mut ramp_results: Vec<RampResult> = Vec::new();

    for &concurrency in &concurrency_levels {
        println!("╔══════════════════════════════════════════════════════════════════════════════════╗");
        println!("║  CONCURRENCY: {:3} ({:.1}x CPUs)                                                   ║",
                 concurrency, concurrency as f64 / num_cpus as f64);
        println!("╚══════════════════════════════════════════════════════════════════════════════════╝");

        let client = create_http_client(concurrency);
        let mut level_results = Vec::new();

        for format in &formats {
            if let Some(files) = format_files.get(*format) {
                print!("  {:12} ... ", format);
                std::io::stdout().flush().unwrap();

                let result = run_format_benchmark(&client, port, format, files, concurrency).await;

                println!(
                    "{:5.1} files/s | {:6.1}ms avg (min:{}, max:{}) | {}/{} ok",
                    result.files_per_second(),
                    result.avg_latency_ms,
                    result.min_latency_ms,
                    result.max_latency_ms,
                    result.successes,
                    result.requests
                );

                level_results.push(result);
            }
        }

        // Aggregate results for this concurrency level
        let total_requests: usize = level_results.iter().map(|r| r.requests).sum();
        let total_successes: usize = level_results.iter().map(|r| r.successes).sum();
        let total_latency: f64 = level_results.iter().map(|r| r.avg_latency_ms * r.successes as f64).sum();
        let avg_latency = if total_successes > 0 { total_latency / total_successes as f64 } else { 0.0 };
        let min_latency = level_results.iter().map(|r| r.min_latency_ms).min().unwrap_or(0);
        let max_latency = level_results.iter().map(|r| r.max_latency_ms).max().unwrap_or(0);
        let total_wallclock: f64 = level_results.iter().map(|r| r.wallclock_seconds).sum();
        let total_audio: f64 = level_results.iter().map(|r| r.total_audio_seconds).sum();
        let files_per_sec = total_successes as f64 / total_wallclock;
        let realtime = total_audio / total_wallclock;

        ramp_results.push(RampResult {
            concurrency,
            total_requests,
            total_successes,
            avg_latency_ms: avg_latency,
            min_latency_ms: min_latency,
            max_latency_ms: max_latency,
            files_per_sec,
            realtime_multiplier: realtime,
        });

        println!();
    }

    // Print ramp summary
    println!("╔══════════════════════════════════════════════════════════════════════════════════════════════════════╗");
    println!("║                                    CONCURRENCY RAMP SUMMARY                                         ║");
    println!("╠═════════════╦═══════════╦═══════════╦═══════════════════════════════╦═══════════════╦════════════════╣");
    println!("║ Concurrency ║  xCPUs    ║ Files/sec ║  Latency (min/avg/max) ms     ║   Realtime    ║  Success Rate  ║");
    println!("╠═════════════╬═══════════╬═══════════╬═══════════════════════════════╬═══════════════╬════════════════╣");

    for r in &ramp_results {
        let success_rate = r.total_successes as f64 / r.total_requests as f64 * 100.0;
        println!(
            "║     {:4}    ║   {:4.1}x   ║   {:5.1}   ║   {:5} / {:6.1} / {:5}      ║     {:5.1}x     ║     {:5.1}%      ║",
            r.concurrency,
            r.concurrency as f64 / num_cpus as f64,
            r.files_per_sec,
            r.min_latency_ms,
            r.avg_latency_ms,
            r.max_latency_ms,
            r.realtime_multiplier,
            success_rate
        );
    }

    println!("╚═════════════╩═══════════╩═══════════╩═══════════════════════════════╩═══════════════╩════════════════╝");

    // Latency vs Concurrency graph
    println!();
    println!("Latency vs Concurrency:");
    let max_latency_for_graph = ramp_results.iter().map(|r| r.avg_latency_ms).fold(0.0f64, |a, b| a.max(b));

    for r in &ramp_results {
        let bar_width = ((r.avg_latency_ms / max_latency_for_graph) * 50.0) as usize;
        let bar = "█".repeat(bar_width);
        println!(
            "  {:3} ({:.1}x): {:6.1}ms  {}",
            r.concurrency,
            r.concurrency as f64 / num_cpus as f64,
            r.avg_latency_ms,
            bar
        );
    }

    // Throughput vs Concurrency graph
    println!();
    println!("Throughput vs Concurrency:");
    let max_fps = ramp_results.iter().map(|r| r.files_per_sec).fold(0.0f64, |a, b| a.max(b));

    for r in &ramp_results {
        let bar_width = ((r.files_per_sec / max_fps) * 50.0) as usize;
        let bar = "█".repeat(bar_width);
        println!(
            "  {:3} ({:.1}x): {:5.1} f/s {}",
            r.concurrency,
            r.concurrency as f64 / num_cpus as f64,
            r.files_per_sec,
            bar
        );
    }

    // Cleanup
    let _ = shutdown_tx.send(());
    let _ = server_handle.await;
}
