use crate::config::MediaApiConfig;
use crate::decode::create_pipeline;
use async_trait::async_trait;
use bytes::Bytes;
use frame_header::{EncodingFlag, Endianness, FrameHeader};
use soundkit_decoder::DecodeOptions;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use web_service::traits::RawStream;
use web_service::{HandlerResult, RawTcpHandler, ServerError};

pub struct DecodeTcpHandler {
    config: Arc<MediaApiConfig>,
}

impl DecodeTcpHandler {
    pub fn new(config: Arc<MediaApiConfig>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl RawTcpHandler for DecodeTcpHandler {
    async fn handle_stream(
        &self,
        mut stream: Box<dyn RawStream>,
        _is_tls: bool,
    ) -> HandlerResult<()> {
        let options = DecodeOptions {
            output_sample_rate: self.config.default_output_sample_rate,
            output_bits_per_sample: self.config.default_output_bits,
            output_channels: self.config.default_output_channels,
        };

        let mut pipeline = create_pipeline(options);

        // Read loop: read frame-header framed input
        let mut header_buf = [0u8; 20]; // Max header size (4 + 8 + 8)

        loop {
            // Read the base header (4 bytes)
            match stream.read_exact(&mut header_buf[..4]).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // EOF - signal pipeline and flush
                    let _ = pipeline.send(Bytes::new());
                    break;
                }
                Err(e) => {
                    tracing::error!("TCP read error: {}", e);
                    return Err(ServerError::Handler(Box::new(e)));
                }
            }

            // Validate and decode header
            let header = match FrameHeader::decode(&mut &header_buf[..4]) {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("Invalid frame header: {}", e);
                    // Try to recover by reading raw data
                    // For now, just continue
                    continue;
                }
            };

            // Read optional ID and PTS if present
            let mut extra_bytes = 0;
            if header.id().is_some() {
                extra_bytes += 8;
            }
            if header.pts().is_some() {
                extra_bytes += 8;
            }

            if extra_bytes > 0 {
                if let Err(e) = stream.read_exact(&mut header_buf[4..4 + extra_bytes]).await {
                    tracing::error!("TCP read error (header extension): {}", e);
                    return Err(ServerError::Handler(Box::new(e)));
                }
            }

            // Calculate payload size
            let bytes_per_sample = header.bits_per_sample() as usize / 8;
            let payload_size =
                header.sample_size() as usize * bytes_per_sample * header.channels() as usize;

            if payload_size == 0 {
                // EOF marker (zero-size frame)
                let _ = pipeline.send(Bytes::new());
                break;
            }

            // Read payload
            let mut payload = vec![0u8; payload_size];
            if let Err(e) = stream.read_exact(&mut payload).await {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    let _ = pipeline.send(Bytes::new());
                    break;
                }
                tracing::error!("TCP read error (payload): {}", e);
                return Err(ServerError::Handler(Box::new(e)));
            }

            // Send to pipeline
            if let Err(e) = pipeline.send(Bytes::from(payload)) {
                tracing::warn!("Pipeline send error: {}", e);
                break;
            }

            // Drain and send output
            while let Some(output) = pipeline.try_recv() {
                match output {
                    Ok(audio_data) => {
                        if let Err(e) = write_audio_frame(&mut stream, &audio_data).await {
                            tracing::error!("TCP write error: {}", e);
                            return Err(e);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Decode error: {}", e);
                    }
                }
            }
        }

        // Flush remaining output
        let timeout = std::time::Duration::from_secs(5);
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                break;
            }

            match pipeline.try_recv() {
                Some(Ok(audio_data)) => {
                    if let Err(e) = write_audio_frame(&mut stream, &audio_data).await {
                        tracing::error!("TCP write error during flush: {}", e);
                        break;
                    }
                }
                Some(Err(e)) => {
                    tracing::warn!("Decode error during flush: {}", e);
                }
                None => {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            }
        }

        // Send EOF frame (zero-size)
        let eof_header = FrameHeader::new(
            EncodingFlag::PCMSigned,
            0,
            48000,
            1,
            16,
            Endianness::LittleEndian,
            None,
            None,
        )
        .ok();

        if let Some(header) = eof_header {
            let mut buf = Vec::with_capacity(4);
            if header.encode(&mut buf).is_ok() {
                let _ = stream.write_all(&buf).await;
            }
        }

        let _ = stream.flush().await;

        Ok(())
    }
}

async fn write_audio_frame(
    stream: &mut Box<dyn RawStream>,
    audio_data: &soundkit::audio_types::AudioData,
) -> HandlerResult<()> {
    let sample_count = audio_data.data().len()
        / (audio_data.bits_per_sample() as usize / 8)
        / audio_data.channel_count() as usize;

    // frame-header only supports up to 12-bit sample counts (0xFFF)
    // For larger frames, we need to split them
    let max_samples = 0xFFF;

    if sample_count <= max_samples {
        write_single_frame(stream, audio_data, sample_count as u16).await
    } else {
        // Split into multiple frames
        let bytes_per_sample =
            audio_data.bits_per_sample() as usize / 8 * audio_data.channel_count() as usize;
        let data = audio_data.data();
        let mut offset = 0;

        while offset < data.len() {
            let remaining_samples = (data.len() - offset) / bytes_per_sample;
            let chunk_samples = remaining_samples.min(max_samples);
            let chunk_bytes = chunk_samples * bytes_per_sample;

            let header = FrameHeader::new(
                audio_data.audio_format(),
                chunk_samples as u16,
                audio_data.sampling_rate(),
                audio_data.channel_count(),
                audio_data.bits_per_sample(),
                Endianness::LittleEndian,
                None,
                None,
            )
            .map_err(|e| {
                ServerError::Handler(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e,
                )))
            })?;

            let mut buf = Vec::with_capacity(header.size() + chunk_bytes);
            header.encode(&mut buf).map_err(|e| {
                ServerError::Handler(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e,
                )))
            })?;
            buf.extend_from_slice(&data[offset..offset + chunk_bytes]);

            stream.write_all(&buf).await.map_err(|e| {
                ServerError::Handler(Box::new(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    e,
                )))
            })?;

            offset += chunk_bytes;
        }

        Ok(())
    }
}

async fn write_single_frame(
    stream: &mut Box<dyn RawStream>,
    audio_data: &soundkit::audio_types::AudioData,
    sample_count: u16,
) -> HandlerResult<()> {
    let header = FrameHeader::new(
        audio_data.audio_format(),
        sample_count,
        audio_data.sampling_rate(),
        audio_data.channel_count(),
        audio_data.bits_per_sample(),
        Endianness::LittleEndian,
        None,
        None,
    )
    .map_err(|e| {
        ServerError::Handler(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e,
        )))
    })?;

    let mut buf = Vec::with_capacity(header.size() + audio_data.data().len());
    header.encode(&mut buf).map_err(|e| {
        ServerError::Handler(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            e,
        )))
    })?;
    buf.extend_from_slice(audio_data.data());

    stream.write_all(&buf).await.map_err(|e| {
        ServerError::Handler(Box::new(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            e,
        )))
    })?;

    Ok(())
}
