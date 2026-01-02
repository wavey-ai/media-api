# media-api

High-performance streaming audio decode service. Accepts encoded audio in various formats and returns raw PCM data.

## Features

- **Multi-format decoding**: MP3, AAC, Opus, OGG/Opus, WebM, M4A, FLAC
- **HTTP/1.1, HTTP/2, HTTP/3**: Full protocol support
- **Configurable output**: Sample rate, bit depth, channel count
- **High throughput**: 300+ files/sec, 15x realtime at full CPU utilization
- **Thread pool**: Pre-spawned decoder workers for minimal latency (~16ms)
- **Stats endpoint**: Real-time metrics for external orchestration

## Quick Start

```bash
# Set TLS certificates (required)
export TLS_CERT_BASE64="..."
export TLS_KEY_BASE64="..."

# Run the server
cargo run --release
```

Server starts on port 8443 by default.

## API

### POST /decode

Decode audio to PCM.

**Request**: Raw encoded audio bytes in body

**Query Parameters**:
| Parameter | Description | Default |
|-----------|-------------|---------|
| `sample_rate` | Output sample rate (Hz) | Source rate |
| `bits` | Bits per sample (16, 24, 32) | 16 |
| `channels` | Output channels (1-16) | Source channels |

**Response Headers**:
- `X-Sample-Rate`: Output sample rate
- `X-Channels`: Output channel count
- `X-Bits-Per-Sample`: Output bit depth
- `X-Encoding`: `pcm-signed` or `pcm-float`

**Response Body**: Raw PCM bytes (little-endian)

**Example**:
```bash
curl -X POST https://localhost:8443/decode?sample_rate=16000&bits=16&channels=1 \
  --data-binary @audio.mp3 \
  -o output.pcm
```

### GET /stats

Real-time server metrics for external scaling orchestration.

**Response**:
```json
{
  "current_connections": 4,
  "num_cpus": 8,
  "pool_size": 16,
  "max_concurrent": 8,
  "utilization": 0.5,
  "avg_utilization": {
    "1s": 0.48,
    "10s": 0.52,
    "60s": 0.45,
    "120s": 0.42,
    "300s": 0.40
  },
  "alarm": false
}
```

| Field | Description |
|-------|-------------|
| `current_connections` | Active decode requests |
| `num_cpus` | CPU count on host |
| `pool_size` | Decoder worker threads (2x CPUs) |
| `max_concurrent` | Maximum efficient concurrency (= num_cpus) |
| `utilization` | Current connections / num_cpus |
| `avg_utilization` | Rolling window averages (1s, 10s, 60s, 2min, 5min) |
| `alarm` | True if any 10s+ window utilization >= 1.0 |

### GET /status, GET /health

Health check endpoints. Returns:
```json
{"status": "ok", "service": "media-api", "version": "0.1.0"}
```

## Configuration

| Environment Variable | CLI Flag | Default | Description |
|---------------------|----------|---------|-------------|
| `PORT` | `--port` | 8443 | HTTPS port |
| `TLS_CERT_BASE64` | `--tls-cert-base64` | Required | Base64 PEM certificate |
| `TLS_KEY_BASE64` | `--tls-key-base64` | Required | Base64 PEM private key |
| `RAW_TCP_PORT` | `--raw-tcp-port` | 9000 | Raw TCP port |
| `ENABLE_RAW_TCP` | `--enable-raw-tcp` | false | Enable raw TCP handler |
| `RAW_TCP_TLS` | `--raw-tcp-tls` | false | TLS for raw TCP |
| `DEFAULT_OUTPUT_SAMPLE_RATE` | `--default-output-sample-rate` | - | Default output sample rate |
| `DEFAULT_OUTPUT_BITS` | `--default-output-bits` | - | Default bits per sample |
| `DEFAULT_OUTPUT_CHANNELS` | `--default-output-channels` | - | Default channel count |

## Supported Formats

| Format | Extension | Concatenation |
|--------|-----------|---------------|
| MP3 | `.mp3` | Yes |
| AAC ADTS | `.aac` | Yes |
| AAC/M4A | `.m4a` | No |
| Opus | `.opus` | Yes |
| OGG/Opus | `.ogg` | At bitstream boundaries |
| WebM/Opus | `.webm` | No |
| FLAC | `.flac` | No |

Formats marked "Yes" for concatenation support multiple files concatenated into a single stream.

## Performance

On an 8-core machine:

| Concurrency | Files/sec | Latency (avg) | Realtime |
|-------------|-----------|---------------|----------|
| 4 (0.5x CPUs) | 150 | 18ms | 330x |
| 8 (1.0x CPUs) | 300 | 20ms | 660x |
| 16 (2.0x CPUs) | 380 | 35ms | 840x |
| 24 (3.0x CPUs) | 420 | 50ms | 930x |

Optimal concurrency equals CPU count. Beyond that, latency increases with diminishing throughput gains.

## Load Testing

```bash
# Copy test data (one-time setup)
cp -r /path/to/testdata benchmark_testdata/

# Run ramped load test
cargo run --release --bin load_test
```

Tests at multiple concurrency levels (0.5x, 1x, 2x, 3x CPUs) with per-format metrics and throughput graphs.

## Project Structure

```
media-api/
├── src/
│   ├── main.rs          # Server entry point
│   ├── lib.rs           # Library exports
│   ├── config.rs        # Configuration
│   ├── router.rs        # HTTP routing and decode handler
│   ├── decode.rs        # Decode options parsing
│   ├── pool.rs          # Decoder thread pool
│   ├── stats.rs         # Connection statistics
│   └── tcp_handler.rs   # Raw TCP handler
├── tests/
│   └── integration.rs   # Integration tests with waveform viz
├── benches/
│   └── load_test.rs     # Ramped load test
└── testdata/            # Test audio files
```

## Development

```bash
# Run tests
cargo test

# Run integration tests (requires TLS certs in certs/)
cargo test --test integration

# Build release
cargo build --release
```

## License

MIT License - see [LICENSE](LICENSE)
