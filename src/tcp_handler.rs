use crate::pool::DecoderPool;
use bytes::Bytes;
use frame_header::EncodingFlag;
use soundkit_decoder::DecodeOptions;
use std::sync::Arc;
use tcp_changes::framing::{read_frame, write_frame, tags};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{error, info, warn};

/// TCP decode server using tcp-changes framing protocol.
///
/// Wire format: [4B length][4B tag][data]
/// - DCOD: client sends encoded audio chunks
/// - PCMS: server sends decoded PCM chunks
/// - DONE: signals end of stream
/// - META: audio metadata (sample_rate:u32, channels:u8, bits:u8)
pub struct TcpDecodeServer {
    decoder_pool: Arc<DecoderPool>,
    options: DecodeOptions,
    tls_acceptor: Option<TlsAcceptor>,
}

impl TcpDecodeServer {
    pub fn new(
        decoder_pool: Arc<DecoderPool>,
        options: DecodeOptions,
        tls_acceptor: Option<TlsAcceptor>,
    ) -> Self {
        Self {
            decoder_pool,
            options,
            tls_acceptor,
        }
    }

    /// Start the TCP server on the given address.
    pub async fn start(
        self: Arc<Self>,
        addr: std::net::SocketAddr,
        mut shutdown: tokio::sync::watch::Receiver<()>,
    ) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        info!("TCP decode server listening on {}", addr);

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer)) => {
                            let server = Arc::clone(&self);
                            tokio::spawn(async move {
                                if let Err(e) = server.handle_connection(stream, peer).await {
                                    warn!("Connection error from {}: {}", peer, e);
                                }
                            });
                        }
                        Err(e) => {
                            error!("Accept error: {}", e);
                        }
                    }
                }
                _ = shutdown.changed() => {
                    info!("TCP decode server shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn handle_connection(
        &self,
        stream: TcpStream,
        peer: std::net::SocketAddr,
    ) -> std::io::Result<()> {
        info!("New TCP connection from {}", peer);

        if let Some(ref acceptor) = self.tls_acceptor {
            let tls_stream = acceptor.accept(stream).await?;
            let (reader, writer) = tokio::io::split(tls_stream);
            self.handle_decode_stream(reader, writer).await
        } else {
            let (reader, writer) = tokio::io::split(stream);
            self.handle_decode_stream(reader, writer).await
        }
    }

    async fn handle_decode_stream<R, W>(&self, mut reader: R, mut writer: W) -> std::io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        // Persistent connection - handle multiple decode sessions
        loop {
            // Wait for first frame to start a new session
            let first_frame = match read_frame(&mut reader).await? {
                Some(f) => f,
                None => {
                    // EOF - client closed connection
                    return Ok(());
                }
            };

            // Must be DCOD to start a session
            if !first_frame.tag_matches(tags::DCOD) {
                warn!("Expected DCOD to start session, got {:?}", first_frame.tag);
                continue;
            }

            // Acquire decoder from pool for this session
            let decoder = self.decoder_pool.acquire(self.options.clone()).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, e)
            })?;

            let mut first_meta_sent = false;
            let mut session_done = false;

            // Process first frame
            if !first_frame.data.is_empty() {
                if let Err(e) = decoder.send(first_frame.data) {
                    warn!("Decoder send error: {}", e);
                    session_done = true;
                }
            } else {
                // Empty DCOD signals end of input immediately
                let _ = decoder.send(Bytes::new());
                session_done = true;
            }

            // Read frames for this session
            while !session_done {
                // Drain available output before reading next frame
                loop {
                    match decoder.try_recv() {
                        Ok(Some(Ok(audio_data))) => {
                            if !first_meta_sent {
                                let encoding = match audio_data.audio_format() {
                                    EncodingFlag::PCMFloat => PcmEncoding::Float,
                                    _ => PcmEncoding::Signed,
                                };
                                let meta = encode_meta(
                                    audio_data.sampling_rate(),
                                    audio_data.channel_count(),
                                    audio_data.bits_per_sample(),
                                    encoding,
                                );
                                write_frame(&mut writer, tags::META, &meta).await?;
                                first_meta_sent = true;
                            }
                            write_frame(&mut writer, tags::PCMS, audio_data.data()).await?;
                        }
                        Ok(Some(Err(e))) => {
                            warn!("Decode error: {}", e);
                        }
                        Ok(None) => break,
                        Err(()) => break,
                    }
                }

                let frame = match read_frame(&mut reader).await? {
                    Some(f) => f,
                    None => {
                        // EOF - signal decoder and end
                        let _ = decoder.send(Bytes::new());
                        break;
                    }
                };

                if frame.tag_matches(tags::DCOD) {
                    if frame.data.is_empty() {
                        // Empty DCOD signals end of input
                        let _ = decoder.send(Bytes::new());
                        session_done = true;
                    } else {
                        if let Err(e) = decoder.send(frame.data) {
                            warn!("Decoder send error: {}", e);
                            session_done = true;
                        }
                    }
                } else if frame.tag_matches(tags::DONE) {
                    // Client signaling end
                    let _ = decoder.send(Bytes::new());
                    session_done = true;
                } else {
                    warn!("Unknown tag: {:?}", frame.tag);
                }
            }

            // Flush remaining output for this session
            loop {
                match decoder.recv() {
                    Some(Ok(audio_data)) => {
                        if !first_meta_sent {
                            let encoding = match audio_data.audio_format() {
                                EncodingFlag::PCMFloat => PcmEncoding::Float,
                                _ => PcmEncoding::Signed,
                            };
                            let meta = encode_meta(
                                audio_data.sampling_rate(),
                                audio_data.channel_count(),
                                audio_data.bits_per_sample(),
                                encoding,
                            );
                            write_frame(&mut writer, tags::META, &meta).await?;
                            first_meta_sent = true;
                        }
                        write_frame(&mut writer, tags::PCMS, audio_data.data()).await?;
                    }
                    Some(Err(e)) => {
                        warn!("Decode error during flush: {}", e);
                    }
                    None => break,
                }
            }

            // Send DONE signal for this session
            write_frame(&mut writer, tags::DONE, &[]).await?;

            // Continue loop to handle next session on same connection
        }
    }
}

/// Encoding type for PCM data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PcmEncoding {
    Signed = 0,
    Float = 1,
}

/// Encode audio metadata: sample_rate (4 bytes) + channels (1 byte) + bits (1 byte) + encoding (1 byte)
fn encode_meta(sample_rate: u32, channels: u8, bits: u8, encoding: PcmEncoding) -> [u8; 7] {
    let mut buf = [0u8; 7];
    buf[0..4].copy_from_slice(&sample_rate.to_be_bytes());
    buf[4] = channels;
    buf[5] = bits;
    buf[6] = encoding as u8;
    buf
}

/// Decode audio metadata
#[allow(dead_code)]
pub fn decode_meta(data: &[u8]) -> Option<(u32, u8, u8, PcmEncoding)> {
    if data.len() < 7 {
        return None;
    }
    let sample_rate = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let channels = data[4];
    let bits = data[5];
    let encoding = match data[6] {
        0 => PcmEncoding::Signed,
        1 => PcmEncoding::Float,
        _ => PcmEncoding::Signed,
    };
    Some((sample_rate, channels, bits, encoding))
}
