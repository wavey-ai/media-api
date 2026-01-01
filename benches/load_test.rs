//! Load test benchmark for media-api decode service
//!
//! Measures throughput in audio seconds per wallclock second for each format.
//! Includes autoscaling notification system with rolling window metrics.
//!
//! Run with: cargo run --release --bin load_test

#![allow(dead_code)]

use bytes::Bytes;
use media_api::{MediaApiConfig, MediaRouter};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, Semaphore};
use web_service::h2::Http2Server;

// =============================================================================
// Configuration
// =============================================================================

const INITIAL_CONCURRENCY: usize = 100;
const MAX_CONCURRENCY: usize = 200;
const CONCURRENCY_RAMP_STEP: usize = 100;
const CONCURRENCY_RAMP_INTERVAL_MS: u64 = 250;
const MAX_FILES_PER_FORMAT: usize = 100;
const WARMUP_REQUESTS: usize = 5;
const ROLLING_WINDOW_SECS: u64 = 10;

// Autoscaling thresholds
const LATENCY_WARNING_MS: f64 = 500.0;
const LATENCY_CRITICAL_MS: f64 = 1000.0;
const LATENCY_EMERGENCY_MS: f64 = 2000.0;
const THROUGHPUT_DROP_WARNING_PCT: f64 = 20.0;
const THROUGHPUT_DROP_CRITICAL_PCT: f64 = 40.0;
const THROUGHPUT_DROP_EMERGENCY_PCT: f64 = 60.0;
const MAX_EMERGENCY_WARNINGS_BEFORE_ABORT: usize = 3;

// =============================================================================
// Autoscaling Notification Types
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Urgency {
    Info,
    Warning,
    Critical,
    Emergency,
}

impl std::fmt::Display for Urgency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Urgency::Info => write!(f, "INFO"),
            Urgency::Warning => write!(f, "WARNING"),
            Urgency::Critical => write!(f, "CRITICAL"),
            Urgency::Emergency => write!(f, "EMERGENCY"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutoscaleNotification {
    pub timestamp: Instant,
    pub urgency: Urgency,
    pub format: String,
    pub reason: String,
    pub current_concurrency: usize,
    pub rolling_avg_latency_ms: f64,
    pub rolling_throughput_audio_per_sec: f64,
    pub baseline_throughput: f64,
    pub throughput_drop_pct: f64,
    pub active_requests: usize,
    pub success_rate: f64,
}

impl AutoscaleNotification {
    pub fn summary(&self) -> String {
        format!(
            "[{}] {} - {} | concurrency={} latency={:.0}ms throughput={:.1}x (drop {:.1}%) success={:.1}%",
            self.urgency,
            self.format,
            self.reason,
            self.current_concurrency,
            self.rolling_avg_latency_ms,
            self.rolling_throughput_audio_per_sec,
            self.throughput_drop_pct,
            self.success_rate * 100.0
        )
    }
}

// =============================================================================
// Rolling Window Metrics
// =============================================================================

#[derive(Debug, Clone)]
struct MetricSample {
    timestamp: Instant,
    latency_ms: f64,
    audio_seconds: f64,
    success: bool,
}

struct RollingMetrics {
    format: String,
    samples: VecDeque<MetricSample>,
    window_duration: Duration,
    baseline_throughput: Option<f64>,
    peak_throughput: f64,
}

impl RollingMetrics {
    fn new(format: &str, window_secs: u64) -> Self {
        Self {
            format: format.to_string(),
            samples: VecDeque::new(),
            window_duration: Duration::from_secs(window_secs),
            baseline_throughput: None,
            peak_throughput: 0.0,
        }
    }

    fn add_sample(&mut self, latency_ms: f64, audio_seconds: f64, success: bool) {
        let now = Instant::now();
        self.samples.push_back(MetricSample {
            timestamp: now,
            latency_ms,
            audio_seconds,
            success,
        });
        self.prune_old_samples(now);

        // Update peak throughput for baseline comparison
        let current = self.throughput_audio_per_sec();
        if current > self.peak_throughput {
            self.peak_throughput = current;
            if self.baseline_throughput.is_none() && self.samples.len() > 10 {
                self.baseline_throughput = Some(current);
            }
        }
    }

    fn prune_old_samples(&mut self, now: Instant) {
        while let Some(front) = self.samples.front() {
            if now.duration_since(front.timestamp) > self.window_duration {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    fn avg_latency_ms(&self) -> f64 {
        let successful: Vec<_> = self.samples.iter().filter(|s| s.success).collect();
        if successful.is_empty() {
            return 0.0;
        }
        successful.iter().map(|s| s.latency_ms).sum::<f64>() / successful.len() as f64
    }

    fn throughput_audio_per_sec(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let oldest = self.samples.front().unwrap().timestamp;
        let newest = self.samples.back().unwrap().timestamp;
        let duration = newest.duration_since(oldest).as_secs_f64();
        if duration < 0.1 {
            return 0.0;
        }
        let total_audio: f64 = self.samples.iter().filter(|s| s.success).map(|s| s.audio_seconds).sum();
        total_audio / duration
    }

    fn success_rate(&self) -> f64 {
        if self.samples.is_empty() {
            return 1.0;
        }
        let successes = self.samples.iter().filter(|s| s.success).count();
        successes as f64 / self.samples.len() as f64
    }

    fn throughput_drop_pct(&self) -> f64 {
        let baseline = self.baseline_throughput.unwrap_or(self.peak_throughput);
        if baseline <= 0.0 {
            return 0.0;
        }
        let current = self.throughput_audio_per_sec();
        ((baseline - current) / baseline * 100.0).max(0.0)
    }

    fn sample_count(&self) -> usize {
        self.samples.len()
    }
}

// =============================================================================
// Autoscaling Monitor
// =============================================================================

struct AutoscaleMonitor {
    metrics: HashMap<String, RollingMetrics>,
    notification_tx: mpsc::UnboundedSender<AutoscaleNotification>,
    current_concurrency: Arc<AtomicUsize>,
    active_requests: Arc<AtomicUsize>,
    last_notification: HashMap<String, (Instant, Urgency)>,
    notification_cooldown: Duration,
}

impl AutoscaleMonitor {
    fn new(
        formats: &[&str],
        notification_tx: mpsc::UnboundedSender<AutoscaleNotification>,
        current_concurrency: Arc<AtomicUsize>,
        active_requests: Arc<AtomicUsize>,
    ) -> Self {
        let mut metrics = HashMap::new();
        for format in formats {
            metrics.insert(format.to_string(), RollingMetrics::new(format, ROLLING_WINDOW_SECS));
        }
        Self {
            metrics,
            notification_tx,
            current_concurrency,
            active_requests,
            last_notification: HashMap::new(),
            notification_cooldown: Duration::from_secs(2),
        }
    }

    fn record_sample(&mut self, format: &str, latency_ms: f64, audio_seconds: f64, success: bool) {
        if let Some(metrics) = self.metrics.get_mut(format) {
            metrics.add_sample(latency_ms, audio_seconds, success);
            self.check_autoscale_triggers(format);
        }
    }

    fn check_autoscale_triggers(&mut self, format: &str) {
        let metrics = match self.metrics.get(format) {
            Some(m) => m,
            None => return,
        };

        // Need enough samples to make a decision
        if metrics.sample_count() < 5 {
            return;
        }

        let avg_latency = metrics.avg_latency_ms();
        let throughput = metrics.throughput_audio_per_sec();
        let throughput_drop = metrics.throughput_drop_pct();
        let success_rate = metrics.success_rate();
        let concurrency = self.current_concurrency.load(Ordering::Relaxed);
        let active = self.active_requests.load(Ordering::Relaxed);

        // Determine urgency and reason
        let (urgency, reason) = self.evaluate_urgency(avg_latency, throughput_drop, success_rate);

        // Check cooldown
        if let Some((last_time, last_urgency)) = self.last_notification.get(format) {
            if last_time.elapsed() < self.notification_cooldown && urgency <= *last_urgency {
                return;
            }
        }

        // Only send notifications for Warning or higher
        if urgency >= Urgency::Warning {
            let notification = AutoscaleNotification {
                timestamp: Instant::now(),
                urgency,
                format: format.to_string(),
                reason,
                current_concurrency: concurrency,
                rolling_avg_latency_ms: avg_latency,
                rolling_throughput_audio_per_sec: throughput,
                baseline_throughput: metrics.baseline_throughput.unwrap_or(metrics.peak_throughput),
                throughput_drop_pct: throughput_drop,
                active_requests: active,
                success_rate,
            };

            self.last_notification.insert(format.to_string(), (Instant::now(), urgency));
            let _ = self.notification_tx.send(notification);
        }
    }

    fn evaluate_urgency(&self, avg_latency: f64, throughput_drop: f64, success_rate: f64) -> (Urgency, String) {
        // Emergency conditions
        if avg_latency > LATENCY_EMERGENCY_MS {
            return (Urgency::Emergency, format!("Latency spike: {:.0}ms > {:.0}ms threshold", avg_latency, LATENCY_EMERGENCY_MS));
        }
        if throughput_drop > THROUGHPUT_DROP_EMERGENCY_PCT {
            return (Urgency::Emergency, format!("Throughput collapse: {:.1}% drop", throughput_drop));
        }
        if success_rate < 0.5 {
            return (Urgency::Emergency, format!("High failure rate: {:.1}%", (1.0 - success_rate) * 100.0));
        }

        // Critical conditions
        if avg_latency > LATENCY_CRITICAL_MS {
            return (Urgency::Critical, format!("High latency: {:.0}ms > {:.0}ms threshold", avg_latency, LATENCY_CRITICAL_MS));
        }
        if throughput_drop > THROUGHPUT_DROP_CRITICAL_PCT {
            return (Urgency::Critical, format!("Significant throughput drop: {:.1}%", throughput_drop));
        }
        if success_rate < 0.8 {
            return (Urgency::Critical, format!("Elevated failure rate: {:.1}%", (1.0 - success_rate) * 100.0));
        }

        // Warning conditions
        if avg_latency > LATENCY_WARNING_MS {
            return (Urgency::Warning, format!("Elevated latency: {:.0}ms > {:.0}ms threshold", avg_latency, LATENCY_WARNING_MS));
        }
        if throughput_drop > THROUGHPUT_DROP_WARNING_PCT {
            return (Urgency::Warning, format!("Throughput degradation: {:.1}%", throughput_drop));
        }
        if success_rate < 0.95 {
            return (Urgency::Warning, format!("Some failures: {:.1}%", (1.0 - success_rate) * 100.0));
        }

        (Urgency::Info, "Normal operation".to_string())
    }

    fn get_format_summary(&self, format: &str) -> Option<(f64, f64, f64)> {
        self.metrics.get(format).map(|m| {
            (m.avg_latency_ms(), m.throughput_audio_per_sec(), m.success_rate())
        })
    }
}

// =============================================================================
// Test Data Loading
// =============================================================================

fn estimate_audio_duration(format: &str, file_size: usize) -> f64 {
    let bitrate_kbps = match format {
        "mp3" => 48.0,
        "flac" => 400.0,
        "opus" | "ogg_opus" => 32.0,
        "mac_aac" => 64.0,
        "wav_24" | "wav_32f" | "wav_stereo" => 768.0,
        "linear16" => 256.0,
        "linear16_48" => 768.0,
        "linear32" => 512.0,
        "linear32_48" => 1536.0,
        "g729" => 8.0,
        "es" => 48.0,
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
    let base_path = benchmark_testdata_path().join(format);
    if !base_path.exists() {
        return Vec::new();
    }

    let mut files = Vec::new();
    let entries: Vec<_> = fs::read_dir(&base_path)
        .ok()
        .map(|rd| rd.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();

    for entry in entries.into_iter().take(MAX_FILES_PER_FORMAT) {
        let path = entry.path();
        if path.is_file() {
            if let Ok(data) = fs::read(&path) {
                let duration = estimate_audio_duration(format, data.len());
                files.push(AudioFile {
                    name: path.file_name().unwrap().to_string_lossy().to_string(),
                    data: Bytes::from(data),
                    estimated_duration: duration,
                });
            }
        }
    }

    files
}

// =============================================================================
// Statistics
// =============================================================================

#[derive(Default)]
struct FormatStats {
    requests: AtomicUsize,
    successes: AtomicUsize,
    failures: AtomicUsize,
    total_input_bytes: AtomicU64,
    total_output_bytes: AtomicU64,
    total_audio_seconds: AtomicU64,
    total_latency_us: AtomicU64,
    min_latency_us: AtomicU64,
    max_latency_us: AtomicU64,
    max_error_free_concurrency: AtomicUsize,
    first_error_concurrency: AtomicUsize,
}

impl FormatStats {
    fn new() -> Self {
        Self {
            min_latency_us: AtomicU64::new(u64::MAX),
            first_error_concurrency: AtomicUsize::new(usize::MAX),
            ..Default::default()
        }
    }

    fn record_success(&self, input_bytes: usize, output_bytes: usize, audio_seconds: f64, latency: Duration, current_concurrency: usize) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.successes.fetch_add(1, Ordering::Relaxed);
        self.total_input_bytes.fetch_add(input_bytes as u64, Ordering::Relaxed);
        self.total_output_bytes.fetch_add(output_bytes as u64, Ordering::Relaxed);
        self.total_audio_seconds.fetch_add((audio_seconds * 1_000_000.0) as u64, Ordering::Relaxed);

        let latency_us = latency.as_micros() as u64;
        self.total_latency_us.fetch_add(latency_us, Ordering::Relaxed);
        self.min_latency_us.fetch_min(latency_us, Ordering::Relaxed);
        self.max_latency_us.fetch_max(latency_us, Ordering::Relaxed);

        // Update max error-free concurrency if no errors have occurred yet
        if self.first_error_concurrency.load(Ordering::Relaxed) == usize::MAX {
            self.max_error_free_concurrency.fetch_max(current_concurrency, Ordering::Relaxed);
        }
    }

    fn record_failure(&self, current_concurrency: usize) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.failures.fetch_add(1, Ordering::Relaxed);
        // Record the concurrency level of first error
        self.first_error_concurrency.fetch_min(current_concurrency, Ordering::Relaxed);
    }

    fn max_error_free_concurrency(&self) -> usize {
        let first_error = self.first_error_concurrency.load(Ordering::Relaxed);
        if first_error == usize::MAX {
            // No errors occurred, return the max concurrency we tracked
            self.max_error_free_concurrency.load(Ordering::Relaxed)
        } else {
            // Errors occurred, max error-free is one less than first error concurrency
            // or the max we tracked before errors, whichever is smaller
            let max_tracked = self.max_error_free_concurrency.load(Ordering::Relaxed);
            max_tracked.min(first_error.saturating_sub(1))
        }
    }
}

#[derive(Debug)]
struct FormatResult {
    format: String,
    requests: usize,
    successes: usize,
    failures: usize,
    total_audio_seconds: f64,
    wallclock_seconds: f64,
    audio_seconds_per_second: f64,
    avg_latency_ms: f64,
    min_latency_ms: f64,
    max_latency_ms: f64,
    input_mb: f64,
    output_mb: f64,
    throughput_mbps: f64,
    max_error_free_concurrency: usize,
}

// =============================================================================
// HTTP Client
// =============================================================================

async fn decode_request(
    client: &reqwest::Client,
    port: u16,
    data: Bytes,
) -> Result<Vec<u8>, String> {
    let response = client
        .post(format!("https://local.wavey.ai:{}/decode", port))
        .body(data.to_vec())
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let body = response
        .bytes()
        .await
        .map_err(|e| format!("Body read failed: {}", e))?;

    Ok(body.to_vec())
}

// =============================================================================
// Server Setup
// =============================================================================

fn load_tls_certs() -> (String, String) {
    use base64::{Engine, engine::general_purpose::STANDARD};

    let certs_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("certs");
    let cert_path = certs_dir.join("fullchain.pem");
    let key_path = certs_dir.join("privkey.pem");

    let cert = fs::read_to_string(&cert_path)
        .map(|s| STANDARD.encode(s))
        .unwrap_or_default();
    let key = fs::read_to_string(&key_path)
        .map(|s| STANDARD.encode(s))
        .unwrap_or_default();

    (cert, key)
}

async fn start_test_server() -> (u16, tokio::sync::watch::Sender<()>) {
    let port = portpicker::pick_unused_port().expect("No ports available");
    let (cert, key) = load_tls_certs();

    let config = MediaApiConfig {
        server: web_service::ServerConfig {
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

    tokio::time::sleep(Duration::from_millis(300)).await;
    (port, shutdown_tx)
}

// =============================================================================
// Load Test Runner with Autoscaling
// =============================================================================

async fn run_format_benchmark_with_autoscale(
    client: &reqwest::Client,
    port: u16,
    format: &str,
    files: &[AudioFile],
    monitor: Arc<Mutex<AutoscaleMonitor>>,
    current_concurrency: Arc<AtomicUsize>,
    active_requests: Arc<AtomicUsize>,
    abort_flag: Arc<AtomicBool>,
) -> FormatResult {
    let stats = Arc::new(FormatStats::new());

    // Warmup with initial concurrency
    let warmup_sem = Arc::new(Semaphore::new(INITIAL_CONCURRENCY));
    for file in files.iter().take(WARMUP_REQUESTS) {
        let _permit = warmup_sem.acquire().await.unwrap();
        let _ = decode_request(client, port, file.data.clone()).await;
    }

    // Reset concurrency and active requests for actual test
    current_concurrency.store(INITIAL_CONCURRENCY, Ordering::SeqCst);
    active_requests.store(0, Ordering::SeqCst);
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENCY));

    // Start with initial permits
    let available_permits = Arc::new(AtomicUsize::new(INITIAL_CONCURRENCY));

    let start = Instant::now();
    let mut handles = Vec::new();

    // Spawn concurrency ramp-up task
    let ramp_concurrency = current_concurrency.clone();
    let ramp_available = available_permits.clone();
    let ramp_handle = tokio::spawn(async move {
        let mut current = INITIAL_CONCURRENCY;
        loop {
            tokio::time::sleep(Duration::from_millis(CONCURRENCY_RAMP_INTERVAL_MS)).await;
            if current >= MAX_CONCURRENCY {
                break;
            }
            current = (current + CONCURRENCY_RAMP_STEP).min(MAX_CONCURRENCY);
            ramp_concurrency.store(current, Ordering::SeqCst);
            ramp_available.store(current, Ordering::SeqCst);
        }
    });

    for file in files.iter() {
        // Check if we should abort
        if abort_flag.load(Ordering::SeqCst) {
            break;
        }

        // Wait for permit based on current allowed concurrency
        loop {
            let allowed = available_permits.load(Ordering::SeqCst);
            let active = active_requests.load(Ordering::SeqCst);
            if active < allowed {
                break;
            }
            if abort_flag.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        if abort_flag.load(Ordering::SeqCst) {
            break;
        }

        let permit = semaphore.clone().acquire_owned().await.unwrap();
        active_requests.fetch_add(1, Ordering::SeqCst);

        let client = client.clone();
        let stats = stats.clone();
        let file = file.clone();
        let monitor = monitor.clone();
        let format = format.to_string();
        let active_req = active_requests.clone();
        let concurrency_tracker = current_concurrency.clone();

        let handle = tokio::spawn(async move {
            let req_start = Instant::now();
            let input_bytes = file.data.len();
            // Capture concurrency at request start
            let req_concurrency = concurrency_tracker.load(Ordering::SeqCst);

            let result = decode_request(&client, port, file.data).await;
            let latency = req_start.elapsed();
            let latency_ms = latency.as_secs_f64() * 1000.0;

            match result {
                Ok(output) => {
                    stats.record_success(input_bytes, output.len(), file.estimated_duration, latency, req_concurrency);
                    let mut mon = monitor.lock().await;
                    mon.record_sample(&format, latency_ms, file.estimated_duration, true);
                }
                Err(_) => {
                    stats.record_failure(req_concurrency);
                    let mut mon = monitor.lock().await;
                    mon.record_sample(&format, latency_ms, 0.0, false);
                }
            }

            active_req.fetch_sub(1, Ordering::SeqCst);
            drop(permit);
        });

        handles.push(handle);
    }

    // If aborted, cancel all in-flight requests; otherwise wait for completion
    if abort_flag.load(Ordering::SeqCst) {
        for handle in handles {
            handle.abort();
        }
    } else {
        for handle in handles {
            let _ = handle.await;
        }
    }

    ramp_handle.abort();
    // Wait for active requests to complete or timeout
    let drain_start = Instant::now();
    while active_requests.load(Ordering::SeqCst) > 0 && drain_start.elapsed() < Duration::from_secs(2) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let wallclock = start.elapsed();
    let requests = stats.requests.load(Ordering::Relaxed);
    let successes = stats.successes.load(Ordering::Relaxed);
    let failures = stats.failures.load(Ordering::Relaxed);
    let total_audio_us = stats.total_audio_seconds.load(Ordering::Relaxed);
    let total_audio_seconds = total_audio_us as f64 / 1_000_000.0;
    let wallclock_seconds = wallclock.as_secs_f64();
    let total_latency_us = stats.total_latency_us.load(Ordering::Relaxed);
    let min_latency_us = stats.min_latency_us.load(Ordering::Relaxed);
    let max_latency_us = stats.max_latency_us.load(Ordering::Relaxed);
    let input_bytes = stats.total_input_bytes.load(Ordering::Relaxed);
    let output_bytes = stats.total_output_bytes.load(Ordering::Relaxed);

    FormatResult {
        format: format.to_string(),
        requests,
        successes,
        failures,
        total_audio_seconds,
        wallclock_seconds,
        audio_seconds_per_second: if wallclock_seconds > 0.0 {
            total_audio_seconds / wallclock_seconds
        } else {
            0.0
        },
        avg_latency_ms: if successes > 0 {
            (total_latency_us as f64 / successes as f64) / 1000.0
        } else {
            0.0
        },
        min_latency_ms: if min_latency_us < u64::MAX {
            min_latency_us as f64 / 1000.0
        } else {
            0.0
        },
        max_latency_ms: max_latency_us as f64 / 1000.0,
        input_mb: input_bytes as f64 / (1024.0 * 1024.0),
        output_mb: output_bytes as f64 / (1024.0 * 1024.0),
        throughput_mbps: if wallclock_seconds > 0.0 {
            (output_bytes as f64 / (1024.0 * 1024.0)) / wallclock_seconds
        } else {
            0.0
        },
        max_error_free_concurrency: stats.max_error_free_concurrency(),
    }
}

// =============================================================================
// Main
// =============================================================================

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════════╗");
    println!("║                    MEDIA-API LOAD TEST WITH AUTOSCALING                          ║");
    println!("╠══════════════════════════════════════════════════════════════════════════════════╣");
    println!("║  Initial concurrency: {:4}  |  Max concurrency: {:4}  |  Ramp step: {:4}        ║",
             INITIAL_CONCURRENCY, MAX_CONCURRENCY, CONCURRENCY_RAMP_STEP);
    println!("║  Rolling window: {:2}s         |  Max files/format: {:4}  |  Warmup: {:4}         ║",
             ROLLING_WINDOW_SECS, MAX_FILES_PER_FORMAT, WARMUP_REQUESTS);
    println!("╠══════════════════════════════════════════════════════════════════════════════════╣");
    println!("║  Autoscale Thresholds:                                                           ║");
    println!("║    Latency    - Warning: {:4.0}ms  Critical: {:5.0}ms  Emergency: {:5.0}ms        ║",
             LATENCY_WARNING_MS, LATENCY_CRITICAL_MS, LATENCY_EMERGENCY_MS);
    println!("║    Throughput - Warning: {:4.0}%   Critical: {:5.0}%   Emergency: {:5.0}%          ║",
             THROUGHPUT_DROP_WARNING_PCT, THROUGHPUT_DROP_CRITICAL_PCT, THROUGHPUT_DROP_EMERGENCY_PCT);
    println!("╚══════════════════════════════════════════════════════════════════════════════════╝");
    println!();

    // Only test formats that are working (FLAC has known issues in concurrent streaming)
    let formats: Vec<&str> = vec![
        "mp3",
        "opus",
        "ogg_opus",
        "mac_aac",
    ];

    // Load test data
    println!("Loading test data...");
    let mut format_files: HashMap<String, Vec<AudioFile>> = HashMap::new();
    for format in &formats {
        let files = load_format_files(format);
        if !files.is_empty() {
            let total_duration: f64 = files.iter().map(|f| f.estimated_duration).sum();
            println!(
                "  {:12} : {:4} files, {:.1}s total audio",
                format,
                files.len(),
                total_duration
            );
            format_files.insert(format.to_string(), files);
        }
    }
    println!();

    if format_files.is_empty() {
        eprintln!("ERROR: No test data found in benchmark_testdata/");
        eprintln!("       Please copy test data files to benchmark_testdata/");
        std::process::exit(1);
    }

    // Start server
    println!("Starting test server...");
    let (port, shutdown) = start_test_server().await;
    println!("  Server running on port {}", port);
    println!();

    // Build HTTP client with timeouts
    use std::net::SocketAddr;
    let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let client = reqwest::Client::builder()
        .resolve("local.wavey.ai", addr)
        .http2_prior_knowledge() // Use HTTP/2 for max concurrent streams per connection
        .pool_max_idle_per_host(MAX_CONCURRENCY)
        .pool_idle_timeout(Duration::from_secs(60))
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(5))
        .danger_accept_invalid_certs(true)
        .build()
        .expect("Failed to build client");

    // Set up autoscaling notification channel
    let (notification_tx, mut notification_rx) = mpsc::unbounded_channel::<AutoscaleNotification>();
    let all_notifications: Arc<Mutex<Vec<AutoscaleNotification>>> = Arc::new(Mutex::new(Vec::new()));

    // Track emergency count per format for early abort
    let emergency_counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let current_format_abort: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // Spawn notification collector
    let notifications_clone = all_notifications.clone();
    let emergency_counts_clone = emergency_counts.clone();
    let abort_flag_clone = current_format_abort.clone();
    let notification_handler = tokio::spawn(async move {
        while let Some(notification) = notification_rx.recv().await {
            println!("  🚨 AUTOSCALE: {}", notification.summary());

            // Track emergency count and trigger abort if threshold exceeded
            if notification.urgency == Urgency::Emergency {
                let mut counts = emergency_counts_clone.lock().await;
                let count = counts.entry(notification.format.clone()).or_insert(0);
                *count += 1;
                if *count >= MAX_EMERGENCY_WARNINGS_BEFORE_ABORT {
                    abort_flag_clone.store(true, Ordering::SeqCst);
                }
            }

            notifications_clone.lock().await.push(notification);
        }
    });

    // Shared state
    let current_concurrency = Arc::new(AtomicUsize::new(INITIAL_CONCURRENCY));
    let active_requests = Arc::new(AtomicUsize::new(0));

    // Create monitor
    let format_refs: Vec<&str> = formats.iter().copied().collect();
    let monitor = Arc::new(Mutex::new(AutoscaleMonitor::new(
        &format_refs,
        notification_tx.clone(),
        current_concurrency.clone(),
        active_requests.clone(),
    )));

    // Run benchmarks
    let mut results: Vec<FormatResult> = Vec::new();

    println!("Running load tests with concurrency ramp-up...");
    println!();

    for format in &formats {
        if let Some(files) = format_files.get(*format) {
            print!("Testing {:12}... ", format);
            std::io::Write::flush(&mut std::io::stdout()).ok();

            // Reset concurrency and abort flag for each format
            current_concurrency.store(INITIAL_CONCURRENCY, Ordering::SeqCst);
            current_format_abort.store(false, Ordering::SeqCst);

            let result = run_format_benchmark_with_autoscale(
                &client,
                port,
                format,
                files,
                monitor.clone(),
                current_concurrency.clone(),
                active_requests.clone(),
                current_format_abort.clone(),
            )
            .await;

            let final_concurrency = current_concurrency.load(Ordering::SeqCst);
            let was_aborted = current_format_abort.load(Ordering::SeqCst);
            let abort_indicator = if was_aborted { " [ABORTED]" } else { "" };
            println!(
                "{:6.1}x realtime | {:5.1}ms avg | {}/{} ok | max_conc={} | max_err_free={}{}",
                result.audio_seconds_per_second,
                result.avg_latency_ms,
                result.successes,
                result.requests,
                final_concurrency,
                result.max_error_free_concurrency,
                abort_indicator
            );
            results.push(result);
        }
    }

    // Close notification channel and wait for handler
    // Must drop monitor first since it holds a clone of notification_tx
    drop(monitor);
    drop(notification_tx);
    let _ = notification_handler.await;

    let _ = shutdown.send(());

    // Print detailed results
    println!();
    println!("╔═══════════════════════════════════════════════════════════════════════════════════════════════════════════════════════════╗");
    println!("║                                              DETAILED RESULTS                                                             ║");
    println!("╠══════════════╦═══════════╦═══════════╦═══════════╦═══════════════════════════╦════════════════════╦══════════════════════╣");
    println!("║    Format    ║  Audio/s  ║  Success  ║  Failure  ║  Latency (min/avg/max)    ║  Throughput (MB/s) ║  Max Err-Free Conc   ║");
    println!("╠══════════════╬═══════════╬═══════════╬═══════════╬═══════════════════════════╬════════════════════╬══════════════════════╣");

    for r in &results {
        println!(
            "║ {:12} ║ {:7.1}x  ║ {:7}   ║ {:7}   ║ {:5.0}/{:6.1}/{:6.0} ms   ║ {:8.2} MB/s       ║ {:>20} ║",
            r.format,
            r.audio_seconds_per_second,
            r.successes,
            r.failures,
            r.min_latency_ms,
            r.avg_latency_ms,
            r.max_latency_ms,
            r.throughput_mbps,
            r.max_error_free_concurrency,
        );
    }

    println!("╚══════════════╩═══════════╩═══════════╩═══════════╩═══════════════════════════╩════════════════════╩══════════════════════╝");

    // Summary
    let total_audio: f64 = results.iter().map(|r| r.total_audio_seconds).sum();
    let total_wallclock: f64 = results.iter().map(|r| r.wallclock_seconds).sum();
    let total_successes: usize = results.iter().map(|r| r.successes).sum();
    let total_failures: usize = results.iter().map(|r| r.failures).sum();
    let total_requests: usize = results.iter().map(|r| r.requests).sum();

    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════════╗");
    println!("║                                    SUMMARY                                       ║");
    println!("╠══════════════════════════════════════════════════════════════════════════════════╣");
    println!("║  Total requests:    {:6}                                                       ║", total_requests);
    println!("║  Successful:        {:6} ({:.1}%)                                                ║",
             total_successes,
             if total_requests > 0 { 100.0 * total_successes as f64 / total_requests as f64 } else { 0.0 });
    println!("║  Failed:            {:6}                                                       ║", total_failures);
    println!("║  Total audio:       {:6.1}s                                                      ║", total_audio);
    println!("║  Total wallclock:   {:6.1}s                                                      ║", total_wallclock);
    println!("║  Overall realtime:  {:6.1}x                                                      ║",
             if total_wallclock > 0.0 { total_audio / total_wallclock } else { 0.0 });
    println!("╚══════════════════════════════════════════════════════════════════════════════════╝");
    println!();

    // Autoscaling Notification Report
    let notifications = all_notifications.lock().await;
    println!("╔══════════════════════════════════════════════════════════════════════════════════╗");
    println!("║                          AUTOSCALING NOTIFICATIONS                               ║");
    println!("╠══════════════════════════════════════════════════════════════════════════════════╣");

    if notifications.is_empty() {
        println!("║  No autoscaling notifications triggered during test.                            ║");
    } else {
        // Group by urgency
        let mut by_urgency: HashMap<Urgency, Vec<&AutoscaleNotification>> = HashMap::new();
        for n in notifications.iter() {
            by_urgency.entry(n.urgency).or_default().push(n);
        }

        let urgency_order = [Urgency::Emergency, Urgency::Critical, Urgency::Warning, Urgency::Info];
        for urgency in urgency_order {
            if let Some(notifs) = by_urgency.get(&urgency) {
                println!("║                                                                                  ║");
                println!("║  {:?} ({} notifications):                                                        ║", urgency, notifs.len());
                for n in notifs.iter().take(5) {
                    let line = format!(
                        "    {} @ conc={}: {}",
                        n.format, n.current_concurrency, n.reason
                    );
                    println!("║  {:76} ║", line);
                }
                if notifs.len() > 5 {
                    println!("║    ... and {} more                                                              ║", notifs.len() - 5);
                }
            }
        }

        println!("║                                                                                  ║");
        println!("╠══════════════════════════════════════════════════════════════════════════════════╣");
        println!("║  Notification Summary:                                                           ║");

        let emergency_count = by_urgency.get(&Urgency::Emergency).map(|v| v.len()).unwrap_or(0);
        let critical_count = by_urgency.get(&Urgency::Critical).map(|v| v.len()).unwrap_or(0);
        let warning_count = by_urgency.get(&Urgency::Warning).map(|v| v.len()).unwrap_or(0);

        println!("║    Emergency:  {:4}  |  Critical:  {:4}  |  Warning:  {:4}                       ║",
                 emergency_count, critical_count, warning_count);

        // Find first trigger per format
        println!("║                                                                                  ║");
        println!("║  First trigger per format:                                                       ║");
        let mut first_by_format: HashMap<&str, &AutoscaleNotification> = HashMap::new();
        for n in notifications.iter() {
            first_by_format.entry(&n.format).or_insert(n);
        }
        for (format, n) in first_by_format.iter() {
            let line = format!(
                "    {:12} @ concurrency {:3}: {:?}",
                format, n.current_concurrency, n.urgency
            );
            println!("║  {:76} ║", line);
        }
    }

    println!("╚══════════════════════════════════════════════════════════════════════════════════╝");
    println!();

    // Performance ranking
    let mut ranked: Vec<_> = results.iter().filter(|r| r.successes > 0).collect();
    ranked.sort_by(|a, b| b.audio_seconds_per_second.partial_cmp(&a.audio_seconds_per_second).unwrap());

    println!("Performance Ranking (by realtime multiplier):");
    for (i, r) in ranked.iter().enumerate() {
        let bar_len = (r.audio_seconds_per_second * 2.0).min(40.0) as usize;
        let bar: String = "█".repeat(bar_len);
        println!(
            "  {:2}. {:12} {:6.1}x  {}",
            i + 1,
            r.format,
            r.audio_seconds_per_second,
            bar
        );
    }
    println!();
}
