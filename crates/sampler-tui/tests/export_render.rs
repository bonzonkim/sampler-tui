use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sampler_audio::{AudioEngine, PatternSwitch, SampleBuffer, audio_channels};
use sampler_core::{
    AssetDigest, BankId, ChokeGroup, DelaySettings, EditablePattern, MasterMixSettings, Meter,
    PadId, PadMixSettings, PadSettings, PatternSlotId, PlaybackMode, ProjectId, ProjectPattern,
    Resolution, ReverbSettings, SAMPLE_PHASE_SCALE, SampleEditRecipe, Tempo, Transport,
    apply_sample_edit,
};
use sampler_tui::export::StagedExportPad;
use sampler_tui::export_render::{OfflineFrameSink, OfflineRenderSummary, render_offline};
use sampler_tui::{
    EXPORT_CHUNK_FRAMES, EXPORT_SAMPLE_RATE, OfflineExportError, OfflineExportSnapshot,
    ProjectSavePad, SourceFingerprint, SupportedAudioExtension,
};

fn pad(index: u8) -> PadId {
    PadId::new(BankId::new(0).unwrap(), index).unwrap()
}

fn staged_pad(
    index: u8,
    sample: Arc<SampleBuffer>,
    settings: PadSettings,
    mix: PadMixSettings,
) -> StagedExportPad {
    StagedExportPad {
        pad: pad(index),
        sample,
        settings,
        mix,
    }
}

fn descriptor(staged: &StagedExportPad) -> ProjectSavePad {
    ProjectSavePad {
        pad: staged.pad,
        source_path: PathBuf::from(format!("audio/pad-{}.wav", staged.pad.index())),
        source_generation: 1,
        fingerprint: SourceFingerprint {
            digest: AssetDigest::from_bytes([staged.pad.index(); 32]),
            encoded_bytes: 1,
            extension: SupportedAudioExtension::Wav,
        },
        settings: staged.settings,
        mix: staged.mix,
        recipe: SampleEditRecipe::identity(),
    }
}

fn stereo_sample(frames: usize, left: f32, right: f32) -> Arc<SampleBuffer> {
    let mut stereo = Vec::with_capacity(frames * 2);
    for index in 0..frames {
        let phase = (index % 31) as f32 / 31.0;
        stereo.extend_from_slice(&[left * (0.5 + phase * 0.5), right * (1.0 - phase * 0.25)]);
    }
    Arc::new(SampleBuffer::new(EXPORT_SAMPLE_RATE, stereo).unwrap())
}

fn reversed_normalized_sample() -> Arc<SampleBuffer> {
    let mut source = Vec::with_capacity(512 * 2);
    for index in 0..512 {
        let amplitude = (index + 1) as f32 / 2_048.0;
        source.extend_from_slice(&[amplitude, -2.0 * amplitude]);
    }
    let plan = apply_sample_edit(
        EXPORT_SAMPLE_RATE,
        &source,
        SampleEditRecipe::new(0, SAMPLE_PHASE_SCALE, true, true).unwrap(),
    )
    .unwrap();
    assert_eq!(&plan.data()[..2], &[0.44562545, -0.8912509]);
    Arc::new(SampleBuffer::new(EXPORT_SAMPLE_RATE, plan.into_stereo()).unwrap())
}

fn fixture(wet: bool) -> (OfflineExportSnapshot, Vec<StagedExportPad>) {
    let choke = ChokeGroup::new(1).unwrap();
    let staged = vec![
        staged_pad(
            0,
            stereo_sample(8_192, 0.65, -0.45),
            PadSettings::new(PlaybackMode::OneShot, -3.0, -0.75, 5.0, Some(choke)).unwrap(),
            PadMixSettings::new(false, 0.35, 0.15).unwrap(),
        ),
        staged_pad(
            1,
            stereo_sample(768, -0.4, 0.7),
            PadSettings::new(PlaybackMode::OneShot, -6.0, 0.75, -4.0, Some(choke)).unwrap(),
            PadMixSettings::new(false, 0.2, 0.4).unwrap(),
        ),
        staged_pad(
            2,
            stereo_sample(1_500, 0.5, 0.25),
            PadSettings::new(PlaybackMode::Gate, -1.0, -0.2, 2.0, None).unwrap(),
            PadMixSettings::new(false, 0.0, 0.25).unwrap(),
        ),
        staged_pad(
            3,
            stereo_sample(193, -0.35, -0.6),
            PadSettings::new(PlaybackMode::Loop, -4.0, 0.25, -7.0, None).unwrap(),
            PadMixSettings::new(false, 0.45, 0.0).unwrap(),
        ),
        staged_pad(
            4,
            reversed_normalized_sample(),
            PadSettings::new(PlaybackMode::OneShot, -2.0, 0.0, 0.0, None).unwrap(),
            PadMixSettings::new(false, 0.8, 0.8).unwrap(),
        ),
    ];

    let transport = Transport::new(
        EXPORT_SAMPLE_RATE,
        Tempo::new(240.0).unwrap(),
        Meter::new(1, 4).unwrap(),
        1,
        Resolution::Sixteenth,
    )
    .unwrap()
    .with_swing(0.68)
    .unwrap();
    assert_eq!(transport.loop_frames(), 12_000);
    let mut editable =
        EditablePattern::new(PatternSlotId::new(2).unwrap(), "offline parity", transport).unwrap();
    editable.set_quantize_strength(0.75).unwrap();
    editable.insert_new(pad(0), 341, 0.82, None).unwrap();
    editable.insert_new(pad(1), 3_221, 0.61, None).unwrap();
    editable.insert_new(pad(2), 4_511, 0.47, Some(900)).unwrap();
    editable
        .insert_new(pad(3), 6_401, 0.73, Some(1_100))
        .unwrap();
    editable
        .insert_new(pad(4), transport.loop_frames() - 40, 0.9, None)
        .unwrap();
    let pattern = ProjectPattern::from_editable(&editable).unwrap();
    assert_eq!(pattern.swing, 0.68);
    assert_eq!(pattern.quantize_strength, 0.75);
    assert!(pattern.events.iter().any(|event| {
        event.raw_frame != event.event.frame && event.event.original_offset.is_some()
    }));

    let master_mix = if wet {
        MasterMixSettings::new(
            -1.5,
            DelaySettings::new(true, 10, 0.55, -2.0).unwrap(),
            ReverbSettings::new(true, 0.8, 0.25, -3.0).unwrap(),
        )
        .unwrap()
    } else {
        MasterMixSettings::new(-3.0, DelaySettings::default(), ReverbSettings::default()).unwrap()
    };
    let descriptors = staged.iter().map(descriptor).collect();
    let snapshot = OfflineExportSnapshot::new(
        ProjectId::from_bytes([9; 16]),
        17,
        editable.slot(),
        pattern,
        descriptors,
        master_mix,
        EXPORT_SAMPLE_RATE,
    )
    .unwrap();
    (snapshot, staged)
}

fn snapshot_for_staged(
    source: &OfflineExportSnapshot,
    staged: &[StagedExportPad],
) -> OfflineExportSnapshot {
    OfflineExportSnapshot::new(
        source.project_id(),
        source.revision(),
        source.slot(),
        source.pattern().clone(),
        staged.iter().map(descriptor).collect(),
        source.master_mix(),
        source.sample_rate(),
    )
    .unwrap()
}

#[derive(Default)]
struct CollectingSink {
    frames: Vec<[f32; 2]>,
    writes: Vec<usize>,
}

impl OfflineFrameSink for CollectingSink {
    fn write_frames(&mut self, frames: &[[f32; 2]]) -> Result<(), OfflineExportError> {
        self.writes.push(frames.len());
        self.frames.extend_from_slice(frames);
        Ok(())
    }
}

fn reference_engine(
    snapshot: &OfflineExportSnapshot,
    staged: &[StagedExportPad],
) -> (sampler_audio::AudioController, AudioEngine) {
    let (mut controller, ports) = audio_channels();
    let mut engine = AudioEngine::new(EXPORT_SAMPLE_RATE, ports).unwrap();
    for staged_pad in staged {
        controller
            .install(
                staged_pad.pad,
                Arc::clone(&staged_pad.sample),
                staged_pad.settings,
                staged_pad.mix,
            )
            .unwrap();
        engine.render_frames(0, |_| {});
    }
    controller.update_master_mix(snapshot.master_mix()).unwrap();
    engine.render_frames(0, |_| {});
    let compiled = Arc::new(snapshot.pattern().to_editable().unwrap().compile().unwrap());
    controller.install_pattern(compiled).unwrap();
    controller
        .select_pattern(snapshot.slot(), PatternSwitch::Immediate)
        .unwrap();
    controller.play_pattern().unwrap();
    engine.render_frames(0, |_| {});
    (controller, engine)
}

fn render_reference(snapshot: &OfflineExportSnapshot, staged: &[StagedExportPad]) -> Vec<[f32; 2]> {
    let (_controller, mut engine) = reference_engine(snapshot, staged);
    let mut frames = Vec::with_capacity(snapshot.loop_frames().unwrap() as usize);
    engine.render_frames(snapshot.loop_frames().unwrap() as usize, |frame| {
        frames.push(frame)
    });
    frames
}

fn bits(frames: &[[f32; 2]]) -> Vec<u32> {
    frames
        .iter()
        .flat_map(|frame| [frame[0].to_bits(), frame[1].to_bits()])
        .collect()
}

fn expected_peak(frames: &[[f32; 2]]) -> [f32; 2] {
    frames.iter().fold([0.0_f32; 2], |peak, frame| {
        [peak[0].max(frame[0].abs()), peak[1].max(frame[1].abs())]
    })
}

#[test]
fn renderer_matches_independent_production_engine_for_dry_and_wet_mix() {
    for wet in [false, true] {
        let (snapshot, staged) = fixture(wet);
        let reference = render_reference(&snapshot, &staged);
        let mut sink = CollectingSink::default();

        let summary =
            render_offline(&snapshot, &staged, &mut sink, &AtomicBool::new(false)).unwrap();

        assert_eq!(
            summary,
            OfflineRenderSummary {
                frame_count: snapshot.loop_frames().unwrap(),
                peak: expected_peak(&reference),
            }
        );
        assert_eq!(bits(&sink.frames), bits(&reference), "wet={wet}");
        assert!(sink.frames.iter().any(|frame| *frame != [0.0, 0.0]));
    }
}

#[test]
fn parity_fixture_exercises_choke_while_the_first_voice_is_audible() {
    let (choked_snapshot, choked_staged) = fixture(false);
    let mut unchoked_staged = choked_staged.clone();
    unchoked_staged[0].settings.choke_group = None;
    unchoked_staged[1].settings.choke_group = None;
    let unchoked_snapshot = snapshot_for_staged(&choked_snapshot, &unchoked_staged);

    let choked = render_reference(&choked_snapshot, &choked_staged);
    let unchoked = render_reference(&unchoked_snapshot, &unchoked_staged);
    let second_trigger = choked_snapshot
        .pattern()
        .events
        .iter()
        .find(|event| event.event.pad == pad(1))
        .unwrap()
        .event
        .frame as usize;
    let comparison_end = (second_trigger + 64).min(choked.len());

    assert_ne!(
        bits(&choked[second_trigger..comparison_end]),
        bits(&unchoked[second_trigger..comparison_end])
    );
}

#[test]
fn renderer_streams_4096_frame_chunks_and_truncates_an_audible_tail_at_loop_edge() {
    let (snapshot, staged) = fixture(true);
    let mut sink = CollectingSink::default();

    render_offline(&snapshot, &staged, &mut sink, &AtomicBool::new(false)).unwrap();

    assert_eq!(sink.frames.len(), 12_000);
    assert_eq!(sink.writes, vec![4_096, 4_096, 3_808]);
    assert!(
        sink.writes
            .iter()
            .all(|count| *count <= EXPORT_CHUNK_FRAMES)
    );
    assert_ne!(sink.frames.last().copied().unwrap(), [0.0, 0.0]);

    let (_controller, mut engine) = reference_engine(&snapshot, &staged);
    engine.render_frames(snapshot.loop_frames().unwrap() as usize, |_| {});
    let mut first_frame_past_edge = [0.0; 2];
    engine.render_frames(1, |frame| first_frame_past_edge = frame);
    assert_ne!(first_frame_past_edge, [0.0, 0.0]);
}

struct CancellingSink<'a> {
    cancelled: &'a AtomicBool,
    writes: usize,
}

impl OfflineFrameSink for CancellingSink<'_> {
    fn write_frames(&mut self, frames: &[[f32; 2]]) -> Result<(), OfflineExportError> {
        assert_eq!(frames.len(), EXPORT_CHUNK_FRAMES);
        self.writes += 1;
        self.cancelled.store(true, Ordering::Release);
        Ok(())
    }
}

#[test]
fn renderer_honors_cancellation_before_setup_and_between_chunks() {
    let (snapshot, staged) = fixture(false);
    let already_cancelled = AtomicBool::new(true);
    let mut untouched = CollectingSink::default();
    assert_eq!(
        render_offline(&snapshot, &staged, &mut untouched, &already_cancelled),
        Err(OfflineExportError::Cancelled)
    );
    assert!(untouched.frames.is_empty());

    let cancelled = AtomicBool::new(false);
    let mut sink = CancellingSink {
        cancelled: &cancelled,
        writes: 0,
    };
    assert_eq!(
        render_offline(&snapshot, &staged, &mut sink, &cancelled),
        Err(OfflineExportError::Cancelled)
    );
    assert_eq!(sink.writes, 1);
}

#[test]
fn renderer_rejects_missing_duplicate_extra_and_wrong_rate_staged_pads() {
    let (snapshot, staged) = fixture(false);
    let mut sink = CollectingSink::default();
    assert_eq!(
        render_offline(
            &snapshot,
            &staged[..staged.len() - 1],
            &mut sink,
            &AtomicBool::new(false),
        ),
        Err(OfflineExportError::RendererUnavailable)
    );

    let mut duplicate = staged.clone();
    let last = duplicate.len() - 1;
    duplicate[last] = staged[0].clone();
    assert_eq!(
        render_offline(&snapshot, &duplicate, &mut sink, &AtomicBool::new(false),),
        Err(OfflineExportError::RendererUnavailable)
    );

    let mut extra = staged.clone();
    let mut unrelated = staged[0].clone();
    unrelated.pad = pad(15);
    extra.push(unrelated);
    assert_eq!(
        render_offline(&snapshot, &extra, &mut sink, &AtomicBool::new(false),),
        Err(OfflineExportError::RendererUnavailable)
    );

    let mut wrong_rate = staged;
    wrong_rate[0].sample = Arc::new(SampleBuffer::new(44_100, vec![0.25, -0.25]).unwrap());
    assert_eq!(
        render_offline(&snapshot, &wrong_rate, &mut sink, &AtomicBool::new(false),),
        Err(OfflineExportError::RendererUnavailable)
    );
}
