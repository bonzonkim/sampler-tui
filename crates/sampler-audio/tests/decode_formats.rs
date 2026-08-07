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

fn hex_fixture(extension: &str, hex: &str) -> Fixture {
    let bytes = hex
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    byte_fixture(extension, &bytes)
}

const AIFF_MONO_8K: &str = concat!(
    "464f524d000000ae41494646434f4d4d000000120001000000400010400bfa000000000000005353",
    "4e440000008800000000000000002ee02ee02ee02ee02ee02ee02ee02ee0d120d120d120d120d120",
    "d120d120d1202ee02ee02ee02ee02ee02ee02ee02ee0d120d120d120d120d120d120d120d1202ee0",
    "2ee02ee02ee02ee02ee02ee02ee0d120d120d120d120d120d120d120d1202ee02ee02ee02ee02ee0",
    "2ee02ee02ee0d120d120d120d120d120d120d120d120",
);

const FLAC_MONO_8K: &str = concat!(
    "664c6143000000220040004000003700003701f400f000000040e70a3b8c6676f736d5d5de649e17",
    "e3cf84000028200000007265666572656e6365206c6962464c414320312e342e3320323032333036",
    "323300000000fff86408003f5e1309771421861c9edc001861c9ee4001861c9edc001861c9ee4001",
    "861c9edc001861c9ee4001861c9edc00186180b769",
);

const MP3_MONO_8K: &str = concat!(
    "ffe318c4000c48a2d878084c0e0544131318c7f8000f98c63f80064639e4c0000082018c1f0fea04",
    "3cffff83ff2e0f82018e27fffffe53cb83e083bfe5cfaaf940f0868439084217ffe318c4090cf88e",
    "e9b808460c984215e42109faa80808082cb05415cb034fc15057fffc447b582a0ac4a0abb11035ff",
    "ffff582a7560a82b56b2ca86cac0c10347412c14103040e0ffe318c41009b8a99c000006047c1616",
    "16070042c2c2c6fffff50b35002161669b182c2c4c414d45332e313030aaaaaaaaaaaaaaaaaaaaaa",
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
);

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
fn decodes_embedded_aiff_fixture() {
    let fixture = hex_fixture("aiff", AIFF_MONO_8K);
    let decoded = decode_path(fixture.path()).unwrap();

    assert_eq!(decoded.sample_rate, 8_000);
    assert_eq!(decoded.channels.len(), 1);
    assert_eq!(decoded.frames(), 64);
    assert!(decoded.channels[0].iter().all(|sample| sample.is_finite()));
    assert!((decoded.channels[0][0] - 12_000.0 / 32_768.0).abs() < 1.0e-4);
    assert!((decoded.channels[0][8] + 12_000.0 / 32_768.0).abs() < 1.0e-4);
}

#[test]
fn decodes_embedded_flac_fixture() {
    let fixture = hex_fixture("flac", FLAC_MONO_8K);
    let decoded = decode_path(fixture.path()).unwrap();

    assert_eq!(decoded.sample_rate, 8_000);
    assert_eq!(decoded.channels.len(), 1);
    assert_eq!(decoded.frames(), 64);
    assert!(decoded.channels[0].iter().all(|sample| sample.is_finite()));
    assert!((decoded.channels[0][0] - 12_000.0 / 32_768.0).abs() < 1.0e-4);
    assert!((decoded.channels[0][8] + 12_000.0 / 32_768.0).abs() < 1.0e-4);
}

#[test]
fn decodes_embedded_mp3_fixture_with_lossy_tolerance() {
    let fixture = hex_fixture("mp3", MP3_MONO_8K);
    let decoded = decode_path(fixture.path()).unwrap();

    assert_eq!(decoded.sample_rate, 8_000);
    assert_eq!(decoded.channels.len(), 1);
    assert_eq!(decoded.frames(), 1_728);
    assert!(decoded.channels[0].iter().all(|sample| sample.is_finite()));
    assert!(decoded.channels[0].iter().any(|sample| sample.abs() > 0.1));
}

#[test]
fn corrupt_input_returns_a_probe_or_decode_error() {
    let fixture = byte_fixture("corrupt.mp3", b"not audio");
    assert!(matches!(
        decode_path(fixture.path()),
        Err(DecodeError::Probe { .. })
    ));
}
