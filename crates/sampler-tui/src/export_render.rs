use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sampler_audio::{AudioController, AudioEngine, PatternSwitch, audio_channels};

use crate::export::{
    EXPORT_CHUNK_FRAMES, EXPORT_SAMPLE_RATE, OfflineExportError, OfflineExportSnapshot,
    StagedExportPad,
};

/// Receives bounded stereo chunks from the offline production-engine renderer.
pub trait OfflineFrameSink {
    fn write_frames(&mut self, frames: &[[f32; 2]]) -> Result<(), OfflineExportError>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OfflineRenderSummary {
    pub frame_count: u64,
    pub peak: [f32; 2],
}

/// Renders exactly one loop from fresh production-engine state without retaining the full render.
pub fn render_offline(
    snapshot: &OfflineExportSnapshot,
    staged: &[StagedExportPad],
    sink: &mut dyn OfflineFrameSink,
    cancelled: &AtomicBool,
) -> Result<OfflineRenderSummary, OfflineExportError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(OfflineExportError::Cancelled);
    }
    validate_staged(snapshot, staged)?;
    let (_controller, mut engine) = prepare_engine(snapshot, staged, cancelled)?;
    let total = snapshot.loop_frames()?;
    let mut remaining = usize::try_from(total).map_err(|_| OfflineExportError::Arithmetic)?;
    let mut chunk = [[0.0_f32; 2]; EXPORT_CHUNK_FRAMES];
    let mut peak = [0.0_f32; 2];

    while remaining != 0 {
        if cancelled.load(Ordering::Acquire) {
            return Err(OfflineExportError::Cancelled);
        }
        let count = remaining.min(EXPORT_CHUNK_FRAMES);
        let mut written = 0;
        engine.render_frames(count, |frame| {
            chunk[written] = frame;
            written += 1;
            #[cfg(test)]
            observe_test_rendered_frame(written, cancelled);
        });
        for frame in &chunk[..written] {
            peak[0] = peak[0].max(frame[0].abs());
            peak[1] = peak[1].max(frame[1].abs());
        }
        if cancelled.load(Ordering::Acquire) {
            return Err(OfflineExportError::Cancelled);
        }
        sink.write_frames(&chunk[..written])?;
        remaining -= written;
    }

    Ok(OfflineRenderSummary {
        frame_count: total,
        peak,
    })
}

#[cfg(test)]
thread_local! {
    static TEST_CANCEL_AFTER_RENDERED_FRAME: std::cell::Cell<Option<usize>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
fn observe_test_rendered_frame(written: usize, cancelled: &AtomicBool) {
    TEST_CANCEL_AFTER_RENDERED_FRAME.with(|target| {
        if target.get() == Some(written) {
            cancelled.store(true, Ordering::Release);
        }
    });
}

fn validate_staged(
    snapshot: &OfflineExportSnapshot,
    staged: &[StagedExportPad],
) -> Result<(), OfflineExportError> {
    if staged.len() != snapshot.pads().len() {
        return Err(OfflineExportError::RendererUnavailable);
    }
    for expected in snapshot.pads() {
        let mut matches = staged
            .iter()
            .filter(|candidate| candidate.pad == expected.pad);
        let Some(candidate) = matches.next() else {
            return Err(OfflineExportError::RendererUnavailable);
        };
        if matches.next().is_some()
            || candidate.sample.sample_rate() != EXPORT_SAMPLE_RATE
            || candidate.settings != expected.settings
            || candidate.mix != expected.mix
        {
            return Err(OfflineExportError::RendererUnavailable);
        }
    }
    Ok(())
}

fn prepare_engine(
    snapshot: &OfflineExportSnapshot,
    staged: &[StagedExportPad],
    cancelled: &AtomicBool,
) -> Result<(AudioController, AudioEngine), OfflineExportError> {
    let (mut controller, ports) = audio_channels();
    let mut engine =
        AudioEngine::new_with_master_mix(EXPORT_SAMPLE_RATE, ports, snapshot.master_mix())
            .map_err(|_| OfflineExportError::RendererUnavailable)?;

    // Consume each install at frame zero so the ordinary bounded controller queue also supports
    // a snapshot referencing every pad without adding a second bootstrap or DSP path.
    for expected in snapshot.pads() {
        if cancelled.load(Ordering::Acquire) {
            return Err(OfflineExportError::Cancelled);
        }
        let staged = staged
            .iter()
            .find(|candidate| candidate.pad == expected.pad)
            .ok_or(OfflineExportError::RendererUnavailable)?;
        controller
            .install(
                staged.pad,
                Arc::clone(&staged.sample),
                staged.settings,
                staged.mix,
            )
            .map_err(|_| OfflineExportError::RendererUnavailable)?;
        engine.render_frames(0, |_| {});
    }

    let editable = snapshot
        .pattern()
        .to_editable()
        .map_err(|_| OfflineExportError::PatternCompile(snapshot.slot()))?;
    let pattern = editable
        .compile()
        .map_err(|_| OfflineExportError::PatternCompile(snapshot.slot()))?;
    controller
        .install_pattern(Arc::new(pattern))
        .map_err(|_| OfflineExportError::RendererUnavailable)?;
    controller
        .select_pattern(snapshot.slot(), PatternSwitch::Immediate)
        .map_err(|_| OfflineExportError::RendererUnavailable)?;
    controller
        .play_pattern()
        .map_err(|_| OfflineExportError::RendererUnavailable)?;
    engine.render_frames(0, |_| {});

    Ok((controller, engine))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sampler_audio::SampleBuffer;
    use sampler_core::{
        AssetDigest, EditablePattern, MasterMixSettings, Meter, PadMixSettings, PadSettings,
        PatternSlotId, ProjectId, ProjectPattern, Resolution, SampleEditRecipe, Tempo, Transport,
    };

    use crate::{ProjectSavePad, SourceFingerprint, SupportedAudioExtension};

    use super::*;

    struct CountingSink {
        writes: usize,
    }

    impl OfflineFrameSink for CountingSink {
        fn write_frames(&mut self, _frames: &[[f32; 2]]) -> Result<(), OfflineExportError> {
            self.writes += 1;
            Ok(())
        }
    }

    fn fixture() -> (OfflineExportSnapshot, Vec<StagedExportPad>) {
        let pad = sampler_core::PadId::first();
        let transport = Transport::new(
            EXPORT_SAMPLE_RATE,
            Tempo::new(240.0).unwrap(),
            Meter::new(1, 4).unwrap(),
            1,
            Resolution::Sixteenth,
        )
        .unwrap();
        let mut editable =
            EditablePattern::new(PatternSlotId::new(0).unwrap(), "cancel", transport).unwrap();
        editable.insert_new(pad, 0, 1.0, None).unwrap();
        let pattern = ProjectPattern::from_editable(&editable).unwrap();
        let settings = PadSettings::default();
        let mix = PadMixSettings::default();
        let snapshot = OfflineExportSnapshot::new(
            ProjectId::from_bytes([4; 16]),
            1,
            editable.slot(),
            pattern,
            vec![ProjectSavePad {
                pad,
                source_path: PathBuf::from("audio/cancel.wav"),
                source_generation: 1,
                fingerprint: SourceFingerprint {
                    digest: AssetDigest::from_bytes([4; 32]),
                    encoded_bytes: 1,
                    extension: SupportedAudioExtension::Wav,
                },
                settings,
                mix,
                recipe: SampleEditRecipe::identity(),
            }],
            MasterMixSettings::default(),
            EXPORT_SAMPLE_RATE,
        )
        .unwrap();
        let staged = vec![StagedExportPad {
            pad,
            sample: Arc::new(SampleBuffer::new(EXPORT_SAMPLE_RATE, vec![0.5, -0.5]).unwrap()),
            settings,
            mix,
        }];
        (snapshot, staged)
    }

    #[test]
    fn cancellation_during_chunk_rendering_writes_no_part_of_that_chunk() {
        let (snapshot, staged) = fixture();
        let cancelled = AtomicBool::new(false);
        let mut sink = CountingSink { writes: 0 };
        TEST_CANCEL_AFTER_RENDERED_FRAME.with(|target| target.set(Some(1)));

        let result = render_offline(&snapshot, &staged, &mut sink, &cancelled);

        TEST_CANCEL_AFTER_RENDERED_FRAME.with(|target| target.set(None));
        assert_eq!(result, Err(OfflineExportError::Cancelled));
        assert_eq!(sink.writes, 0);
    }
}
