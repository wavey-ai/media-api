use bytes::Bytes;
use soundkit_decoder::{DecodeOptions, DecodePipeline, DecodePipelineHandle};

/// Parse decode options from query parameters
pub fn parse_decode_options(query: Option<&str>) -> DecodeOptions {
    let mut options = DecodeOptions::default();

    if let Some(q) = query {
        for pair in q.split('&') {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("");

            match key {
                "sample_rate" | "output_sample_rate" => {
                    if let Ok(rate) = value.parse::<u32>() {
                        options.output_sample_rate = Some(rate);
                    }
                }
                "bits" | "output_bits" | "bits_per_sample" => {
                    if let Ok(bits) = value.parse::<u8>() {
                        if matches!(bits, 16 | 24 | 32) {
                            options.output_bits_per_sample = Some(bits);
                        }
                    }
                }
                "channels" | "output_channels" => {
                    if let Ok(ch) = value.parse::<u8>() {
                        if ch > 0 && ch <= 16 {
                            options.output_channels = Some(ch);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    options
}

/// Create a decode pipeline with the given options
pub fn create_pipeline(options: DecodeOptions) -> DecodePipelineHandle {
    DecodePipeline::spawn_with_options(options)
}

/// Feed a chunk of data to the pipeline
pub fn feed_pipeline(pipeline: &mut DecodePipelineHandle, data: Bytes) -> Result<(), String> {
    pipeline.send(data).map_err(|e| e.to_string())
}

/// Signal end of input to the pipeline
pub fn finish_pipeline(pipeline: &mut DecodePipelineHandle) -> Result<(), String> {
    pipeline.send(Bytes::new()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_decode_options() {
        let opts = parse_decode_options(Some("sample_rate=16000&bits=16&channels=1"));
        assert_eq!(opts.output_sample_rate, Some(16000));
        assert_eq!(opts.output_bits_per_sample, Some(16));
        assert_eq!(opts.output_channels, Some(1));

        let opts = parse_decode_options(Some("output_sample_rate=44100"));
        assert_eq!(opts.output_sample_rate, Some(44100));
        assert_eq!(opts.output_bits_per_sample, None);

        let opts = parse_decode_options(None);
        assert_eq!(opts.output_sample_rate, None);
    }

    #[test]
    fn test_parse_invalid_options() {
        let opts = parse_decode_options(Some("bits=20")); // Invalid bits
        assert_eq!(opts.output_bits_per_sample, None);

        let opts = parse_decode_options(Some("channels=0")); // Invalid channels
        assert_eq!(opts.output_channels, None);

        let opts = parse_decode_options(Some("channels=17")); // Too many channels
        assert_eq!(opts.output_channels, None);
    }
}
