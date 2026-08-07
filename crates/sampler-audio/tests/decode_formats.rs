use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use sampler_audio::{DecodeError, decode_path};

static FIXTURE_ID: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    path: PathBuf,
}

impl Fixture {
    fn new(extension: &str) -> Self {
        let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sampler-audio-decode-{}-{id}.{extension}",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn wav_fixture(sample_rate: u32, channels: u16, samples: &[i16]) -> Fixture {
    let fixture = Fixture::new("wav");
    let mut writer = hound::WavWriter::create(
        fixture.path(),
        hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .unwrap();
    for &sample in samples {
        writer.write_sample(sample).unwrap();
    }
    writer.finalize().unwrap();
    fixture
}

fn byte_fixture(name: &str, bytes: &[u8]) -> Fixture {
    let fixture = Fixture::new(name);
    fs::write(fixture.path(), bytes).unwrap();
    fixture
}

#[test]
fn decodes_mono_wav_without_changing_channel_count() {
    let fixture = wav_fixture(44_100, 1, &[0_i16, i16::MAX, i16::MIN]);
    let decoded = decode_path(fixture.path()).unwrap();
    assert_eq!(decoded.sample_rate, 44_100);
    assert_eq!(decoded.channels.len(), 1);
    assert_eq!(decoded.frames(), 3);
    assert!(decoded.channels[0][1] > 0.99);
}

#[test]
fn preserves_stereo_channel_order() {
    let fixture = wav_fixture(48_000, 2, &[i16::MAX, 0, 0, i16::MIN]);
    let decoded = decode_path(fixture.path()).unwrap();
    assert_eq!(decoded.channels.len(), 2);
    assert!(decoded.channels[0][0] > 0.99);
    assert!(decoded.channels[1][1] < -0.99);
}

#[test]
fn corrupt_input_returns_a_probe_or_decode_error() {
    let fixture = byte_fixture("corrupt.mp3", b"not audio");
    assert!(matches!(
        decode_path(fixture.path()),
        Err(DecodeError::Probe { .. })
    ));
}
