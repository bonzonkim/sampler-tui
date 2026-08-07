use std::{fs::File, path::Path};

use symphonia::{
    core::{
        codecs::audio::AudioDecoderOptions,
        errors::Error as SymphoniaError,
        formats::{FormatOptions, TrackType, probe::Hint},
        io::MediaSourceStream,
        meta::MetadataOptions,
    },
    default::{get_codecs, get_probe},
};

use crate::error::DecodeError;

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedAudio {
    pub sample_rate: u32,
    pub channels: Vec<Vec<f32>>,
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
}

pub fn decode_path(path: &Path) -> Result<DecodedAudio, DecodeError> {
    let source = File::open(path).map_err(|error| DecodeError::Open {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let stream = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
        hint.with_extension(&extension.to_ascii_lowercase());
    }

    let mut format = get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|error| DecodeError::Probe {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
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

                packet_samples.resize(buffer.samples_interleaved(), 0.0);
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

    DecodedAudio::new(sample_rate.unwrap_or_default(), channels)
}

#[cfg(test)]
mod tests {
    use super::DecodedAudio;
    use crate::DecodeError;

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
}
