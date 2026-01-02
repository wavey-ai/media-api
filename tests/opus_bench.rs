//! Opus decoder benchmark using soundkit-decoder pipeline
//!
//! Measures audio seconds decoded per wall-clock second (realtime factor).
//! Processes files serially to measure single-threaded decode performance.
//!
//! Run with: cargo test --release --test opus_bench -- --nocapture

use bytes::Bytes;
use soundkit_decoder::{DecodeOptions, DecodePipeline};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn benchmark_testdata_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmark_testdata")
}

/// Decode a single Opus file using the soundkit-decoder pipeline
fn decode_opus_file(data: &[u8]) -> Result<(usize, Duration), String> {
    let options = DecodeOptions {
        output_sample_rate: Some(16_000),
        output_bits_per_sample: Some(16),
        output_channels: Some(1),
    };

    let mut pipeline = DecodePipeline::spawn_with_options(options);

    // Send all data
    pipeline
        .send(Bytes::from(data.to_vec()))
        .map_err(|e| format!("Send error: {:?}", e))?;

    // Send EOF
    pipeline
        .send(Bytes::new())
        .map_err(|e| format!("EOF send error: {:?}", e))?;

    let start = Instant::now();
    let mut total_output_bytes = 0usize;

    // Drain all output
    loop {
        match pipeline.recv() {
            Some(Ok(audio_data)) => {
                total_output_bytes += audio_data.data().len();
            }
            Some(Err(e)) => {
                return Err(format!("Decode error: {:?}", e));
            }
            None => break,
        }
    }

    Ok((total_output_bytes, start.elapsed()))
}

struct FileResult {
    input_bytes: usize,
    output_bytes: usize,
    actual_audio_secs: f64,
    decode_time: Duration,
}

impl FileResult {
    fn realtime_factor(&self) -> f64 {
        if self.decode_time.as_secs_f64() > 0.0 {
            self.actual_audio_secs / self.decode_time.as_secs_f64()
        } else {
            0.0
        }
    }
}

#[test]
fn bench_opus_serial_decode() {
    println!();
    println!("╔══════════════════════════════════════════════════════════════════════════════════╗");
    println!("║              SOUNDKIT-DECODER OPUS BENCHMARK (Serial Processing)                 ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════════╝");
    println!();

    let opus_dir = benchmark_testdata_path().join("opus");
    if !opus_dir.exists() {
        println!("Opus test data not found at {:?}, skipping benchmark", opus_dir);
        return;
    }

    // Load all opus files
    let mut files: Vec<PathBuf> = fs::read_dir(&opus_dir)
        .expect("Failed to read opus directory")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| e.path())
        .collect();

    files.sort();

    println!("Found {} Opus files", files.len());
    println!();

    let mut results: Vec<FileResult> = Vec::new();
    let mut failures = 0usize;

    let overall_start = Instant::now();

    // Process files serially
    for (i, path) in files.iter().enumerate() {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let data = match fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("  [{}] Failed to read {}: {}", i + 1, name, e);
                failures += 1;
                continue;
            }
        };

        let input_bytes = data.len();

        let file_start = Instant::now();
        match decode_opus_file(&data) {
            Ok((output_bytes, _decode_time)) => {
                let decode_time = file_start.elapsed();

                // Calculate actual audio duration from output
                // 16kHz, 16-bit, mono = 32000 bytes/sec
                let actual_audio_secs = output_bytes as f64 / 32000.0;

                let result = FileResult {
                    input_bytes,
                    output_bytes,
                    actual_audio_secs,
                    decode_time,
                };

                // Progress every 100 files
                if (i + 1) % 100 == 0 {
                    println!(
                        "  [{:4}/{}] {:5.1}x realtime | {:.2}s audio in {:.3}s",
                        i + 1,
                        files.len(),
                        result.realtime_factor(),
                        result.actual_audio_secs,
                        result.decode_time.as_secs_f64()
                    );
                }

                results.push(result);
            }
            Err(e) => {
                eprintln!("  [{}] Failed to decode {}: {}", i + 1, name, e);
                failures += 1;
            }
        }
    }

    let overall_elapsed = overall_start.elapsed();

    println!();
    println!("═══════════════════════════════════════════════════════════════════════════════════");
    println!();

    // Calculate statistics
    let total_input_bytes: usize = results.iter().map(|r| r.input_bytes).sum();
    let total_output_bytes: usize = results.iter().map(|r| r.output_bytes).sum();
    let total_audio_secs: f64 = results.iter().map(|r| r.actual_audio_secs).sum();
    let total_decode_secs: f64 = results.iter().map(|r| r.decode_time.as_secs_f64()).sum();

    let realtime_factors: Vec<f64> = results.iter().map(|r| r.realtime_factor()).collect();
    let min_rt = realtime_factors.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_rt = realtime_factors.iter().cloned().fold(0.0f64, f64::max);
    let avg_rt = if !realtime_factors.is_empty() {
        realtime_factors.iter().sum::<f64>() / realtime_factors.len() as f64
    } else {
        0.0
    };

    // Median
    let mut sorted_rt = realtime_factors.clone();
    sorted_rt.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median_rt = if !sorted_rt.is_empty() {
        sorted_rt[sorted_rt.len() / 2]
    } else {
        0.0
    };

    // P5 (worst 5%)
    let p5_idx = (sorted_rt.len() as f64 * 0.05) as usize;
    let p5_rt = if p5_idx < sorted_rt.len() {
        sorted_rt[p5_idx]
    } else {
        min_rt
    };

    // Overall realtime factor (total audio / total decode time)
    let overall_rt = if total_decode_secs > 0.0 {
        total_audio_secs / total_decode_secs
    } else {
        0.0
    };

    println!("  RESULTS");
    println!("  ───────────────────────────────────────────────────────────────────────────────");
    println!();
    println!("  Files processed:     {:>6}", results.len());
    println!("  Failures:            {:>6}", failures);
    println!();
    println!("  Total input:         {:>6.2} MB", total_input_bytes as f64 / 1_048_576.0);
    println!("  Total output:        {:>6.2} MB", total_output_bytes as f64 / 1_048_576.0);
    println!("  Total audio:         {:>6.1} seconds ({:.1} minutes)", total_audio_secs, total_audio_secs / 60.0);
    println!();
    println!("  Total decode time:   {:>6.2} seconds", total_decode_secs);
    println!("  Wall clock time:     {:>6.2} seconds", overall_elapsed.as_secs_f64());
    println!();
    println!("  ───────────────────────────────────────────────────────────────────────────────");
    println!("  REALTIME FACTOR (audio sec / decode sec)");
    println!("  ───────────────────────────────────────────────────────────────────────────────");
    println!();
    println!("  Overall:             {:>6.1}x", overall_rt);
    println!();
    println!("  Per-file statistics:");
    println!("    Min:               {:>6.1}x", min_rt);
    println!("    P5 (worst 5%):     {:>6.1}x", p5_rt);
    println!("    Median:            {:>6.1}x", median_rt);
    println!("    Average:           {:>6.1}x", avg_rt);
    println!("    Max:               {:>6.1}x", max_rt);
    println!();

    // Histogram of realtime factors
    println!("  ───────────────────────────────────────────────────────────────────────────────");
    println!("  REALTIME FACTOR DISTRIBUTION");
    println!("  ───────────────────────────────────────────────────────────────────────────────");
    println!();

    let buckets = [
        (0.0, 10.0, "  <10x"),
        (10.0, 50.0, "10-50x"),
        (50.0, 100.0, "50-100x"),
        (100.0, 500.0, "100-500x"),
        (500.0, 1000.0, "500-1000x"),
        (1000.0, 2000.0, "1k-2kx"),
        (2000.0, f64::INFINITY, " >2000x"),
    ];

    let max_count = buckets
        .iter()
        .map(|(lo, hi, _)| {
            realtime_factors
                .iter()
                .filter(|&&x| x >= *lo && x < *hi)
                .count()
        })
        .max()
        .unwrap_or(1);

    for (lo, hi, label) in &buckets {
        let count = realtime_factors
            .iter()
            .filter(|&&x| x >= *lo && x < *hi)
            .count();

        let bar_width = if max_count > 0 {
            (count * 50) / max_count
        } else {
            0
        };
        let bar = "█".repeat(bar_width);
        let pct = if !results.is_empty() {
            count as f64 / results.len() as f64 * 100.0
        } else {
            0.0
        };

        println!("  {} │{:>4} ({:>5.1}%) {}", label, count, pct, bar);
    }

    println!();
    println!("═══════════════════════════════════════════════════════════════════════════════════");
    println!();

    // Assert reasonable performance
    assert!(failures == 0, "Some files failed to decode");
    assert!(overall_rt > 10.0, "Realtime factor should be > 10x, got {:.1}x", overall_rt);
}
