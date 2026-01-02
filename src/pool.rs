use access_unit::{detect_audio, AudioType};
use bytes::Bytes;
use soundkit::audio_types::AudioData;
use soundkit_decoder::{DecodeOptions, DecodePipeline};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tracing::info;

/// A pool of decoder worker threads that process audio decode jobs.
/// Workers are pre-spawned and wait for jobs, avoiding thread spawn overhead per request.
pub struct DecoderPool {
    job_tx: SyncSender<Job>,
    _workers: Vec<JoinHandle<()>>,
    pool_size: usize,
}

struct Job {
    input_rx: Receiver<Bytes>,
    output_tx: SyncSender<Result<AudioData, String>>,
    options: DecodeOptions,
}

/// Handle for interacting with a pooled decoder.
/// Provides the same interface as DecodePipelineHandle.
pub struct PooledDecoder {
    input_tx: SyncSender<Bytes>,
    output_rx: Receiver<Result<AudioData, String>>,
}

impl DecoderPool {
    /// Create a new decoder pool with the specified number of worker threads.
    /// Workers are spawned immediately and wait for jobs.
    pub fn new(size: usize) -> Self {
        // Bounded channel - blocks when all workers are busy
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(size);
        let job_rx = Arc::new(Mutex::new(job_rx));

        let workers: Vec<_> = (0..size)
            .map(|id| {
                let job_rx = Arc::clone(&job_rx);
                thread::Builder::new()
                    .name(format!("decoder-worker-{}", id))
                    .spawn(move || {
                        loop {
                            // Wait for a job (blocks until one is available)
                            let job = {
                                let rx = job_rx.lock().unwrap();
                                match rx.recv() {
                                    Ok(job) => job,
                                    Err(_) => break, // Channel closed, shutdown
                                }
                            };
                            Self::process_job(job);
                        }
                    })
                    .expect("Failed to spawn decoder worker")
            })
            .collect();

        Self {
            job_tx,
            _workers: workers,
            pool_size: size,
        }
    }

    /// Process a single decode job
    fn process_job(job: Job) {
        let job_start = Instant::now();

        // Create decoder pipeline for this job
        let mut pipeline = DecodePipeline::spawn_with_options(job.options.clone());

        let mut input_done = false;
        let mut input_bytes = 0usize;
        let mut output_bytes = 0usize;
        let mut output_sample_rate = 0u32;
        let mut output_channels = 0u8;
        let mut codec: Option<AudioType> = None;

        // Process until input is exhausted and output is drained
        loop {
            // Read available input
            if !input_done {
                match job.input_rx.try_recv() {
                    Ok(chunk) => {
                        if chunk.is_empty() {
                            // EOF signal
                            let _ = pipeline.send(Bytes::new());
                            input_done = true;
                        } else {
                            // Detect codec from first chunk
                            if codec.is_none() && chunk.len() >= 12 {
                                codec = Some(detect_audio(&chunk));
                            }
                            input_bytes += chunk.len();

                            // Retry if buffer is full - don't drop data!
                            while pipeline.send(chunk.clone()).is_err() {
                                std::thread::sleep(Duration::from_micros(100));
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        // No input available yet
                    }
                    Err(TryRecvError::Disconnected) => {
                        // Input channel closed
                        let _ = pipeline.send(Bytes::new());
                        input_done = true;
                    }
                }
            }

            // Drain available output
            while let Some(result) = pipeline.try_recv() {
                if let Ok(ref audio_data) = result {
                    output_bytes += audio_data.data().len();
                    output_sample_rate = audio_data.sampling_rate();
                    output_channels = audio_data.channel_count();
                }
                let mapped = result.map_err(|e| e.to_string());
                if job.output_tx.send(mapped).is_err() {
                    // Output receiver dropped, abort
                    return;
                }
            }

            // Check if we're done - use blocking recv to drain remaining output
            if input_done {
                // Final drain using blocking recv - waits for pipeline worker to finish
                loop {
                    match pipeline.recv() {
                        Some(result) => {
                            if let Ok(ref audio_data) = result {
                                output_bytes += audio_data.data().len();
                                output_sample_rate = audio_data.sampling_rate();
                                output_channels = audio_data.channel_count();
                            }
                            let mapped = result.map_err(|e| e.to_string());
                            if job.output_tx.send(mapped).is_err() {
                                return;
                            }
                        }
                        None => {
                            // Pipeline worker finished - no more output
                            break;
                        }
                    }
                }
                break;
            }

            // Small yield to avoid busy spinning while waiting for input
            thread::sleep(Duration::from_micros(100));
        }

        // Log metrics
        let total_time = job_start.elapsed();

        // Calculate audio duration from output
        // bytes / (sample_rate * channels * bytes_per_sample)
        let bytes_per_sample = job.options.output_bits_per_sample.unwrap_or(16) as usize / 8;
        let audio_ms = if output_sample_rate > 0 && output_channels > 0 {
            (output_bytes as u64 * 1000)
                / (output_sample_rate as u64 * output_channels as u64 * bytes_per_sample as u64)
        } else {
            0
        };

        let time_ms = total_time.as_millis() as u64;
        let realtime_factor = if time_ms > 0 { audio_ms / time_ms } else { 0 };

        let codec_name = codec
            .map(|c| format!("{:?}", c))
            .unwrap_or_else(|| "unknown".to_string());

        info!(
            codec = %codec_name,
            audio_ms,
            rt = realtime_factor,
            in_bytes = input_bytes,
            out_bytes = output_bytes,
            time_ms,
            "decode"
        );
    }

    /// Acquire a decoder from the pool.
    /// Blocks if all workers are busy (provides natural backpressure).
    pub fn acquire(&self, options: DecodeOptions) -> Result<PooledDecoder, String> {
        // Bounded channels for this request's I/O
        let (input_tx, input_rx) = mpsc::sync_channel::<Bytes>(32);
        let (output_tx, output_rx) = mpsc::sync_channel::<Result<AudioData, String>>(32);

        let job = Job {
            input_rx,
            output_tx,
            options,
        };

        // This blocks if all workers are busy
        self.job_tx
            .send(job)
            .map_err(|_| "Decoder pool shutdown".to_string())?;

        Ok(PooledDecoder { input_tx, output_rx })
    }

    /// Get the pool size
    pub fn size(&self) -> usize {
        self.pool_size
    }
}

impl PooledDecoder {
    /// Send input data to the decoder.
    /// Send an empty Bytes to signal EOF.
    pub fn send(&self, data: Bytes) -> Result<(), String> {
        self.input_tx
            .send(data)
            .map_err(|_| "Decoder channel closed".to_string())
    }

    /// Try to receive decoded output (non-blocking).
    /// Returns Some(result) if data available, None if empty, Err if channel closed.
    pub fn try_recv(&self) -> Result<Option<Result<AudioData, String>>, ()> {
        match self.output_rx.try_recv() {
            Ok(result) => Ok(Some(result)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(()), // Decoder finished
        }
    }

    /// Receive decoded output (blocking).
    /// Returns Some(result) when data available, None when channel closed (decoder finished).
    pub fn recv(&self) -> Option<Result<AudioData, String>> {
        self.output_rx.recv().ok()
    }
}

impl Drop for PooledDecoder {
    fn drop(&mut self) {
        // Send EOF if not already sent to let the worker finish
        let _ = self.input_tx.send(Bytes::new());
    }
}
