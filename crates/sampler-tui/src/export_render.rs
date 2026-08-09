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
        });
        for frame in &chunk[..written] {
            peak[0] = peak[0].max(frame[0].abs());
            peak[1] = peak[1].max(frame[1].abs());
        }
        sink.write_frames(&chunk[..written])?;
        remaining -= written;
    }

    Ok(OfflineRenderSummary {
        frame_count: total,
        peak,
    })
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
    let mut engine = AudioEngine::new(EXPORT_SAMPLE_RATE, ports)
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

    controller
        .update_master_mix(snapshot.master_mix())
        .map_err(|_| OfflineExportError::RendererUnavailable)?;
    engine.render_frames(0, |_| {});

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
