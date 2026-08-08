use std::{fs::File, io::Cursor, path::Path, sync::Arc};

use symphonia::{
    core::{
        codecs::audio::AudioDecoderOptions,
        errors::Error as SymphoniaError,
        formats::{FormatId, FormatOptions, FormatReader, TrackType, probe::Hint, well_known},
        io::{MediaSource, MediaSourceStream},
        meta::MetadataOptions,
    },
    default::{get_codecs, get_probe},
};

use crate::error::DecodeError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    pub max_frames: usize,
    pub max_bytes: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedAudio {
    pub sample_rate: u32,
    pub channels: Vec<Vec<f32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodedAudioFormat {
    Wav,
    Aiff,
    Flac,
    Mp3,
}

impl DecodedAudio {
    pub fn new(sample_rate: u32, channels: Vec<Vec<f32>>) -> Result<Self, DecodeError> {
        let channel_count = channels.len();
        if !(1..=2).contains(&channel_count) {
            return Err(DecodeError::UnsupportedChannels(channel_count));
        }
        if sample_rate == 0
            || channels[0].is_empty()
            || channels
                .iter()
                .any(|channel| channel.len() != channels[0].len())
        {
            return Err(DecodeError::Empty);
        }
        if channels.iter().flatten().any(|sample| !sample.is_finite()) {
            return Err(DecodeError::NonFinite);
        }

        Ok(Self {
            sample_rate,
            channels,
        })
    }

    pub fn frames(&self) -> usize {
        self.channels[0].len()
    }

    pub fn new_with_limits(
        sample_rate: u32,
        channels: Vec<Vec<f32>>,
        limits: DecodeLimits,
    ) -> Result<Self, DecodeError> {
        let frames = channels.first().map_or(0, Vec::len);
        validate_payload_limits(frames, channels.len(), limits)?;
        Self::new(sample_rate, channels)
    }
}

pub fn decode_path(path: &Path) -> Result<DecodedAudio, DecodeError> {
    let source = open_path(path)?;
    decode_source_inner(path, source, None).map(|(decoded, _)| decoded)
}

pub fn decode_path_with_limits(
    path: &Path,
    limits: DecodeLimits,
) -> Result<DecodedAudio, DecodeError> {
    let source = open_path(path)?;
    decode_source_inner(path, source, Some(limits)).map(|(decoded, _)| decoded)
}

/// Decodes an already-read encoded payload using `path` only as a format/error hint.
pub fn decode_bytes_with_limits(
    path: &Path,
    encoded: Vec<u8>,
    limits: DecodeLimits,
) -> Result<DecodedAudio, DecodeError> {
    decode_source_inner(path, Box::new(Cursor::new(encoded)), Some(limits))
        .map(|(decoded, _)| decoded)
}

/// Probes the supported container format from already-read shared bytes.
pub fn probe_shared_audio_format(
    path: &Path,
    encoded: Arc<[u8]>,
) -> Result<EncodedAudioFormat, DecodeError> {
    let format = probe_source(path, Box::new(Cursor::new(encoded)))?;
    supported_audio_format(path, format.format_info().format)
}

/// Decodes already-read shared bytes without copying or opening `path`.
pub fn decode_shared_bytes_with_limits(
    path: &Path,
    encoded: Arc<[u8]>,
    limits: DecodeLimits,
) -> Result<DecodedAudio, DecodeError> {
    decode_source_inner(path, Box::new(Cursor::new(encoded)), Some(limits))
        .map(|(decoded, _)| decoded)
}

fn open_path(path: &Path) -> Result<Box<dyn MediaSource>, DecodeError> {
    File::open(path)
        .map(|source| Box::new(source) as Box<dyn MediaSource>)
        .map_err(|error| DecodeError::Open {
            path: path.to_path_buf(),
            message: error.to_string(),
        })
}

fn decode_source_inner(
    path: &Path,
    source: Box<dyn MediaSource>,
    limits: Option<DecodeLimits>,
) -> Result<(DecodedAudio, FormatId), DecodeError> {
    let mut format = probe_source(path, source)?;
    let encoded_format = format.format_info().format;
    let track =
        format
            .default_track(TrackType::Audio)
            .ok_or_else(|| DecodeError::NoAudioTrack {
                path: path.to_path_buf(),
            })?;
    let track_id = track.id;
    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .ok_or_else(|| DecodeError::UnsupportedCodec {
            path: path.to_path_buf(),
            message: "missing audio codec parameters".to_owned(),
        })?;
    let mut decoder = get_codecs()
        .make_audio_decoder(codec_params, &AudioDecoderOptions::default())
        .map_err(|error| DecodeError::UnsupportedCodec {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;

    let mut sample_rate = None;
    let mut channel_count = None;
    let mut channels = Vec::new();
    let mut packet_samples = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::ResetRequired) => return Err(DecodeError::ChangingFormat),
            Err(error) => {
                return Err(DecodeError::Decode {
                    path: path.to_path_buf(),
                    message: error.to_string(),
                });
            }
        };
        if packet.track_id != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(buffer) => {
                let spec = buffer.spec();
                let count = spec.channels().count();
                if !(1..=2).contains(&count) {
                    return Err(DecodeError::UnsupportedChannels(count));
                }
                if sample_rate.is_some_and(|rate| rate != spec.rate())
                    || channel_count.is_some_and(|expected| expected != count)
                {
                    return Err(DecodeError::ChangingFormat);
                }
                sample_rate = Some(spec.rate());
                channel_count = Some(count);
                if channels.is_empty() {
                    channels = (0..count).map(|_| Vec::new()).collect();
                }

                let packet_sample_count = buffer.samples_interleaved();
                let packet_frames = packet_sample_count / count;
                if let Some(limits) = limits {
                    let decoded_frames = channels[0].len().saturating_add(packet_frames);
                    validate_payload_limits(decoded_frames, count, limits)?;
                }
                packet_samples.resize(packet_sample_count, 0.0);
                buffer.copy_to_slice_interleaved(&mut packet_samples);
                for frame in packet_samples.chunks_exact(count) {
                    for (channel, sample) in channels.iter_mut().zip(frame) {
                        channel.push(*sample);
                    }
                }
            }
            Err(SymphoniaError::ResetRequired) => decoder.reset(),
            Err(error) => {
                return Err(DecodeError::Decode {
                    path: path.to_path_buf(),
                    message: error.to_string(),
                });
            }
        }
    }

    let decoded = match limits {
        Some(limits) => {
            DecodedAudio::new_with_limits(sample_rate.unwrap_or_default(), channels, limits)
        }
        None => DecodedAudio::new(sample_rate.unwrap_or_default(), channels),
    }?;
    Ok((decoded, encoded_format))
}

fn probe_source(
    path: &Path,
    source: Box<dyn MediaSource>,
) -> Result<Box<dyn FormatReader>, DecodeError> {
    let stream = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
        hint.with_extension(&extension.to_ascii_lowercase());
    }

    get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| DecodeError::Probe {
            path: path.to_path_buf(),
            message: error.to_string(),
        })
}

fn supported_audio_format(
    path: &Path,
    format: FormatId,
) -> Result<EncodedAudioFormat, DecodeError> {
    match format {
        well_known::FORMAT_ID_WAVE => Ok(EncodedAudioFormat::Wav),
        well_known::FORMAT_ID_AIFF => Ok(EncodedAudioFormat::Aiff),
        well_known::FORMAT_ID_FLAC => Ok(EncodedAudioFormat::Flac),
        well_known::FORMAT_ID_MP3 => Ok(EncodedAudioFormat::Mp3),
        _ => Err(DecodeError::UnsupportedCodec {
            path: path.to_path_buf(),
            message: format!("unsupported project audio container {format}"),
        }),
    }
}

fn validate_payload_limits(
    frames: usize,
    channels: usize,
    limits: DecodeLimits,
) -> Result<(), DecodeError> {
    if frames > limits.max_frames {
        return Err(DecodeError::FrameLimitExceeded {
            frames,
            max_frames: limits.max_frames,
        });
    }
    let bytes = frames
        .checked_mul(channels)
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>()))
        .unwrap_or(usize::MAX);
    if bytes > limits.max_bytes {
        return Err(DecodeError::ByteLimitExceeded {
            bytes,
            max_bytes: limits.max_bytes,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::path::Path;

    use super::{DecodeLimits, DecodedAudio, decode_bytes_with_limits};
    use crate::DecodeError;

    fn wav_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 48_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let cursor = Cursor::new(&mut bytes);
            let mut writer = hound::WavWriter::new(cursor, spec).unwrap();
            writer.write_sample(0_i16).unwrap();
            writer.write_sample(i16::MAX).unwrap();
            writer.finalize().unwrap();
        }
        bytes
    }

    #[test]
    fn validates_decoded_audio_shape() {
        assert_eq!(
            DecodedAudio::new(48_000, vec![vec![]]).unwrap_err(),
            DecodeError::Empty
        );
        assert_eq!(
            DecodedAudio::new(48_000, vec![vec![0.0], vec![0.0], vec![0.0]]).unwrap_err(),
            DecodeError::UnsupportedChannels(3)
        );
    }

    #[test]
    fn decoded_payload_enforces_independent_frame_and_byte_limits() {
        let over_frames = DecodedAudio::new_with_limits(
            48_000,
            vec![vec![0.0; 3]],
            DecodeLimits {
                max_frames: 2,
                max_bytes: 64,
            },
        );
        assert!(over_frames.is_err());

        let over_bytes = DecodedAudio::new_with_limits(
            48_000,
            vec![vec![0.0; 2], vec![0.0; 2]],
            DecodeLimits {
                max_frames: 2,
                max_bytes: 15,
            },
        );
        assert!(over_bytes.is_err());
    }

    #[test]
    fn encoded_bytes_use_the_logical_path_hint_and_preserve_decode_limits() {
        let encoded = wav_bytes();
        let decoded = decode_bytes_with_limits(
            Path::new("loaded-from-memory.wav"),
            encoded.clone(),
            DecodeLimits {
                max_frames: 2,
                max_bytes: 8,
            },
        )
        .unwrap();
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.frames(), 2);

        let error = decode_bytes_with_limits(
            Path::new("loaded-from-memory.wav"),
            encoded,
            DecodeLimits {
                max_frames: 1,
                max_bytes: 8,
            },
        )
        .unwrap_err();
        assert!(matches!(error, DecodeError::FrameLimitExceeded { .. }));
    }
}
