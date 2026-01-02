use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Rolling window sample
struct Sample {
    timestamp: Instant,
    value: usize,
}

/// Connection statistics tracker
pub struct Stats {
    /// Current number of active connections
    active_connections: AtomicUsize,
    /// Samples for rolling averages (protected by mutex)
    samples: Mutex<VecDeque<Sample>>,
    /// Number of CPUs
    num_cpus: usize,
    /// Maximum samples to keep (covers longest window + buffer)
    max_samples: usize,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            active_connections: AtomicUsize::new(0),
            samples: Mutex::new(VecDeque::with_capacity(6000)), // ~5min at 20 samples/sec
            num_cpus: num_cpus::get(),
            max_samples: 6000,
        }
    }

    /// Increment active connections and record sample
    pub fn connection_start(&self) {
        let count = self.active_connections.fetch_add(1, Ordering::SeqCst) + 1;
        self.record_sample(count);
    }

    /// Decrement active connections and record sample
    pub fn connection_end(&self) {
        let count = self.active_connections.fetch_sub(1, Ordering::SeqCst) - 1;
        self.record_sample(count);
    }

    /// Record a sample of current connection count
    fn record_sample(&self, value: usize) {
        if let Ok(mut samples) = self.samples.lock() {
            samples.push_back(Sample {
                timestamp: Instant::now(),
                value,
            });
            // Trim old samples
            while samples.len() > self.max_samples {
                samples.pop_front();
            }
        }
    }

    /// Get current active connection count
    pub fn current(&self) -> usize {
        self.active_connections.load(Ordering::SeqCst)
    }

    /// Get number of CPUs
    pub fn num_cpus(&self) -> usize {
        self.num_cpus
    }

    /// Calculate average connections over a time window
    pub fn average(&self, window: Duration) -> f64 {
        let now = Instant::now();
        let cutoff = now - window;

        if let Ok(samples) = self.samples.lock() {
            let relevant: Vec<_> = samples
                .iter()
                .filter(|s| s.timestamp >= cutoff)
                .collect();

            if relevant.is_empty() {
                return self.current() as f64;
            }

            let sum: usize = relevant.iter().map(|s| s.value).sum();
            sum as f64 / relevant.len() as f64
        } else {
            self.current() as f64
        }
    }

    /// Get stats as JSON-compatible structure
    pub fn to_json(&self) -> serde_json::Value {
        let util_1s = self.average(Duration::from_secs(1)) / self.num_cpus as f64;
        let util_10s = self.average(Duration::from_secs(10)) / self.num_cpus as f64;
        let util_60s = self.average(Duration::from_secs(60)) / self.num_cpus as f64;
        let util_120s = self.average(Duration::from_secs(120)) / self.num_cpus as f64;
        let util_300s = self.average(Duration::from_secs(300)) / self.num_cpus as f64;

        // Alarm if any rolling average >= 10s exceeds capacity
        let alarm = util_10s >= 1.0 || util_60s >= 1.0 || util_120s >= 1.0 || util_300s >= 1.0;

        serde_json::json!({
            "current_connections": self.current(),
            "num_cpus": self.num_cpus,
            "pool_size": self.num_cpus * 2,
            "max_concurrent": self.num_cpus,
            "utilization": self.current() as f64 / self.num_cpus as f64,
            "avg_utilization": {
                "1s": util_1s,
                "10s": util_10s,
                "60s": util_60s,
                "120s": util_120s,
                "300s": util_300s,
            },
            "alarm": alarm,
        })
    }
}

impl Default for Stats {
    fn default() -> Self {
        Self::new()
    }
}
