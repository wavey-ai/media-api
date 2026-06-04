use bytes::Bytes;
use libopus::{decoder, encoder, Application};
use soundkit::audio_bytes::s16le_to_i16;
use soundkit_decoder::{DecodeOptions, DecodePipeline};
use std::env;
use std::f64::INFINITY;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_SOURCE_DIR: &str = "/Users/jamie/Downloads/Lori Asha - Lori Asha Album Premix";
const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_CHANNELS: u8 = 2;
const DEFAULT_BITRATE: u32 = 96_000;
const FRAME_MS: usize = 20;
const MAX_FRAME_SAMPLES: usize = 5760;
const PACKET_PREFIX_BYTES: usize = 4;

#[derive(Clone)]
struct Config {
    source_dir: PathBuf,
    sample_rate: u32,
    channels: u8,
    bitrate: u32,
}

#[derive(Clone)]
struct SourceTrack {
    name: String,
    pcm: Vec<i16>,
}

#[derive(Clone)]
struct Quality {
    sample_count: usize,
    min_len: usize,
    max_len: usize,
    rms_error: f64,
    mae: f64,
    max_abs: i16,
    snr_db: f64,
    mse: f64,
}

#[derive(Clone, Copy)]
enum Backend {
    SoundKit,
    LibOpusRust,
}

impl Backend {
    fn label(&self) -> &'static str {
        match self {
            Backend::SoundKit => "soundkit-opus",
            Backend::LibOpusRust => "libopus-rs",
        }
    }
}

#[derive(Clone)]
struct EngineResult {
    track: String,
    backend: Backend,
    encode_time: Duration,
    decode_time: Duration,
    encoded_bytes: usize,
    decoded_bytes: usize,
    audio_secs: f64,
    quality: Option<Quality>,
    output_len_match: isize,
    encode_error: Option<String>,
    decode_error: Option<String>,
}

impl EngineResult {
    fn encode_rtf(&self) -> f64 {
        if self.encode_time.is_zero() || self.encoded_bytes == 0 {
            0.0
        } else {
            self.audio_secs / self.encode_time.as_secs_f64()
        }
    }

    fn decode_rtf(&self) -> f64 {
        if self.decode_time.is_zero() {
            0.0
        } else {
            self.audio_secs / self.decode_time.as_secs_f64()
        }
    }
}

fn parse_args() -> Config {
    let mut config = Config {
        source_dir: PathBuf::from(DEFAULT_SOURCE_DIR),
        sample_rate: DEFAULT_SAMPLE_RATE,
        channels: DEFAULT_CHANNELS,
        bitrate: DEFAULT_BITRATE,
    };

    let args = env::args().collect::<Vec<_>>();
    let mut i = 1usize;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "--source-dir" => {
                if i + 1 < args.len() {
                    config.source_dir = PathBuf::from(&args[i + 1]);
                    i += 2;
                } else {
                    panic!("--source-dir requires a value");
                }
            }
            "--sample-rate" => {
                if i + 1 >= args.len() {
                    eprintln!(
                        "--sample-rate requires a value, using default {DEFAULT_SAMPLE_RATE}"
                    );
                    i += 1;
                    continue;
                }
                match args.get(i + 1).and_then(|value| value.parse::<u32>().ok()) {
                    Some(value) => config.sample_rate = value,
                    None => {
                        eprintln!(
                            "invalid --sample-rate value '{}', expected integer Hz",
                            args.get(i + 1).unwrap_or(&String::new())
                        );
                        config.sample_rate = DEFAULT_SAMPLE_RATE;
                    }
                }
                i += 2;
            }
            "--channels" => {
                if i + 1 >= args.len() {
                    eprintln!("--channels requires a value, using default {DEFAULT_CHANNELS}");
                    i += 1;
                    continue;
                }
                match args.get(i + 1).and_then(|value| value.parse::<u8>().ok()) {
                    Some(value) => config.channels = value,
                    None => {
                        eprintln!(
                            "invalid --channels value '{}', expected 1-255",
                            args.get(i + 1).unwrap_or(&String::new())
                        );
                        config.channels = DEFAULT_CHANNELS;
                    }
                }
                i += 2;
            }
            "--bitrate" => {
                if i + 1 >= args.len() {
                    eprintln!("--bitrate requires a value, using default {DEFAULT_BITRATE}");
                    i += 1;
                    continue;
                }
                match args.get(i + 1).and_then(|value| value.parse::<u32>().ok()) {
                    Some(value) => config.bitrate = value,
                    None => {
                        eprintln!(
                            "invalid --bitrate value '{}', expected integer",
                            args.get(i + 1).unwrap_or(&String::new())
                        );
                        config.bitrate = DEFAULT_BITRATE;
                    }
                }
                i += 2;
            }
            _ => {
                if args[i].starts_with("--") {
                    eprintln!("warning: unknown arg {}", args[i]);
                }
                i += 1;
            }
        }
    }

    config
}

fn print_help() {
    println!("opus_benchmark");
    println!("Usage: cargo run --bin opus_benchmark -- [options]");
    println!("Options:");
    println!(
        "  --source-dir <path>    Directory containing mp3 tracks (default: {DEFAULT_SOURCE_DIR})"
    );
    println!("  --sample-rate <hz>     Decode/encode sample rate for PCM and Opus ops (default: {DEFAULT_SAMPLE_RATE})");
    println!("  --channels <num>       Channels for decoded PCM and Opus ops (default: {DEFAULT_CHANNELS})");
    println!("  --bitrate <bps>        Opus bitrate for both engines (default: {DEFAULT_BITRATE})");
    println!("  --help                 Show this help");
}

fn discover_mp3_tracks(source_dir: &Path) -> Vec<PathBuf> {
    let mut tracks = fs::read_dir(source_dir)
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mp3"))
                && entry.path().is_file()
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();

    tracks.sort();
    tracks
}

fn decode_source_track(path: &Path, sample_rate: u32, channels: u8) -> Result<SourceTrack, String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let options = DecodeOptions {
        output_sample_rate: Some(sample_rate),
        output_bits_per_sample: Some(16),
        output_channels: Some(channels),
    };

    let mut pipeline = DecodePipeline::spawn_with_options(options);

    pipeline
        .send(Bytes::from(bytes))
        .map_err(|e| format!("decode send for {} failed: {e:?}", path.display()))?;
    pipeline
        .send(Bytes::new())
        .map_err(|e| format!("decode eof send for {} failed: {e:?}", path.display()))?;

    let mut output = Vec::new();
    loop {
        match pipeline.recv() {
            Some(Ok(audio_data)) => {
                if audio_data.bits_per_sample() != 16 {
                    return Err(format!(
                        "{}: unsupported bit depth {}",
                        path.display(),
                        audio_data.bits_per_sample()
                    ));
                }
                output.extend_from_slice(audio_data.data());
            }
            Some(Err(error)) => {
                return Err(format!("{}: decode error: {:?}", path.display(), error));
            }
            None => break,
        }
    }

    if output.is_empty() {
        return Err(format!("{}: decode produced no audio", path.display()));
    }

    Ok(SourceTrack {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        pcm: s16le_to_i16(&output),
    })
}

fn encode_with_soundkit(
    track: &SourceTrack,
    sample_rate: u32,
    channels: usize,
    frame_size: usize,
    bitrate: u32,
) -> Result<Vec<u8>, String> {
    use soundkit_opus::OpusEncoder;

    let frame_samples = frame_size.saturating_mul(channels);
    let mut encoder =
        OpusEncoder::new(sample_rate, 16, channels as u32, frame_size as u32, bitrate);
    encoder.init()?;

    let mut frame = vec![0i16; frame_samples];
    let mut encoded = vec![0u8; 1500];
    let mut packets = Vec::new();

    for chunk in track.pcm.chunks(frame_samples) {
        frame.fill(0);
        frame[..chunk.len()].copy_from_slice(chunk);

        let encoded_len = encoder.encode_i16(&frame, &mut encoded)?;
        if encoded_len == 0 {
            continue;
        }

        let encoded_len_u32 = encoded_len as u32;
        packets.extend_from_slice(&encoded_len_u32.to_le_bytes());
        packets.extend_from_slice(&encoded[..encoded_len]);
    }

    Ok(packets)
}

fn decode_with_soundkit(
    packets: &[u8],
    sample_rate: u32,
    channels: usize,
) -> Result<Vec<i16>, String> {
    use soundkit_opus::OpusDecoder;

    let mut decoder = OpusDecoder::new(sample_rate as usize, channels);
    decoder.init()?;

    let mut decoded = Vec::new();
    let mut scratch = vec![0i16; MAX_FRAME_SAMPLES.saturating_mul(channels)];
    let mut cursor = 0usize;

    while cursor + PACKET_PREFIX_BYTES <= packets.len() {
        let mut len_buf = [0u8; PACKET_PREFIX_BYTES];
        len_buf.copy_from_slice(&packets[cursor..cursor + PACKET_PREFIX_BYTES]);
        cursor += PACKET_PREFIX_BYTES;

        let packet_len = u32::from_le_bytes(len_buf) as usize;
        if packet_len == 0 {
            continue;
        }
        if cursor + packet_len > packets.len() {
            return Err("truncated soundkit packet stream".to_string());
        }

        let packet = &packets[cursor..cursor + packet_len];
        cursor += packet_len;

        let samples_per_channel = decoder.decode_i16(packet, &mut scratch, false)?;
        let sample_count = samples_per_channel.saturating_mul(channels);
        if sample_count > 0 {
            decoded.extend_from_slice(&scratch[..sample_count]);
        }
    }

    Ok(decoded)
}

fn encode_with_libopus_rust(
    track: &SourceTrack,
    sample_rate: u32,
    channels: usize,
    frame_size: usize,
    bitrate: u32,
) -> Result<Vec<u8>, String> {
    let frame_samples = frame_size.saturating_mul(channels);
    let coupled = if channels > 1 { 1 } else { 0 };
    let mut encoder = encoder::Encoder::create(
        sample_rate as usize,
        channels,
        1,
        coupled,
        &[0u8, 1u8],
        Application::Audio,
    )
    .map_err(|e| format!("libopus-rs create encoder: {e}"))?;
    encoder
        .set_option(encoder::OPUS_SET_BITRATE_REQUEST, bitrate)
        .map_err(|e| format!("libopus-rs set bitrate: {e}"))?;

    let mut frame = vec![0i16; frame_samples];
    let mut encoded = vec![0u8; 1500];
    let mut packets = Vec::new();

    for chunk in track.pcm.chunks(frame_samples) {
        frame.fill(0);
        frame[..chunk.len()].copy_from_slice(chunk);

        let encoded_len = encoder
            .encode(&frame, &mut encoded)
            .map_err(|e| format!("libopus-rs encode: {e}"))?;
        if encoded_len == 0 {
            continue;
        }

        let encoded_len_u32 = encoded_len as u32;
        packets.extend_from_slice(&encoded_len_u32.to_le_bytes());
        packets.extend_from_slice(&encoded[..encoded_len]);
    }

    Ok(packets)
}

fn decode_with_libopus_rust(
    packets: &[u8],
    sample_rate: u32,
    channels: usize,
) -> Result<Vec<i16>, String> {
    let coupled = if channels > 1 { 1 } else { 0 };
    let mut decoder =
        decoder::Decoder::create(sample_rate as usize, channels, 1, coupled, &[0u8, 1u8])
            .map_err(|e| format!("libopus-rs create decoder: {e}"))?;

    let mut decoded = Vec::new();
    let mut scratch = vec![0i16; MAX_FRAME_SAMPLES.saturating_mul(channels)];
    let mut cursor = 0usize;

    while cursor + PACKET_PREFIX_BYTES <= packets.len() {
        let mut len_buf = [0u8; PACKET_PREFIX_BYTES];
        len_buf.copy_from_slice(&packets[cursor..cursor + PACKET_PREFIX_BYTES]);
        cursor += PACKET_PREFIX_BYTES;

        let packet_len = u32::from_le_bytes(len_buf) as usize;
        if packet_len == 0 {
            continue;
        }
        if cursor + packet_len > packets.len() {
            return Err("truncated libopus-rs packet stream".to_string());
        }

        let packet = &packets[cursor..cursor + packet_len];
        cursor += packet_len;

        let samples_per_channel = decoder.decode(packet, &mut scratch, false);
        let samples_per_channel = match samples_per_channel {
            Ok(v) => v,
            Err(e) => return Err(format!("libopus-rs decode: {e}")),
        };
        let sample_count = samples_per_channel.saturating_mul(channels);
        if sample_count > 0 {
            decoded.extend_from_slice(&scratch[..sample_count]);
        }
    }

    Ok(decoded)
}

fn compare_pcm(reference: &[i16], decoded: &[i16]) -> Option<Quality> {
    let compare_len = reference.len().min(decoded.len());
    if compare_len == 0 {
        return None;
    }

    let mut mse_sum = 0.0f64;
    let mut mae_sum = 0.0f64;
    let mut peak_err = 0i16;
    let mut signal_sum = 0.0f64;

    for i in 0..compare_len {
        let diff = (reference[i] as f64) - (decoded[i] as f64);
        let abs_err = diff.abs();
        mse_sum += diff * diff;
        mae_sum += abs_err;
        signal_sum += (reference[i] as f64).powi(2);

        let err_i16 = (reference[i] - decoded[i]).abs();
        if err_i16 > peak_err {
            peak_err = err_i16;
        }
    }

    let mse = mse_sum / compare_len as f64;
    let rms_error = mse.sqrt();
    let snr_db = {
        let signal_rms = (signal_sum / compare_len as f64).sqrt();
        if signal_rms == 0.0 {
            0.0
        } else if rms_error == 0.0 {
            INFINITY
        } else {
            20.0 * (signal_rms / rms_error).log10()
        }
    };

    Some(Quality {
        sample_count: reference.len().min(decoded.len()),
        min_len: compare_len,
        max_len: reference.len().max(decoded.len()),
        rms_error,
        mae: mae_sum / compare_len as f64,
        max_abs: peak_err,
        snr_db,
        mse,
    })
}

fn run_engine(backend: Backend, track: &SourceTrack, config: &Config) -> EngineResult {
    let sample_rate = config.sample_rate;
    let channels = config.channels as usize;
    let frame_size = (sample_rate as usize / 1000) * FRAME_MS;
    let bitrate = config.bitrate;

    let encode_start = Instant::now();
    let encoded = match backend {
        Backend::SoundKit => {
            encode_with_soundkit(track, sample_rate, channels, frame_size, bitrate)
        }
        Backend::LibOpusRust => {
            encode_with_libopus_rust(track, sample_rate, channels, frame_size, bitrate)
        }
    };
    let encode_time = encode_start.elapsed();

    if let Err(error) = encoded {
        return EngineResult {
            track: track.name.clone(),
            backend,
            encode_time,
            decode_time: Duration::ZERO,
            encoded_bytes: 0,
            decoded_bytes: 0,
            audio_secs: 0.0,
            quality: None,
            output_len_match: 0,
            encode_error: Some(error),
            decode_error: None,
        };
    }
    let encoded = encoded.unwrap_or_default();

    let decode_start = Instant::now();
    let decoded = match backend {
        Backend::SoundKit => decode_with_soundkit(&encoded, sample_rate, channels),
        Backend::LibOpusRust => decode_with_libopus_rust(&encoded, sample_rate, channels),
    };
    let decode_time = decode_start.elapsed();

    let (decoded, decode_error) = match decoded {
        Ok(decoded) => (decoded, None),
        Err(error) => (Vec::new(), Some(error)),
    };

    let audio_secs = track.pcm.len() as f64 / (sample_rate as f64 * channels as f64);
    let quality = if decode_error.is_none() {
        compare_pcm(&track.pcm, &decoded)
    } else {
        None
    };

    let output_len_match = decoded.len() as isize - track.pcm.len() as isize;
    let decoded_bytes = decoded.len().saturating_mul(2);

    EngineResult {
        track: track.name.clone(),
        backend,
        encode_time,
        decode_time,
        encoded_bytes: encoded.len(),
        decoded_bytes,
        audio_secs,
        quality,
        output_len_match,
        encode_error: None,
        decode_error,
    }
}

fn print_header() {
    println!(
        "{:<44} {:<18} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>11} {:>9} {:>10} {:>12} {:>7}",
        "track",
        "backend",
        "enc s",
        "encRTF",
        "enc MB",
        "dec s",
        "decRTF",
        "dec MB",
        "SNR(dB)",
        "MAE",
        "RMS",
        "maxAbs",
        "lenΔ",
    );
    println!(
        "{:-<44} {:->18} {:->10} {:->10} {:->10} {:->10} {:->10} {:->10} {:->11} {:->9} {:->10} {:->12} {:->7}",
        "", "", "", "", "", "", "", "", "", "", "", "", "",
    );
}

fn print_row(result: &EngineResult) {
    if let Some(err) = result.encode_error.as_ref() {
        println!(
            "{:<44} {:<18} encode fail: {err}",
            result.track,
            result.backend.label(),
        );
        return;
    }

    if let Some(err) = result.decode_error.as_ref() {
        println!(
            "{:<44} {:<18} enc {:>9.3} MB {:>10.3}s decode fail: {err}",
            result.track,
            result.backend.label(),
            result.encoded_bytes as f64 / 1_048_576.0,
            result.encode_time.as_secs_f64(),
        );
        return;
    }

    let quality = result
        .quality
        .as_ref()
        .expect("quality missing for successful run");

    println!(
        "{:<44} {:<18} {:>9.3} {:>10.2}x {:>10.3} {:>10.3} {:>10.2}x {:>10.3} {:>11.2} {:>9.2} {:>10.4} {:>12} {:>7}",
        result.track,
        result.backend.label(),
        result.encode_time.as_secs_f64(),
        result.encode_rtf(),
        result.encoded_bytes as f64 / 1_048_576.0,
        result.decode_time.as_secs_f64(),
        result.decode_rtf(),
        result.decoded_bytes as f64 / 1_048_576.0,
        quality.snr_db,
        quality.mae,
        quality.rms_error,
        quality.max_abs,
        result.output_len_match,
    );
}

fn summarize(results: &[EngineResult], audio_source_secs: f64, source_bytes: usize, label: &str) {
    if results.is_empty() {
        return;
    }
    let mut total_encode = Duration::ZERO;
    let mut total_decode = Duration::ZERO;
    let mut total_encoded = 0usize;
    let mut total_decoded = 0usize;
    let mut good = 0usize;
    let mut snr_sum = 0.0f64;
    let mut mae_sum = 0.0f64;
    let mut rms_sum = 0.0f64;

    for result in results {
        if result.encode_error.is_none() && result.decode_error.is_none() {
            good += 1;
            total_encode += result.encode_time;
            total_decode += result.decode_time;
            total_encoded += result.encoded_bytes;
            total_decoded += result.decoded_bytes;
            if let Some(quality) = &result.quality {
                snr_sum += quality.snr_db;
                mae_sum += quality.mae;
                rms_sum += quality.rms_error;
            }
        }
    }

    if good == 0 {
        println!();
        println!("{}: no successful runs", label);
        return;
    }

    let audio_seconds = audio_source_secs;
    let encode_secs = total_encode.as_secs_f64();
    let decode_secs = total_decode.as_secs_f64();
    let enc_rtf = if encode_secs > 0.0 {
        audio_seconds / encode_secs
    } else {
        0.0
    };
    let dec_rtf = if decode_secs > 0.0 {
        audio_seconds / decode_secs
    } else {
        0.0
    };
    let avg_snr = snr_sum / good as f64;
    let avg_mae = mae_sum / good as f64;
    let avg_rms = rms_sum / good as f64;
    let compression_ratio = (source_bytes as f64) / (total_encoded.max(1) as f64);

    println!();
    println!("SUMMARY ({label})");
    println!("  tracks completed:     {:>6} / {}", good, results.len());
    println!("  audio duration:       {:>8.2} s", audio_seconds);
    println!(
        "  encode:              {:>8.2} s total | {:>7.2}x RTF",
        encode_secs, enc_rtf
    );
    println!(
        "  decode:              {:>8.2} s total | {:>7.2}x RTF",
        decode_secs, dec_rtf
    );
    println!(
        "  encoded output:      {:>8.2} MB",
        total_encoded as f64 / 1_048_576.0
    );
    println!(
        "  decoded output:      {:>8.2} MB",
        total_decoded as f64 / 1_048_576.0
    );
    println!(
        "  avg output ratio:    {:>7.2}x (raw to opus bytes)",
        compression_ratio
    );
    println!(
        "  avg PCM quality:     SNR {:>6.2} dB | MAE {:>8.3} | RMS {:>8.4}",
        avg_snr, avg_mae, avg_rms
    );
}

fn main() {
    let config = parse_args();
    let tracks = discover_mp3_tracks(&config.source_dir);

    if tracks.is_empty() {
        eprintln!(
            "No mp3 tracks found under {}",
            config.source_dir.to_string_lossy()
        );
        return;
    }

    println!(
        "Decoding source tracks to PCM @{} Hz / {} ch",
        config.sample_rate, config.channels
    );
    println!("Tracks:");
    for track in &tracks {
        println!("  {}", track.display());
    }

    let mut source_tracks = Vec::new();
    for path in &tracks {
        match decode_source_track(path, config.sample_rate, config.channels) {
            Ok(track) => source_tracks.push(track),
            Err(error) => eprintln!("Skipping track: {error}"),
        }
    }

    if source_tracks.is_empty() {
        eprintln!("Unable to decode any source tracks");
        return;
    }

    let mut total_source_bytes = 0usize;
    let mut total_source_secs = 0.0f64;
    for track in &source_tracks {
        total_source_bytes += track.pcm.len().saturating_mul(2);
        total_source_secs +=
            track.pcm.len() as f64 / (config.sample_rate as f64 * config.channels as f64);
    }

    let engines = [Backend::SoundKit, Backend::LibOpusRust];
    let mut all_results = Vec::new();

    print_header();

    println!(
        "\nRunning in order: {} then {}",
        Backend::SoundKit.label(),
        Backend::LibOpusRust.label()
    );

    for backend in engines {
        println!("\n{}:", backend.label());
        for track in &source_tracks {
            let result = run_engine(backend, track, &config);
            print_row(&result);
            all_results.push(result);
        }
    }

    let soundkit_results = all_results
        .iter()
        .filter(|result| matches!(result.backend, Backend::SoundKit))
        .cloned()
        .collect::<Vec<_>>();
    let libopus_results = all_results
        .iter()
        .filter(|result| matches!(result.backend, Backend::LibOpusRust))
        .cloned()
        .collect::<Vec<_>>();

    summarize(
        &soundkit_results,
        total_source_secs,
        total_source_bytes,
        Backend::SoundKit.label(),
    );
    summarize(
        &libopus_results,
        total_source_secs,
        total_source_bytes,
        Backend::LibOpusRust.label(),
    );
}
