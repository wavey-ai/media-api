use bytes::Bytes;
use soundkit::audio_types::AudioData;
use soundkit_decoder::{DecodeOptions, DecodePipeline};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
        let pipeline_start = Instant::now();
        let mut pipeline = DecodePipeline::spawn_with_options(job.options);
        let pipeline_create_ms = pipeline_start.elapsed().as_millis();

        let mut input_done = false;
        let mut total_input_bytes = 0usize;
        let mut total_output_bytes = 0usize;
        let process_start = Instant::now();

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
                            total_input_bytes += chunk.len();
                            let _ = pipeline.send(chunk);
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
            let mut had_output = false;
            while let Some(result) = pipeline.try_recv() {
                had_output = true;
                if let Ok(ref audio) = result {
                    total_output_bytes += audio.data().len();
                }
                let mapped = result.map_err(|e| e.to_string());
                if job.output_tx.send(mapped).is_err() {
                    // Output receiver dropped, abort
                    return;
                }
            }

            // Check if we're done
            if input_done && !had_output {
                // Give decoder a bit more time to flush
                thread::sleep(Duration::from_millis(1));

                // Final drain - reduced from 50*5ms=250ms to 10*1ms=10ms
                let flush_start = Instant::now();
                let mut idle_count = 0;
                loop {
                    match pipeline.try_recv() {
                        Some(result) => {
                            idle_count = 0;
                            if let Ok(ref audio) = result {
                                total_output_bytes += audio.data().len();
                            }
                            let mapped = result.map_err(|e| e.to_string());
                            if job.output_tx.send(mapped).is_err() {
                                return;
                            }
                        }
                        None => {
                            idle_count += 1;
                            if idle_count > 10 {
                                // No more output after 10ms
                                break;
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                    }
                }
                let flush_ms = flush_start.elapsed().as_millis();
                let process_ms = process_start.elapsed().as_millis();
                let total_ms = job_start.elapsed().as_millis();

                // Debug timing (uncomment to enable)
                // eprintln!(
                //     "[POOL] pipeline_create={}ms process={}ms flush={}ms total={}ms input={}KB output={}KB",
                //     pipeline_create_ms, process_ms, flush_ms, total_ms,
                //     total_input_bytes / 1024, total_output_bytes / 1024
                // );
                let _ = (pipeline_create_ms, process_ms, flush_ms, total_ms, total_input_bytes, total_output_bytes);
                break;
            }

            // Small yield to avoid busy spinning
            thread::sleep(Duration::from_micros(100));
        }
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
}

impl Drop for PooledDecoder {
    fn drop(&mut self) {
        // Send EOF if not already sent to let the worker finish
        let _ = self.input_tx.send(Bytes::new());
    }
}
