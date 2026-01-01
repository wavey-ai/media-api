# media-api

High-performance streaming audio decode service. Accepts encoded audio in various formats and returns raw PCM data.

## Features

- **Multi-format decoding**: MP3, AAC, Opus, OGG/Opus, WebM, M4A, FLAC
- **HTTP/2 API**: POST encoded audio, receive PCM response
- **Configurable output**: Sample rate, bit depth, channel count
- **High throughput**: 40-150x realtime depending on format

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

| Format | Extension | Status |
|--------|-----------|--------|
| MP3 | `.mp3` | Supported |
| AAC | `.aac`, `.m4a` | Supported |
| Opus | `.opus` | Supported |
| OGG/Opus | `.ogg` | Supported |
| WebM/Opus | `.webm` | Supported |
| FLAC | `.flac` | Partial |
| WAV | `.wav` | Not yet |
| Raw PCM | `.raw` | Not yet |

## Performance

Throughput measured as audio seconds decoded per wallclock second:

| Format | Throughput | Notes |
|--------|------------|-------|
| OGG/Opus | ~150x | Highest throughput |
| MP3 | ~45x | Common format |
| AAC | ~45x | Including M4A container |
| Opus | ~20x | Raw Opus packets |

Tested with 50 concurrent connections, ramping to 200.

## Load Testing

Run the benchmark suite:

```bash
# Copy test data (one-time setup)
cp -r /path/to/testdata benchmark_testdata/

# Run load test with autoscaling metrics
cargo run --release --bin load_test
```

The load test includes:
- Concurrency ramp-up (10 -> 200)
- Rolling 10-second window metrics
- Autoscaling notifications with urgency levels:
  - **Warning**: >500ms latency, >20% throughput drop
  - **Critical**: >1000ms latency, >40% throughput drop
  - **Emergency**: >2000ms latency, >60% throughput drop

## Project Structure

```
media-api/
├── src/
│   ├── main.rs          # Server entry point
│   ├── lib.rs           # Library exports
│   ├── config.rs        # Configuration
│   ├── router.rs        # HTTP routing and decode handler
│   ├── decode.rs        # Decode pipeline wrapper
│   └── tcp_handler.rs   # Raw TCP handler
├── tests/
│   └── integration.rs   # Integration tests with waveform viz
├── benches/
│   └── load_test.rs     # Load test with autoscaling metrics
└── testdata/            # Test audio files
```

## Development

```bash
# Run tests
cargo test

# Run integration tests (requires TLS certs)
cargo test --test integration

# Build release
cargo build --release
```

## License

Proprietary - Wavey AI
