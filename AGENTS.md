# HTTP/3 MP3 Data Corruption Issue

## Problem Summary

HTTP/3 (QUIC) MP3 streaming produces corrupted audio output where portions of the data become zeros, while HTTP/2 works correctly. All other audio formats (FLAC, AAC, M4A, Opus, WebM) work correctly with both HTTP/2 and HTTP/3.

## Specific Symptoms

1. **File Size**: Both HTTP/2 and HTTP/3 produce files of identical size (99072 bytes for the test MP3)
2. **Corruption Location**: Data corruption starts at exactly byte 49536 (exactly half of 99072)
3. **Corruption Pattern**: H3 output has 1600 more zero bytes than H2 in the second half
4. **Audio Impact**: Second half of audio has silent sections where audio should be

### Hex Dump Comparison

```
HTTP/2 at byte 49530-49550:
0000c17a: faff c6ff 2700 2200 b0ff 3000 2000 afff  ....'."...0. ...

HTTP/3 at byte 49530-49550:
0000c17a: faff c6ff 2700 0000 0000 0000 0000 0000  ....'...........
```

### Byte Analysis

- Total differences: 16996 bytes
- First diff at byte: 49536 (exactly half the file)
- Last diff at byte: 94858
- Pattern: Multiple zero regions in H3 where H2 has valid audio samples

## Architecture

```
Client (H3) --> QUIC/HTTP3 --> Server Router --> DecoderPool --> soundkit-decoder
                                  |
                                  v
                         H3SplitStreamWriter
                                  |
                                  v
                         h3_quinn SendStream
```

### Key Components

1. **Router** (`src/router.rs`): Handles decode streaming, receives body chunks, sends to decoder, streams output back
2. **DecoderPool** (`src/pool.rs`): Pool of worker threads, each creates a DecodePipeline for jobs
3. **H3SplitStreamWriter** (`web-services/web-service/src/h3.rs`): Wraps h3_quinn SendStream in Arc<Mutex>
4. **soundkit-mp3** (`soundkit/soundkit-mp3`): MP3 decoder using forked nanomp3

## What Has Been Verified Working

### 1. MP3 Decoder (soundkit-mp3)
- Chunk invariance test PASSES - same output regardless of input chunk size
- Test: `cargo test --package soundkit-mp3 test_mp3_chunk_size_invariance`
- Output: "Large chunk output: 49536 samples, Small chunk output: 49536 samples"

### 2. Decode Pipeline (soundkit-decoder)
- Pipeline chunk invariance test PASSES
- Test: `cargo test --package soundkit-decoder test_mp3_pipeline_chunk_invariance`
- All chunk sizes (all-at-once, 1200-byte, 256-byte) produce identical 99072 bytes

### 3. Server-Side Data (Router)
Debug logging shows the server is sending VALID data with non-zero bytes:

```
[ROUTER FLUSH] chunk 0: 49536 bytes at offset 0, 31765 non-zero
[ROUTER FLUSH] chunk 1: 5760 bytes at offset 49536, 4134 non-zero
[ROUTER FLUSH] chunk 2: 6912 bytes at offset 55296, 4843 non-zero
[ROUTER FLUSH] chunk 3: 8064 bytes at offset 62208, 5625 non-zero
[ROUTER FLUSH] chunk 4: 13824 bytes at offset 70272, 11513 non-zero
[ROUTER FLUSH] chunk 5: 6912 bytes at offset 84096, 4984 non-zero
[ROUTER FLUSH] chunk 6: 8064 bytes at offset 91008, 3685 non-zero
[ROUTER] decode stream complete: 99072 bytes in 7 chunks
```

The server sends 99072 bytes in 7 chunks, all with valid non-zero data.

### 4. Client-Side Data (H3 Client)
Debug logging shows the client receives ZERO data in some chunks:

```
[H3 CLIENT] chunk 34: 1236 bytes at offset 48300, 948 non-zero
[H3 CLIENT] chunk 35: 181 bytes at offset 49536, 0 non-zero   <-- ALL ZEROS!
[H3 CLIENT] chunk 36: 1420 bytes at offset 49717, 666 non-zero
...
[H3 CLIENT] chunk 40: 98 bytes at offset 55296, 0 non-zero    <-- ALL ZEROS!
...
[H3 CLIENT] chunk 46: 283 bytes at offset 62208, 0 non-zero   <-- ALL ZEROS!
...
[H3 CLIENT] chunk 75: 1090 bytes at offset 97982, 0 non-zero  <-- ALL ZEROS!
[H3 CLIENT] total: 76 chunks, 99072 bytes
```

## Key Finding

**The corruption occurs in the H3/QUIC transport layer, NOT in the decoder or router.**

- Server sends 7 chunks with valid data
- Client receives 76 chunks (QUIC fragmentation) with some containing all zeros
- The zero chunks appear at the exact offsets where we see corruption in the output file

## Relevant Code Paths

### H3 Body Stream Handler (`h3.rs:214-251`)

```rust
async fn handle_h3_body_stream_request(
    req: http::Request<()>,
    stream: RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    router: Arc<dyn Router>,
) -> Result<(), H3Error> {
    let (send_stream, recv_stream) = stream.split();

    let body_stream: BodyStream = Box::pin(unfold(recv_stream, |mut recv| async move {
        match recv.recv_data().await {
            Ok(Some(mut chunk)) => {
                let data = chunk.copy_to_bytes(chunk.remaining());
                Some((Ok(data), recv))
            }
            Ok(None) => None,
            Err(e) => Some((Err(ServerError::Handler(Box::new(e))), recv)),
        }
    }));

    let send_stream = Arc::new(Mutex::new(send_stream));
    let stream_writer = H3SplitStreamWriter {
        stream: Arc::clone(&send_stream),
    };

    router
        .route_body_stream(req, body_stream, Box::new(stream_writer))
        .await
        .map_err(H3Error::Router)?;

    // Finish the stream
    let mut guard = send_stream.lock().await;
    guard
        .finish()
        .await
        .map_err(|e| H3Error::Transport(e.to_string()))
}
```

### H3 Stream Writer (`h3.rs:80-107`)

```rust
pub struct H3SplitStreamWriter {
    stream: Arc<Mutex<RequestStream<h3_quinn::SendStream<Bytes>, Bytes>>>,
}

#[async_trait::async_trait]
impl StreamWriter for H3SplitStreamWriter {
    async fn send_data(&mut self, data: Bytes) -> Result<(), ServerError> {
        let mut guard = self.stream.lock().await;
        guard
            .send_data(data)
            .await
            .map_err(|e| ServerError::Handler(Box::new(H3Error::Transport(e.to_string()))))
    }
}
```

### Router Decode Handler (`router.rs:86-191`)

```rust
async fn handle_decode_stream(
    &self,
    req: Request<()>,
    mut body: BodyStream,
    mut stream_writer: Box<dyn StreamWriter>,
) -> HandlerResult<()> {
    // ... setup ...

    // Feed input chunks to decoder and stream output
    while let Some(chunk_result) = body.next().await {
        match chunk_result {
            Ok(chunk) => {
                if chunk.is_empty() { continue; }
                decoder.send(chunk)?;

                // Drain available output
                loop {
                    match decoder.try_recv() {
                        Ok(Some(Ok(audio_data))) => {
                            stream_writer.send_data(Bytes::copy_from_slice(audio_data.data())).await?;
                        }
                        Ok(None) => break,
                        Err(()) => break,
                    }
                }
            }
            Err(e) => break,
        }
    }

    // Signal EOF and drain remaining output
    decoder.send(Bytes::new());

    loop {
        match decoder.try_recv() {
            Ok(Some(Ok(audio_data))) => {
                stream_writer.send_data(Bytes::copy_from_slice(audio_data.data())).await?;
            }
            Ok(None) => tokio::task::yield_now().await,
            Err(()) => break,
        }
    }

    stream_writer.finish().await
}
```

## Critical Observation: ONLY MP3 IS AFFECTED

**All other formats work correctly with HTTP/3**:

| Format | H2 vs H3 |
|--------|----------|
| flac | IDENTICAL |
| **mp3** | **DIFFERS (16996 bytes)** |
| aac | IDENTICAL |
| m4a | IDENTICAL |
| m4a_slow | IDENTICAL |
| opus | IDENTICAL |
| ogg_opus | IDENTICAL |
| webm | IDENTICAL |

This rules out a generic H3/QUIC transport bug. The issue must be related to something MP3-specific:

### MP3-Specific Differences

1. **Decoder output chunk sizes**: MP3 decoder produces output in different patterns than other decoders
2. **First chunk size**: Looking at server logs, MP3 sends a large first chunk (49536 bytes) while other formats send smaller initial chunks
3. **Frame boundaries**: MP3 frame decoding timing may interact poorly with H3 streaming

### Server Output Comparison

**MP3** (affected):
```
chunk 0: 49536 bytes at offset 0      <-- LARGE first chunk
chunk 1: 5760 bytes at offset 49536
chunk 2: 6912 bytes at offset 55296
...
```

**FLAC** (works fine):
```
chunk 0: 4608 bytes at offset 0       <-- Smaller chunks
chunk 1: 9216 bytes at offset 4608
chunk 2: 2304 bytes at offset 13824
...
```

The MP3 decoder produces a much larger initial chunk (49536 bytes = exactly half the output).

## Things NOT Tried Yet

1. **Chunking large sends**: Break up the 49536-byte MP3 chunk into smaller pieces before sending
2. **Try different h3/h3_quinn versions**: Currently using git HEAD
3. **Use shared mutex writer instead of split writer**: Different H3 stream writer implementation exists
4. **Add explicit QUIC stream flush**: Ensure data is flushed after each send

## Dependencies

```toml
# h3 and h3_quinn from git HEAD
h3 = { git = "https://github.com/hyperium/h3.git" }
h3-quinn = { git = "https://github.com/hyperium/h3.git" }
quinn = "0.11"
```

## Test Commands

```bash
# Run HTTP/3 test
cargo test test_http3_decode --release -- --nocapture

# Run HTTP/2 test (works correctly)
cargo test test_http2_decode --release -- --nocapture

# Compare outputs
cmp testdata/golden/http2_mp3.s16le testdata/golden/http3_mp3.s16le

# Play audio
ffplay -f s16le -ar 16000 -ch_layout mono testdata/golden/http3_mp3.s16le
```

## Files to Review

- `/Users/jamie/wavey.ai/media-api/src/router.rs` - Decode streaming handler
- `/Users/jamie/wavey.ai/web-services/web-service/src/h3.rs` - H3 server and stream writers
- `/Users/jamie/wavey.ai/media-api/tests/integration.rs` - Test client using h3/h3_quinn
