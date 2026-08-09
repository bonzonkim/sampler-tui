use std::array;
use std::collections::VecDeque;
use std::fmt;
use std::mem;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use sampler_audio::{
    CaptureBuffer, CaptureOutcome, CaptureSource, CaptureState, CaptureStatus, LiveCommandId,
    MAX_CAPTURE_FRAMES, SampleBuffer, Telemetry, TransportStamp,
};
use sampler_core::pad::{BANK_COUNT, PADS_PER_BANK};
use sampler_core::{
    BankId, MIDI_NOTE_COUNT, MIDI_OWNERSHIP_COUNT, MasterMixSettings, MidiChannelFilter, MidiNote,
    MidiSettings, PadId, PadMixSettings, PadSettings, PatternSlotId, PlaybackMode, ProjectDocument,
    ProjectId, SampleEditRecipe,
};

use crate::PatternSwitch;
use crate::audio::{AudioPort, open_default_audio};
use crate::capture::{CaptureError, CaptureFailureCause, CapturePhase};
use crate::capture_store::{ManagedCapture, ManagedCaptureId};
use crate::file_picker::FilePicker;
use crate::input::{InputAction, KeyboardCapabilities, map_key};
use crate::loader::{
    EditPreview, FinalizeCaptureRequest, LoadPurpose, MAX_DIRECTORY_ENTRIES,
    ProjectSaveWorkerRequest, ProjectToken, RenderedSample, StageProjectSampleRequest,
    WORKER_CHANNEL_CAPACITY, WorkerRequest, WorkerResult, WorkerSendError,
};
use crate::midi::{MidiEvent, MidiService, MidiServiceEvent};
use crate::mixer::{MixerAction, MixerContext, MixerCursor, MixerIntent};
use crate::palette::{LineEditor, PaletteCommand, parse_palette};
use crate::pattern::{PatternStatus, PatternWorkspace, WorkspaceView};
use crate::project_session::{
    ProjectOpenError, ProjectOpenPhase, ProjectOpenStage, ProjectSession, ProjectSnapshotError,
    ProjectStageError, RecoveryChoice,
};
use crate::project_store::{
    ProjectProbe, ProjectSavePad, ProjectSaveRequest, ProjectSaveSnapshot, ProjectStoreError,
    SaveKind, SaveReceipt, SourceFingerprint,
};
use crate::sample_editor::{
    SampleEditor, SampleEditorContext, SampleEditorError, SampleEditorIntent, SampleMarker,
};

pub const PAD_VIEW_COUNT: usize = 160;
/// Fixed worker-generated waveform resolution. Perform uses a bounded 64-column projection.
pub const EDIT_PREVIEW_COLUMNS: usize = 1_024;
pub const PREVIEW_COLUMNS: usize = 64;
const AUTOSAVE_DEBOUNCE: Duration = Duration::from_secs(2);
const MIDI_RECORDING_KEY_OFFSET: usize = PADS_PER_BANK as usize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectSaveError {
    Untitled,
    OperationPending,
    Snapshot(ProjectSnapshotError),
    Entropy(String),
    TokenExhausted,
}

#[cfg(test)]
mod mixer_task6_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use sampler_audio::{
        CaptureCompletion, CaptureSource, Frame, SampleBuffer, SampleSlot, Telemetry,
    };
    use sampler_core::{
        BankId, ChokeGroup, DelaySettings, MasterMixSettings, PadId, PadMixSettings, PadSettings,
        PlaybackMode, ProjectDocument, ProjectId, ReverbSettings, SampleEditRecipe,
    };

    use super::{
        App, EDIT_PREVIEW_COLUMNS, Overlay, PadLoadState, PreviewColumn, ProjectAdmission,
        ProjectOpenOperation, StagedProjectPad, pad_offset,
    };
    use crate::KeyboardCapabilities;
    use crate::audio::{AudioPort, CaptureSupport};
    use crate::capture::CapturePhase;
    use crate::capture_store::{ManagedCapture, ManagedCaptureId};
    use crate::loader::{LoadedSample, ProjectToken, WorkerRequest, WorkerResult};
    use crate::mixer::{MixerSection, PadField};
    use crate::pattern::WorkspaceView;
    use crate::project_store::SourceFingerprint;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MixerAudioCallKind {
        Install,
        UpdatePad,
        UpdatePadMix,
        UpdateMasterMix,
        RemoveSample,
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum MixerAudioCall {
        Install(PadId, PadSettings, PadMixSettings),
        UpdatePad(PadId, PadSettings),
        UpdatePadMix(PadId, PadMixSettings),
        UpdateMasterMix(MasterMixSettings),
        RemoveSample(PadId),
        Trigger(PadId),
        Release(PadId),
    }

    struct MixerAudioState {
        calls: Vec<MixerAudioCall>,
        fail_next: VecDeque<MixerAudioCallKind>,
        fail_install_number: Option<usize>,
        install_attempts: usize,
        runtime_error: Option<String>,
        installed: Vec<(PadId, PadSettings, PadMixSettings)>,
        master: Option<MasterMixSettings>,
    }

    #[derive(Clone)]
    struct MixerProbe(Rc<RefCell<MixerAudioState>>);

    impl MixerProbe {
        fn calls(&self) -> Vec<MixerAudioCall> {
            self.0.borrow().calls.clone()
        }

        fn fail_next(&self, kind: MixerAudioCallKind) {
            self.0.borrow_mut().fail_next.push_back(kind);
        }

        fn fail_device(&self, message: &str) {
            self.0.borrow_mut().runtime_error = Some(message.to_owned());
        }

        fn fail_install_number(&self, number: usize) {
            let mut state = self.0.borrow_mut();
            state.fail_install_number = Some(number);
            state.install_attempts = 0;
        }

        fn installed_mix(&self, pad: PadId) -> Option<PadMixSettings> {
            self.0
                .borrow()
                .installed
                .iter()
                .rev()
                .find_map(|(candidate, _, mix)| (*candidate == pad).then_some(*mix))
        }

        fn installed_tuple(&self, pad: PadId) -> Option<(PadSettings, PadMixSettings)> {
            self.0
                .borrow()
                .installed
                .iter()
                .rev()
                .find_map(|(candidate, settings, mix)| {
                    (*candidate == pad).then_some((*settings, *mix))
                })
        }

        fn master(&self) -> Option<MasterMixSettings> {
            self.0.borrow().master
        }
    }

    struct MixerAudio(MixerProbe);

    impl MixerAudio {
        fn new() -> (Self, MixerProbe) {
            let probe = MixerProbe(Rc::new(RefCell::new(MixerAudioState {
                calls: Vec::new(),
                fail_next: VecDeque::new(),
                fail_install_number: None,
                install_attempts: 0,
                runtime_error: None,
                installed: Vec::new(),
                master: None,
            })));
            (Self(probe.clone()), probe)
        }

        fn admit(&self, kind: MixerAudioCallKind, call: MixerAudioCall) -> Result<(), String> {
            let mut state = self.0.0.borrow_mut();
            state.calls.push(call);
            if kind == MixerAudioCallKind::Install {
                state.install_attempts += 1;
                if state.fail_install_number == Some(state.install_attempts) {
                    state.fail_install_number = None;
                    return Err(format!("install {} failed", state.install_attempts));
                }
            }
            if state.fail_next.front() == Some(&kind) {
                state.fail_next.pop_front();
                Err(format!("{kind:?} failed"))
            } else {
                Ok(())
            }
        }
    }

    impl AudioPort for MixerAudio {
        fn sample_rate(&self) -> u32 {
            48_000
        }

        fn channels(&self) -> u16 {
            2
        }

        fn render_horizon(&self) -> Frame {
            0
        }

        fn install(
            &mut self,
            pad: PadId,
            _sample: Arc<SampleBuffer>,
            settings: PadSettings,
            mix: PadMixSettings,
        ) -> Result<SampleSlot, String> {
            self.admit(
                MixerAudioCallKind::Install,
                MixerAudioCall::Install(pad, settings, mix),
            )?;
            let mut state = self.0.0.borrow_mut();
            state
                .installed
                .retain(|(candidate, _, _)| *candidate != pad);
            state.installed.push((pad, settings, mix));
            SampleSlot::new(0).map_err(|error| error.to_string())
        }

        fn trigger(&mut self, pad: PadId, _at: Frame, _velocity: f32) -> Result<(), String> {
            self.0
                .0
                .borrow_mut()
                .calls
                .push(MixerAudioCall::Trigger(pad));
            Ok(())
        }

        fn release(&mut self, pad: PadId, _at: Frame) -> Result<(), String> {
            self.0
                .0
                .borrow_mut()
                .calls
                .push(MixerAudioCall::Release(pad));
            Ok(())
        }

        fn stop_pad(&mut self, _pad: PadId) -> Result<(), String> {
            Ok(())
        }

        fn stop_all(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn remove_sample(&mut self, pad: PadId) -> Result<(), String> {
            self.admit(
                MixerAudioCallKind::RemoveSample,
                MixerAudioCall::RemoveSample(pad),
            )?;
            self.0
                .0
                .borrow_mut()
                .installed
                .retain(|(candidate, _, _)| *candidate != pad);
            Ok(())
        }

        fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String> {
            self.admit(
                MixerAudioCallKind::UpdatePad,
                MixerAudioCall::UpdatePad(pad, settings),
            )?;
            if let Some((_, installed, _)) = self
                .0
                .0
                .borrow_mut()
                .installed
                .iter_mut()
                .find(|(candidate, _, _)| *candidate == pad)
            {
                *installed = settings;
            }
            Ok(())
        }

        fn update_pad_mix(&mut self, pad: PadId, settings: PadMixSettings) -> Result<(), String> {
            self.admit(
                MixerAudioCallKind::UpdatePadMix,
                MixerAudioCall::UpdatePadMix(pad, settings),
            )?;
            if let Some((_, _, installed)) = self
                .0
                .0
                .borrow_mut()
                .installed
                .iter_mut()
                .find(|(candidate, _, _)| *candidate == pad)
            {
                *installed = settings;
            }
            Ok(())
        }

        fn update_master_mix(&mut self, settings: MasterMixSettings) -> Result<(), String> {
            self.admit(
                MixerAudioCallKind::UpdateMasterMix,
                MixerAudioCall::UpdateMasterMix(settings),
            )?;
            self.0.0.borrow_mut().master = Some(settings);
            Ok(())
        }

        fn reclaim_retired(&mut self) -> usize {
            0
        }

        fn latest_telemetry(&mut self) -> Option<Telemetry> {
            None
        }

        fn poll_runtime_error(&mut self) -> Option<String> {
            self.0.0.borrow_mut().runtime_error.take()
        }

        fn capture_support(&self) -> CaptureSupport {
            CaptureSupport::Unsupported
        }
    }

    fn pad(index: u8) -> PadId {
        PadId::new(BankId::new(0).unwrap(), index).unwrap()
    }

    fn sample() -> Arc<SampleBuffer> {
        Arc::new(SampleBuffer::new(48_000, vec![0.25, -0.25]).unwrap())
    }

    fn install_loaded_pad(app: &mut App, target: PadId, mix: PadMixSettings) {
        app.update_pad_mix(target, mix).unwrap();
        let sample = sample();
        let settings = PadSettings::default();
        app.audio
            .as_mut()
            .unwrap()
            .install(target, Arc::clone(&sample), settings, mix)
            .unwrap();
        let offset = pad_offset(target);
        app.pads[offset].sample = Some(sample);
        app.pads[offset].state = PadLoadState::Ready;
        app.current_session_bound[offset] = true;
    }

    fn loaded_mixer_app() -> (App, MixerProbe, PadId) {
        let (audio, probe) = MixerAudio::new();
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0);
        install_loaded_pad(&mut app, target, PadMixSettings::default());
        (app, probe, target)
    }

    fn nondefault_master_mix() -> MasterMixSettings {
        MasterMixSettings::new(
            -3.0,
            DelaySettings::new(true, 20, 0.4, -6.0).unwrap(),
            ReverbSettings::new(true, 0.7, 0.3, -9.0).unwrap(),
        )
        .unwrap()
    }

    fn managed_capture(id: u64) -> ManagedCapture {
        let rendered = sample();
        let fingerprint =
            SourceFingerprint::from_encoded_bytes(Path::new("capture.wav"), b"capture").unwrap();
        ManagedCapture {
            id: ManagedCaptureId::new(id),
            path: PathBuf::from(format!("managed-{id}.wav")),
            fingerprint,
            sample: LoadedSample {
                fingerprint,
                base: Arc::clone(&rendered),
                base_preview: Arc::new([PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS]),
                rendered,
                rendered_preview: Arc::new(
                    [PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS],
                ),
                recipe: SampleEditRecipe::identity(),
                source_rate: 48_000,
                source_frames: 1,
                duration: Duration::from_secs_f64(1.0 / 48_000.0),
            },
        }
    }

    fn mixer_loaded_sample() -> LoadedSample {
        let rendered = sample();
        let fingerprint =
            SourceFingerprint::from_encoded_bytes(Path::new("fixture.wav"), b"").unwrap();
        LoadedSample {
            fingerprint,
            base: Arc::clone(&rendered),
            base_preview: Arc::new([PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS]),
            rendered,
            rendered_preview: Arc::new([PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS]),
            recipe: SampleEditRecipe::identity(),
            source_rate: 48_000,
            source_frames: 1,
            duration: Duration::from_secs_f64(1.0 / 48_000.0),
        }
    }

    fn mixer_project_pad(
        target: PadId,
        settings: PadSettings,
        mix: PadMixSettings,
    ) -> sampler_core::ProjectPad {
        let fingerprint =
            SourceFingerprint::from_encoded_bytes(Path::new("fixture.wav"), b"").unwrap();
        sampler_core::ProjectPad::new(
            target,
            format!("audio/{}.wav", fingerprint.digest),
            fingerprint.digest,
            settings,
            mix,
            SampleEditRecipe::identity(),
        )
        .unwrap()
    }

    fn stage_mixer_project_open(app: &mut App, document: ProjectDocument) {
        let pad_specs = document
            .pads
            .iter()
            .map(|pad| (pad.pad, pad.settings, pad.mix))
            .collect::<Vec<_>>();
        let operation = app
            .build_project_open_candidate(
                ProjectToken::new(801),
                PathBuf::from("mixer-project"),
                document,
                0,
                false,
            )
            .unwrap();
        let ProjectOpenOperation::Staging(mut candidate) = operation else {
            panic!("expected staged project candidate")
        };
        for (target, settings, mix) in pad_specs {
            candidate.staged_pads[pad_offset(target)] = Some(Box::new(StagedProjectPad {
                path: PathBuf::from("mixer-project/fixture.wav"),
                settings,
                mix,
                loaded: mixer_loaded_sample(),
            }));
        }
        candidate.next_decode = candidate.document.pads.len();
        candidate.progress.staged_pads = candidate.document.pads.len();
        app.project_open = Some(ProjectOpenOperation::Staging(candidate));
    }

    fn finish_mixer_project_open(app: &mut App) {
        while app.project_open_stage().is_some() {
            if let Some(ProjectOpenOperation::Staging(candidate)) = app.project_open.as_mut()
                && matches!(candidate.admission, ProjectAdmission::Patterns(_))
            {
                candidate.admission = ProjectAdmission::Complete;
            }
            assert!(app.maintain_project(Instant::now()));
        }
    }

    fn ready_capture_target_fixture() -> (App, MixerProbe, PadId, ManagedCaptureId) {
        let (mut app, probe, target) = loaded_mixer_app();
        app.capture_session
            .begin(CaptureSource::Resample, target, 48_000, 4)
            .unwrap();
        app.capture_target_fence = Some(app.capture_target_fence_for(target));
        app.capture_session.mark_arming().unwrap();
        app.capture_session.mark_recording().unwrap();
        app.capture_session
            .accept_completion(CaptureCompletion {
                token: app.capture_session.token().unwrap(),
                target,
                source: CaptureSource::Resample,
                sample_rate: 48_000,
                stereo: vec![0.5, -0.5],
                hard_limit: false,
                peak: 0.5,
            })
            .unwrap();
        app.capture_session.mark_ready_to_install().unwrap();
        let id = ManagedCaptureId::new(91);
        app.capture_session
            .set_managed_capture_id(Some(id))
            .unwrap();
        app.capture_ready = Some(managed_capture(id.get()));
        (app, probe, target, id)
    }

    #[test]
    fn loaded_pad_mix_and_master_commit_only_after_exact_audio_admission() {
        let (mut app, probe, target) = loaded_mixer_app();
        let revision = app.project_revision();
        let pad_mix = PadMixSettings::new(true, 0.25, 0.75).unwrap();
        app.update_pad_mix(target, pad_mix).unwrap();
        assert_eq!(
            probe.calls().last(),
            Some(&MixerAudioCall::UpdatePadMix(target, pad_mix))
        );
        assert_eq!(app.pad_mix(target), pad_mix);
        assert_eq!(app.project_revision(), revision + 1);

        let revision = app.project_revision();
        let master = nondefault_master_mix();
        app.update_master_mix(master).unwrap();
        assert_eq!(
            probe.calls().last(),
            Some(&MixerAudioCall::UpdateMasterMix(master))
        );
        assert_eq!(app.master_mix(), master);
        assert_eq!(app.project_revision(), revision + 1);
    }

    #[test]
    fn mixer_noops_unloaded_updates_failures_and_revision_exhaustion_are_exact() {
        let (mut app, probe, target) = loaded_mixer_app();
        let before = (
            app.pad_mix(target),
            app.master_mix(),
            app.project_revision(),
        );
        probe.fail_next(MixerAudioCallKind::UpdatePadMix);
        assert!(
            app.update_pad_mix(target, PadMixSettings::new(true, 1.0, 1.0).unwrap())
                .is_err()
        );
        assert_eq!(
            (
                app.pad_mix(target),
                app.master_mix(),
                app.project_revision()
            ),
            before
        );

        app.project_session
            .set_current_revision_for_test(i64::MAX as u64);
        let before = (app.master_mix(), app.project_revision(), probe.calls());
        assert!(app.update_master_mix(nondefault_master_mix()).is_err());
        assert_eq!(
            (app.master_mix(), app.project_revision(), probe.calls()),
            before
        );

        let (audio, unloaded_probe) = MixerAudio::new();
        let mut unloaded = App::with_audio(Box::new(audio));
        let local = PadMixSettings::new(false, 0.3, 0.6).unwrap();
        unloaded.update_pad_mix(target, local).unwrap();
        assert_eq!(unloaded.pad_mix(target), local);
        assert_eq!(unloaded.project_revision(), 0);
        let calls = unloaded_probe.calls();
        unloaded.update_pad_mix(target, local).unwrap();
        assert_eq!(unloaded_probe.calls(), calls);
        assert_eq!(unloaded.project_revision(), 0);
    }

    #[test]
    fn replacement_audio_is_fully_configured_before_device_recovery_commits() {
        let (mut app, old_probe, first_pad) = loaded_mixer_app();
        let first_mix = PadMixSettings::new(true, 0.2, 0.8).unwrap();
        app.update_pad_mix(first_pad, first_mix).unwrap();
        let second_pad = pad(1);
        let second_mix = PadMixSettings::new(false, 0.4, 0.6).unwrap();
        install_loaded_pad(&mut app, second_pad, second_mix);
        let master = nondefault_master_mix();
        app.update_master_mix(master).unwrap();

        old_probe.fail_device("lost");
        assert!(app.maintain_audio());
        let (replacement, replacement_probe) = MixerAudio::new();
        assert!(app.retry_default_device_with(|| Ok(Box::new(replacement))));
        assert_eq!(replacement_probe.master(), Some(master));
        assert_eq!(replacement_probe.installed_mix(first_pad), Some(first_mix));
        assert_eq!(
            replacement_probe.installed_mix(second_pad),
            Some(second_mix)
        );
        assert_eq!(
            replacement_probe.calls(),
            vec![
                MixerAudioCall::UpdateMasterMix(master),
                MixerAudioCall::Install(first_pad, PadSettings::default(), first_mix),
                MixerAudioCall::Install(second_pad, PadSettings::default(), second_mix),
            ]
        );
    }

    #[test]
    fn mixer_task6_candidate_failures_preserve_the_old_app_and_audio_session_tuple() {
        for failure in [
            MixerAudioCallKind::UpdateMasterMix,
            MixerAudioCallKind::Install,
            MixerAudioCallKind::UpdatePad,
        ] {
            let (mut app, old_probe, first_pad) = loaded_mixer_app();
            let second_pad = pad(1);
            install_loaded_pad(
                &mut app,
                second_pad,
                PadMixSettings::new(false, 0.4, 0.6).unwrap(),
            );
            let master = nondefault_master_mix();
            app.update_master_mix(master).unwrap();
            let before = (
                app.pad_mix(first_pad),
                app.pad_mix(second_pad),
                app.master_mix(),
                app.project_revision(),
            );

            let (candidate, candidate_probe) = MixerAudio::new();
            match failure {
                MixerAudioCallKind::UpdateMasterMix => {
                    candidate_probe.fail_next(MixerAudioCallKind::UpdateMasterMix)
                }
                MixerAudioCallKind::Install => candidate_probe.fail_install_number(1),
                MixerAudioCallKind::UpdatePad => candidate_probe.fail_install_number(2),
                MixerAudioCallKind::UpdatePadMix | MixerAudioCallKind::RemoveSample => {
                    unreachable!()
                }
            }
            assert!(app.retry_with(Box::new(candidate)));
            assert_eq!(
                (
                    app.pad_mix(first_pad),
                    app.pad_mix(second_pad),
                    app.master_mix(),
                    app.project_revision(),
                ),
                before
            );

            let admitted = PadMixSettings::new(true, 0.3, 0.7).unwrap();
            app.update_pad_mix(first_pad, admitted).unwrap();
            assert_eq!(
                old_probe.calls().last(),
                Some(&MixerAudioCall::UpdatePadMix(first_pad, admitted)),
                "candidate {failure:?} failure must preserve the old audio session"
            );
        }
    }

    #[test]
    fn mixer_task6_project_open_admits_default_master_before_commit_and_retries_atomically() {
        let (mut app, probe, _) = loaded_mixer_app();
        let nondefault = nondefault_master_mix();
        app.update_master_mix(nondefault).unwrap();
        let old_project_id = app.project_session.project_id();
        let document = ProjectDocument::new_v4(
            ProjectId::from_bytes([0x61; 16]),
            "Mixer open",
            7,
            Vec::new(),
            app.patterns.export_project_patterns().unwrap(),
            MasterMixSettings::default(),
            sampler_core::MidiSettings::default(),
        )
        .unwrap();
        app.project_open = Some(
            app.build_project_open_candidate(
                ProjectToken::new(91),
                PathBuf::from("mixer-open"),
                document,
                7,
                false,
            )
            .unwrap(),
        );

        assert!(app.maintain_project(Instant::now()));
        probe.fail_next(MixerAudioCallKind::UpdateMasterMix);
        assert!(!app.maintain_project(Instant::now()));
        assert_eq!(app.master_mix(), nondefault);
        assert_eq!(app.project_session.project_id(), old_project_id);
        assert_eq!(app.project_open_stage().unwrap().admitted_actions, 2);
        assert_eq!(
            probe.calls().last(),
            Some(&MixerAudioCall::UpdateMasterMix(
                MasterMixSettings::default()
            ))
        );

        assert!(app.maintain_project(Instant::now()));
        assert_eq!(app.project_open_stage().unwrap().admitted_actions, 3);
        assert_eq!(
            probe.calls().last(),
            Some(&MixerAudioCall::UpdateMasterMix(
                MasterMixSettings::default()
            ))
        );
    }

    #[test]
    fn mixer_project_open_remove_failure_restores_the_exact_old_audio_tuple_before_retry() {
        let (mut app, probe, old_pad) = loaded_mixer_app();
        let old_mix = PadMixSettings::new(true, 0.2, 0.8).unwrap();
        app.update_pad_mix(old_pad, old_mix).unwrap();
        let old_master = nondefault_master_mix();
        app.update_master_mix(old_master).unwrap();
        let old_tuple = (
            app.pad(old_pad).settings,
            app.pad_mix(old_pad),
            app.master_mix(),
            app.project_revision(),
        );
        let candidate_master = MasterMixSettings::default();
        let document = ProjectDocument::new_v4(
            ProjectId::from_bytes([0xa1; 16]),
            "Remove old pad",
            21,
            Vec::new(),
            app.patterns.export_project_patterns().unwrap(),
            candidate_master,
            sampler_core::MidiSettings::default(),
        )
        .unwrap();
        stage_mixer_project_open(&mut app, document);

        assert!(app.maintain_project(Instant::now()));
        assert!(app.maintain_project(Instant::now()));
        probe.fail_next(MixerAudioCallKind::RemoveSample);
        assert!(!app.maintain_project(Instant::now()));

        assert_eq!(
            (
                app.pad(old_pad).settings,
                app.pad_mix(old_pad),
                app.master_mix(),
                app.project_revision(),
            ),
            old_tuple
        );
        assert_eq!(probe.master(), Some(old_master));
        assert_eq!(
            probe.installed_tuple(old_pad),
            Some((PadSettings::default(), old_mix))
        );

        finish_mixer_project_open(&mut app);
        assert_eq!(app.project_revision(), 21);
        assert_eq!(app.master_mix(), candidate_master);
        assert_eq!(probe.master(), Some(candidate_master));
        assert_eq!(probe.installed_tuple(old_pad), None);
    }

    #[test]
    fn mixer_project_open_later_install_failure_rolls_back_earlier_candidate_pad_before_retry() {
        let (mut app, probe, first_pad) = loaded_mixer_app();
        let old_settings = PadSettings::new(
            PlaybackMode::Gate,
            -5.0,
            0.1,
            1.5,
            Some(ChokeGroup::new(2).unwrap()),
        )
        .unwrap();
        app.update_pad_settings(first_pad, old_settings).unwrap();
        let old_mix = PadMixSettings::new(true, 0.1, 0.9).unwrap();
        app.update_pad_mix(first_pad, old_mix).unwrap();
        let old_master = nondefault_master_mix();
        app.update_master_mix(old_master).unwrap();
        let old_tuple = (
            app.pad(first_pad).settings,
            app.pad_mix(first_pad),
            app.master_mix(),
            app.project_revision(),
        );

        let candidate_master =
            MasterMixSettings::new(2.0, DelaySettings::default(), ReverbSettings::default())
                .unwrap();
        let first_settings = PadSettings::new(PlaybackMode::OneShot, -1.0, 0.2, 0.5, None).unwrap();
        let first_mix = PadMixSettings::new(false, 0.7, 0.3).unwrap();
        let second_pad = pad(1);
        let second_settings = PadSettings::new(PlaybackMode::Gate, -2.0, 0.3, 0.75, None).unwrap();
        let second_mix = PadMixSettings::new(false, 0.4, 0.6).unwrap();
        let document = ProjectDocument::new_v4(
            ProjectId::from_bytes([0xa2; 16]),
            "Replace pads",
            22,
            vec![
                mixer_project_pad(first_pad, first_settings, first_mix),
                mixer_project_pad(second_pad, second_settings, second_mix),
            ],
            app.patterns.export_project_patterns().unwrap(),
            candidate_master,
            sampler_core::MidiSettings::default(),
        )
        .unwrap();
        stage_mixer_project_open(&mut app, document);

        assert!(app.maintain_project(Instant::now()));
        assert!(app.maintain_project(Instant::now()));
        probe.fail_install_number(2);
        assert!(app.maintain_project(Instant::now()));
        assert!(!app.maintain_project(Instant::now()));

        assert_eq!(
            (
                app.pad(first_pad).settings,
                app.pad_mix(first_pad),
                app.master_mix(),
                app.project_revision(),
            ),
            old_tuple
        );
        assert_eq!(probe.master(), Some(old_master));
        assert_eq!(
            probe.installed_tuple(first_pad),
            Some((old_settings, old_mix))
        );
        assert_eq!(probe.installed_tuple(second_pad), None);

        finish_mixer_project_open(&mut app);
        assert_eq!(app.project_revision(), 22);
        assert_eq!(app.master_mix(), candidate_master);
        assert_eq!(
            probe.installed_tuple(first_pad),
            Some((first_settings, first_mix))
        );
        assert_eq!(
            probe.installed_tuple(second_pad),
            Some((second_settings, second_mix))
        );
    }

    #[test]
    fn mixer_project_open_rollback_failure_stays_inconsistent_until_exact_restore_retries() {
        let (mut app, probe, old_pad) = loaded_mixer_app();
        let old_mix = PadMixSettings::new(true, 0.35, 0.65).unwrap();
        app.update_pad_mix(old_pad, old_mix).unwrap();
        let old_master = nondefault_master_mix();
        app.update_master_mix(old_master).unwrap();
        let old_tuple = (
            app.pad(old_pad).settings,
            app.pad_mix(old_pad),
            app.master_mix(),
            app.project_revision(),
        );
        let document = ProjectDocument::new_v4(
            ProjectId::from_bytes([0xa3; 16]),
            "Rollback retry",
            23,
            Vec::new(),
            app.patterns.export_project_patterns().unwrap(),
            MasterMixSettings::default(),
            sampler_core::MidiSettings::default(),
        )
        .unwrap();
        stage_mixer_project_open(&mut app, document);

        assert!(app.maintain_project(Instant::now()));
        assert!(app.maintain_project(Instant::now()));
        probe.fail_next(MixerAudioCallKind::RemoveSample);
        probe.fail_next(MixerAudioCallKind::UpdateMasterMix);
        assert!(!app.maintain_project(Instant::now()));
        assert!(app.status().contains("rollback"));
        assert_eq!(
            (
                app.pad(old_pad).settings,
                app.pad_mix(old_pad),
                app.master_mix(),
                app.project_revision(),
            ),
            old_tuple
        );
        assert_ne!(probe.master(), Some(old_master));

        assert!(app.maintain_project(Instant::now()));
        assert_eq!(probe.master(), Some(old_master));
        assert_eq!(
            probe.installed_tuple(old_pad),
            Some((PadSettings::default(), old_mix))
        );

        finish_mixer_project_open(&mut app);
        assert_eq!(app.project_revision(), 23);
        assert_eq!(probe.master(), Some(MasterMixSettings::default()));
        assert_eq!(probe.installed_tuple(old_pad), None);
    }

    #[test]
    fn capture_target_fence_rejects_ready_take_after_mix_or_choke_changes() {
        for change_mix in [true, false] {
            let (mut app, probe, target, managed_id) = ready_capture_target_fixture();
            let installs_before = probe
                .calls()
                .into_iter()
                .filter(|call| matches!(call, MixerAudioCall::Install(..)))
                .count();
            if change_mix {
                app.update_pad_mix(target, PadMixSettings::new(true, 0.5, 0.5).unwrap())
                    .unwrap();
            } else {
                app.update_pad_settings(
                    target,
                    PadSettings::new(
                        PlaybackMode::OneShot,
                        0.0,
                        0.0,
                        0.0,
                        Some(ChokeGroup::new(1).unwrap()),
                    )
                    .unwrap(),
                )
                .unwrap();
            }
            assert!(app.maintain_capture());
            assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
            let installs_after = probe
                .calls()
                .into_iter()
                .filter(|call| matches!(call, MixerAudioCall::Install(..)))
                .count();
            assert_eq!(installs_after, installs_before);
            assert_eq!(app.capture_session().managed_capture_id(), Some(managed_id));
            assert_eq!(app.pending_managed_releases.back(), Some(&managed_id));
            app.cancel_capture().unwrap();
            assert!(app.maintain_capture());
            assert_eq!(
                app.take_worker_requests(),
                [WorkerRequest::ReleaseManagedCapture { id: managed_id }]
            );
            assert!(
                app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                    id: managed_id,
                    result: Ok(()),
                })
            );
            assert!(app.maintain_capture());
            assert_eq!(app.capture_session().phase(), None);
            assert_eq!(app.capture_session().managed_capture_id(), None);
            assert_eq!(app.managed_release_in_flight(), None);
            assert!(app.pending_managed_releases.is_empty());
        }
    }

    #[test]
    fn mixer_task8_keys_commit_exact_audio_first_values_and_preserve_failures_and_noops() {
        let (mut app, probe, target) = loaded_mixer_app();
        app.patterns.set_view(WorkspaceView::Mixer);
        let revision = app.project_revision();

        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let raised = PadSettings::new(PlaybackMode::OneShot, 1.0, 0.0, 0.0, None).unwrap();
        assert_eq!(app.pad(target).settings, raised);
        assert_eq!(app.project_revision(), revision + 1);
        assert_eq!(
            probe.calls().last(),
            Some(&MixerAudioCall::UpdatePad(target, raised))
        );

        app.apply_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let muted = PadMixSettings::new(true, 0.0, 0.0).unwrap();
        assert_eq!(app.pad_mix(target), muted);
        assert_eq!(
            probe.calls().last(),
            Some(&MixerAudioCall::UpdatePadMix(target, muted))
        );

        app.apply_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(app.mixer_cursor().section(), MixerSection::Reverb);
        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.mixer_cursor().section(), MixerSection::Master);
        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let master =
            MasterMixSettings::new(1.0, DelaySettings::default(), ReverbSettings::default())
                .unwrap();
        assert_eq!(app.master_mix(), master);
        assert_eq!(
            probe.calls().last(),
            Some(&MixerAudioCall::UpdateMasterMix(master))
        );

        probe.fail_next(MixerAudioCallKind::UpdateMasterMix);
        let before = (app.master_mix(), app.project_revision());
        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!((app.master_mix(), app.project_revision()), before);
        assert_eq!(
            probe.calls().last(),
            Some(&MixerAudioCall::UpdateMasterMix(
                MasterMixSettings::new(2.0, DelaySettings::default(), ReverbSettings::default(),)
                    .unwrap()
            ))
        );

        app.update_master_mix(
            MasterMixSettings::new(6.0, DelaySettings::default(), ReverbSettings::default())
                .unwrap(),
        )
        .unwrap();
        let before = (app.project_revision(), probe.calls());
        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!((app.project_revision(), probe.calls()), before);
    }

    #[test]
    fn mixer_task8_global_routing_pad_selection_and_four_view_keys_have_priority() {
        let (mut app, probe, _) = loaded_mixer_app();
        app.patterns.set_view(WorkspaceView::Mixer);
        assert_eq!(app.mixer_cursor().pad_field(), PadField::Level);

        app.set_keyboard_capabilities(KeyboardCapabilities {
            release_events: true,
        });
        app.apply_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert_eq!(app.selected_pad(), 5);
        assert_eq!(probe.calls().last(), Some(&MixerAudioCall::Trigger(pad(5))));
        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Char('w'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        ));
        assert_eq!(probe.calls().last(), Some(&MixerAudioCall::Release(pad(5))));

        app.apply_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_eq!(app.overlay(), Some(&Overlay::Help));
        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.mixer_cursor().section(), MixerSection::Pad);
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        app.apply_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE));
        assert_eq!(app.overlay(), Some(&Overlay::Palette));
        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.mixer_cursor().section(), MixerSection::Pad);
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        app.overlay = Some(Overlay::ProjectSaveProgress);
        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(app.mixer_cursor().section(), MixerSection::Pad);
        app.overlay = None;

        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.workspace_view(), WorkspaceView::Perform);
        for expected in [
            WorkspaceView::Pattern,
            WorkspaceView::Sample,
            WorkspaceView::Mixer,
            WorkspaceView::Perform,
        ] {
            app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert_eq!(app.workspace_view(), expected);
        }
        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Mixer);
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
    }
}

#[cfg(test)]
mod capture_task7_tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use sampler_audio::{
        CaptureBuffer, CaptureCompletion, CaptureOutcome, CaptureSource, CaptureStatus, Frame,
        LiveCommandId, MAX_CAPTURE_FRAMES, PatternSnapshotSlot, SampleBuffer, SampleSlot,
        Telemetry,
    };
    use sampler_core::{BankId, PadId, PadSettings, PatternSnapshot, ProjectId, SampleEditRecipe};

    use super::{
        App, EDIT_PREVIEW_COLUMNS, Overlay, PadLoadState, PendingProjectAction, PreviewColumn,
        ProjectAction,
    };
    use crate::audio::{AudioPort, CaptureCommandFailure, CaptureSupport};
    use crate::capture::{CaptureError, CaptureFailureCause, CapturePhase};
    use crate::capture_store::{CaptureStore, CaptureStoreError, ManagedCapture, ManagedCaptureId};
    use crate::input::InputAction;
    use crate::loader::{
        CaptureFinalizeError, FinalizeCaptureRequest, LoadPurpose, LoadedSample,
        ProjectSaveWorkerRequest, RenderedSample, WORKER_CHANNEL_CAPACITY, WorkerRequest,
        WorkerResult, WorkerSendError,
    };
    use crate::project_session::ProjectSnapshotError;
    use crate::project_store::{ProjectAssetMapping, SaveKind, SaveReceipt, SourceFingerprint};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CaptureCall {
        Begin {
            token: u64,
            target: PadId,
            source: CaptureSource,
            rate: u32,
            max_frames: usize,
        },
        Start(CaptureSource, u64),
        Stop(CaptureSource, u64),
        Cancel(CaptureSource, u64),
        Trigger(PadId),
        Release(PadId),
        StopAll,
        Install(PadId, usize),
        Remove(PadId),
    }

    struct CaptureAudioState {
        output_rate: Cell<u32>,
        input_rate: u32,
        calls: Vec<CaptureCall>,
        armed: Option<CaptureBuffer>,
        outcomes: VecDeque<CaptureOutcome>,
        runtime_errors: VecDeque<(CaptureSource, CaptureError)>,
        output_session_error: Option<String>,
        install_failures: usize,
        installed: Vec<Arc<SampleBuffer>>,
    }

    #[derive(Clone)]
    struct CaptureProbe(Rc<RefCell<CaptureAudioState>>);

    impl CaptureProbe {
        fn calls(&self) -> Vec<CaptureCall> {
            self.0.borrow().calls.clone()
        }

        fn complete(&self, completion: CaptureCompletion) {
            let mut state = self.0.borrow_mut();
            state.armed = None;
            state
                .outcomes
                .push_back(CaptureOutcome::Completed(completion));
        }

        fn fail_device(&self, source: CaptureSource, message: &str) {
            let error = match source {
                CaptureSource::Resample => CaptureError::OutputRuntime(message.to_owned()),
                CaptureSource::Input => CaptureError::InputRuntime(message.to_owned()),
            };
            self.0
                .borrow_mut()
                .runtime_errors
                .push_back((source, error));
        }

        fn fail_next_install(&self) {
            self.0.borrow_mut().install_failures += 1;
        }

        fn fail_output_session(&self, message: &str) {
            self.0.borrow_mut().output_session_error = Some(message.to_owned());
        }

        fn set_output_rate(&self, rate: u32) {
            self.0.borrow().output_rate.set(rate);
        }
    }

    struct CaptureAudio(CaptureProbe);

    impl CaptureAudio {
        fn new(output_rate: u32, input_rate: u32) -> (Self, CaptureProbe) {
            let probe = CaptureProbe(Rc::new(RefCell::new(CaptureAudioState {
                output_rate: Cell::new(output_rate),
                input_rate,
                calls: Vec::new(),
                armed: None,
                outcomes: VecDeque::new(),
                runtime_errors: VecDeque::new(),
                output_session_error: None,
                install_failures: 0,
                installed: Vec::new(),
            })));
            (Self(probe.clone()), probe)
        }
    }

    impl AudioPort for CaptureAudio {
        fn sample_rate(&self) -> u32 {
            self.0.0.borrow().output_rate.get()
        }
        fn channels(&self) -> u16 {
            2
        }
        fn render_horizon(&self) -> Frame {
            0
        }
        fn install(
            &mut self,
            pad: PadId,
            sample: Arc<SampleBuffer>,
            _settings: PadSettings,
            _mix: sampler_core::PadMixSettings,
        ) -> Result<SampleSlot, String> {
            let mut state = self.0.0.borrow_mut();
            state
                .calls
                .push(CaptureCall::Install(pad, Arc::as_ptr(&sample) as usize));
            if state.install_failures > 0 {
                state.install_failures -= 1;
                return Err("audio install queue full".to_owned());
            }
            state.installed.push(sample);
            SampleSlot::new(0).map_err(|error| error.to_string())
        }
        fn trigger(&mut self, pad: PadId, _at: Frame, _velocity: f32) -> Result<(), String> {
            self.0.0.borrow_mut().calls.push(CaptureCall::Trigger(pad));
            Ok(())
        }
        fn trigger_live_tracked(
            &mut self,
            _pad: PadId,
            _velocity: f32,
        ) -> Result<LiveCommandId, String> {
            Ok(LiveCommandId::FIRST)
        }
        fn release(&mut self, pad: PadId, _at: Frame) -> Result<(), String> {
            self.0.0.borrow_mut().calls.push(CaptureCall::Release(pad));
            Ok(())
        }
        fn release_live_tracked(&mut self, _pad: PadId) -> Result<LiveCommandId, String> {
            Ok(LiveCommandId::FIRST)
        }
        fn install_pattern(
            &mut self,
            _snapshot: Arc<PatternSnapshot>,
        ) -> Result<PatternSnapshotSlot, String> {
            Err("unused".to_owned())
        }
        fn remove_sample(&mut self, pad: PadId) -> Result<(), String> {
            self.0.0.borrow_mut().calls.push(CaptureCall::Remove(pad));
            Ok(())
        }
        fn stop_pad(&mut self, _pad: PadId) -> Result<(), String> {
            Ok(())
        }
        fn stop_all(&mut self) -> Result<(), String> {
            self.0.0.borrow_mut().calls.push(CaptureCall::StopAll);
            Ok(())
        }
        fn update_pad(&mut self, _pad: PadId, _settings: PadSettings) -> Result<(), String> {
            Ok(())
        }
        fn update_pad_mix(
            &mut self,
            _pad: PadId,
            _settings: sampler_core::PadMixSettings,
        ) -> Result<(), String> {
            Ok(())
        }
        fn update_master_mix(
            &mut self,
            _settings: sampler_core::MasterMixSettings,
        ) -> Result<(), String> {
            Ok(())
        }
        fn reclaim_retired(&mut self) -> usize {
            0
        }
        fn latest_telemetry(&mut self) -> Option<Telemetry> {
            None
        }
        fn poll_runtime_error(&mut self) -> Option<String> {
            self.0.0.borrow_mut().output_session_error.take()
        }
        fn capture_support(&self) -> CaptureSupport {
            CaptureSupport::Available
        }
        fn capture_source_rate(&mut self, source: CaptureSource) -> Result<u32, CaptureError> {
            let state = self.0.0.borrow();
            Ok(match source {
                CaptureSource::Resample => state.output_rate.get(),
                CaptureSource::Input => state.input_rate,
            })
        }
        fn begin_capture(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureCommandFailure> {
            let mut state = self.0.0.borrow_mut();
            state.calls.push(CaptureCall::Begin {
                token: buffer.token(),
                target: buffer.target(),
                source: buffer.source(),
                rate: buffer.sample_rate(),
                max_frames: buffer.max_frames(),
            });
            state.armed = Some(buffer);
            Ok(())
        }
        fn start_capture(
            &mut self,
            source: CaptureSource,
            token: u64,
        ) -> Result<(), CaptureCommandFailure> {
            self.0
                .0
                .borrow_mut()
                .calls
                .push(CaptureCall::Start(source, token));
            Ok(())
        }
        fn stop_capture(
            &mut self,
            source: CaptureSource,
            token: u64,
        ) -> Result<(), CaptureCommandFailure> {
            self.0
                .0
                .borrow_mut()
                .calls
                .push(CaptureCall::Stop(source, token));
            Ok(())
        }
        fn cancel_capture(
            &mut self,
            source: CaptureSource,
            token: u64,
        ) -> Result<(), CaptureCommandFailure> {
            let mut state = self.0.0.borrow_mut();
            state.calls.push(CaptureCall::Cancel(source, token));
            if let Some(buffer) = state.armed.take() {
                state.outcomes.push_back(CaptureOutcome::Cancelled(buffer));
            }
            Ok(())
        }
        fn capture_status(&mut self, _source: CaptureSource) -> Option<CaptureStatus> {
            None
        }
        fn capture_completion(&mut self, source: CaptureSource) -> Option<CaptureOutcome> {
            let mut state = self.0.0.borrow_mut();
            let position = state.outcomes.iter().position(|outcome| match outcome {
                CaptureOutcome::Completed(completion) => completion.source == source,
                CaptureOutcome::Cancelled(buffer) => buffer.source() == source,
            })?;
            state.outcomes.remove(position)
        }
        fn capture_runtime_error(&mut self, source: CaptureSource) -> Option<CaptureError> {
            let mut state = self.0.0.borrow_mut();
            let position = state
                .runtime_errors
                .iter()
                .position(|(candidate, _)| *candidate == source)?;
            state
                .runtime_errors
                .remove(position)
                .map(|(_, error)| error)
        }
    }

    fn pad(index: u8) -> PadId {
        PadId::new(BankId::new(0).unwrap(), index).unwrap()
    }

    fn fingerprint(bytes: &[u8]) -> SourceFingerprint {
        SourceFingerprint::from_encoded_bytes(Path::new("fixture.wav"), bytes).unwrap()
    }

    fn loaded_result(target: PadId, generation: u64, source: &str) -> WorkerResult {
        loaded_result_with_purpose(target, generation, LoadPurpose::User, source, 48_000)
    }

    fn loaded_result_with_purpose(
        target: PadId,
        generation: u64,
        purpose: LoadPurpose,
        source: &str,
        engine_rate: u32,
    ) -> WorkerResult {
        let rendered = Arc::new(SampleBuffer::new(engine_rate, vec![0.2, -0.2]).unwrap());
        WorkerResult::Loaded {
            pad: target,
            generation,
            purpose,
            path: source.into(),
            result: Ok(LoadedSample {
                fingerprint: fingerprint(source.as_bytes()),
                base: Arc::clone(&rendered),
                base_preview: Arc::new([PreviewColumn { min: -3, max: 3 }; EDIT_PREVIEW_COLUMNS]),
                rendered,
                rendered_preview: Arc::new(
                    [PreviewColumn { min: -3, max: 3 }; EDIT_PREVIEW_COLUMNS],
                ),
                recipe: SampleEditRecipe::identity(),
                source_rate: engine_rate,
                source_frames: 1,
                duration: std::time::Duration::from_secs_f64(1.0 / f64::from(engine_rate)),
            }),
        }
    }

    fn install_imported(app: &mut App, target: PadId, source: &str) {
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(target, source).expect("load request")
        else {
            panic!("expected load request")
        };
        assert!(app.apply_worker_result(loaded_result(target, generation, source)));
    }

    #[derive(Debug, Clone, PartialEq)]
    struct CaptureCompletionSnapshot {
        token: u64,
        target: PadId,
        source: CaptureSource,
        sample_rate: u32,
        stereo: Vec<f32>,
        hard_limit: bool,
        peak_bits: u32,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct CaptureSessionSnapshot {
        sequence: (u64, u64),
        token: Option<u64>,
        generation: Option<u64>,
        target: Option<PadId>,
        source: Option<CaptureSource>,
        source_rate: Option<u32>,
        max_frames: Option<usize>,
        phase: Option<CapturePhase>,
        completion: Option<CaptureCompletionSnapshot>,
        failure: Option<String>,
        failure_cause: Option<CaptureFailureCause>,
        failure_is_retryable: bool,
        managed_capture_id: Option<ManagedCaptureId>,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct PadSnapshot {
        source: Option<PathBuf>,
        label: String,
        settings: PadSettings,
        generation: u64,
        state: PadLoadState,
        sample: Option<Arc<SampleBuffer>>,
        preview: [PreviewColumn; super::PREVIEW_COLUMNS],
        active: bool,
        base: Option<Arc<SampleBuffer>>,
        source_generation: u64,
        fingerprint: Option<SourceFingerprint>,
        recipe: SampleEditRecipe,
        base_preview: Option<Arc<[PreviewColumn; EDIT_PREVIEW_COLUMNS]>>,
        rendered_preview: Option<Arc<[PreviewColumn; EDIT_PREVIEW_COLUMNS]>>,
        managed_capture: Option<ManagedCaptureId>,
        current_session_bound: bool,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct CaptureAdmissionSnapshot {
        session: CaptureSessionSnapshot,
        overlay: Option<Overlay>,
        status: String,
        capture_status: Option<CaptureStatus>,
        pad: PadSnapshot,
        revision: u64,
        audio_calls: Vec<CaptureCall>,
    }

    fn capture_admission_snapshot(
        app: &App,
        probe: &CaptureProbe,
        target: PadId,
    ) -> CaptureAdmissionSnapshot {
        let session = app.capture_session();
        CaptureAdmissionSnapshot {
            session: CaptureSessionSnapshot {
                sequence: session.sequence_for_test(),
                token: session.token(),
                generation: session.generation(),
                target: session.target(),
                source: session.source(),
                source_rate: session.source_rate(),
                max_frames: session.max_frames(),
                phase: session.phase(),
                completion: session
                    .completion()
                    .map(|completion| CaptureCompletionSnapshot {
                        token: completion.token,
                        target: completion.target,
                        source: completion.source,
                        sample_rate: completion.sample_rate,
                        stereo: completion.stereo.clone(),
                        hard_limit: completion.hard_limit,
                        peak_bits: completion.peak.to_bits(),
                    }),
                failure: session.failure().map(str::to_owned),
                failure_cause: session.failure_cause(),
                failure_is_retryable: session.failure_is_retryable(),
                managed_capture_id: session.managed_capture_id(),
            },
            overlay: app.overlay().cloned(),
            status: app.status().to_owned(),
            capture_status: app.capture_status_view(),
            pad: {
                let offset = super::pad_offset(target);
                let pad = app.pad(target);
                let commit = &app.sample_editor.commits[offset];
                PadSnapshot {
                    source: pad.source.clone(),
                    label: pad.label.clone(),
                    settings: pad.settings,
                    generation: pad.generation,
                    state: pad.state.clone(),
                    sample: pad.sample.clone(),
                    preview: pad.preview,
                    active: pad.active,
                    base: commit.base.clone(),
                    source_generation: commit.source_generation,
                    fingerprint: commit.fingerprint,
                    recipe: commit.recipe,
                    base_preview: commit.base_preview.clone(),
                    rendered_preview: commit.rendered_preview.clone(),
                    managed_capture: commit.managed_capture,
                    current_session_bound: app.current_session_bound[offset],
                }
            },
            revision: app.project_revision(),
            audio_calls: probe.calls(),
        }
    }

    fn oversized_frame_limit_error() -> CaptureError {
        CaptureError::Command(sampler_audio::CaptureError::FrameLimitTooLarge {
            max_frames: MAX_CAPTURE_FRAMES + 1,
        })
    }

    fn completion(
        app: &App,
        source: CaptureSource,
        stereo: Vec<f32>,
        hard_limit: bool,
    ) -> CaptureCompletion {
        CaptureCompletion {
            token: app.capture_session().token().unwrap(),
            target: app.capture_session().target().unwrap(),
            source,
            sample_rate: app.capture_session().source_rate().unwrap(),
            stereo,
            hard_limit,
            peak: 0.75,
        }
    }

    fn take_finalize(app: &mut App) -> FinalizeCaptureRequest {
        let requests = app.take_worker_requests();
        let [WorkerRequest::FinalizeCapture(request)] = requests.as_slice() else {
            panic!("expected one finalize request, got {requests:?}")
        };
        request.clone()
    }

    fn managed_capture(id: u64, rate: u32, bytes: &[u8]) -> ManagedCapture {
        let rendered = Arc::new(SampleBuffer::new(rate, vec![0.7, -0.7, 0.4, -0.4]).unwrap());
        let source_fingerprint = fingerprint(bytes);
        ManagedCapture {
            id: ManagedCaptureId::new(id),
            path: PathBuf::from(format!("managed-{id}.wav")),
            fingerprint: source_fingerprint,
            sample: LoadedSample {
                fingerprint: source_fingerprint,
                base: Arc::clone(&rendered),
                base_preview: Arc::new([PreviewColumn { min: -7, max: 7 }; EDIT_PREVIEW_COLUMNS]),
                rendered,
                rendered_preview: Arc::new(
                    [PreviewColumn { min: -6, max: 6 }; EDIT_PREVIEW_COLUMNS],
                ),
                recipe: SampleEditRecipe::identity(),
                source_rate: rate,
                source_frames: 2,
                duration: std::time::Duration::from_secs_f64(2.0 / f64::from(rate)),
            },
        }
    }

    fn finalized(
        request: &FinalizeCaptureRequest,
        result: Result<ManagedCapture, CaptureFinalizeError>,
    ) -> WorkerResult {
        WorkerResult::CaptureFinalized {
            token: request.token,
            generation: request.generation,
            target: request.target,
            source: request.source,
            source_rate: request.source_rate,
            engine_rate: request.engine_rate,
            stereo: Arc::clone(&request.stereo),
            hard_limit: request.hard_limit,
            result,
        }
    }

    fn start_capture(app: &mut App, source: CaptureSource, frames: usize) {
        app.request_capture_with_limit_for_test(source, frames)
            .unwrap();
        if app.capture_session().phase() == Some(CapturePhase::Confirm) {
            app.confirm_capture().unwrap();
        }
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Recording));
    }

    fn drive_ready(
        app: &mut App,
        probe: &CaptureProbe,
        source: CaptureSource,
        id: u64,
        hard_limit: bool,
    ) -> FinalizeCaptureRequest {
        probe.complete(completion(app, source, vec![0.25, -0.25], hard_limit));
        assert!(app.maintain_capture());
        assert!(app.take_worker_requests().is_empty());
        assert!(app.maintain_capture());
        let request = take_finalize(app);
        assert!(app.apply_worker_result(finalized(
            &request,
            Ok(managed_capture(id, request.engine_rate, b"captured")),
        )));
        assert!(app.maintain_capture());
        assert_eq!(
            app.capture_session().phase(),
            Some(CapturePhase::ReadyToInstall)
        );
        request
    }

    #[test]
    fn capture_transaction_empty_pad_uses_one_boundary_action_per_maintenance_and_commits_full_tuple_once()
     {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0);

        start_capture(&mut app, CaptureSource::Resample, 4);
        let token = app.capture_session().token().unwrap();
        assert_eq!(
            probe.calls(),
            [
                CaptureCall::Begin {
                    token,
                    target,
                    source: CaptureSource::Resample,
                    rate: 48_000,
                    max_frames: 4,
                },
                CaptureCall::Start(CaptureSource::Resample, token),
            ]
        );
        app.stop_capture().unwrap();
        assert_eq!(
            probe.calls().last(),
            Some(&CaptureCall::Stop(CaptureSource::Resample, token))
        );

        drive_ready(&mut app, &probe, CaptureSource::Resample, 7, true);
        assert_eq!(app.project_revision(), 0);
        assert!(app.pad(target).sample.is_none());
        assert!(app.maintain_capture());

        let snapshot = app.project_snapshot().unwrap();
        assert_eq!(app.project_revision(), 1);
        assert_eq!(snapshot.revision, 1);
        assert_eq!(snapshot.pads.len(), 1);
        assert_eq!(snapshot.pads[0].source_path, PathBuf::from("managed-7.wav"));
        assert_eq!(snapshot.pads[0].fingerprint, fingerprint(b"captured"));
        assert_eq!(snapshot.pads[0].recipe, SampleEditRecipe::identity());
        assert_eq!(app.pad(target).state, PadLoadState::Ready);
        assert_eq!(app.capture_session().phase(), None);
        assert!(app.status().contains("MAX"));
    }

    #[test]
    fn capture_transaction_occupied_pad_waits_for_confirmation_and_preserves_exact_candidate_through_busy_failure_and_install_retry()
     {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0);
        install_imported(&mut app, target, "old.wav");
        let old_sample = Arc::clone(app.pad(target).sample.as_ref().unwrap());
        let old_revision = app.project_revision();

        app.request_capture_with_limit_for_test(CaptureSource::Resample, 4)
            .unwrap();
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Confirm));
        assert!(
            !probe
                .calls()
                .iter()
                .any(|call| matches!(call, CaptureCall::Begin { .. }))
        );
        app.confirm_capture().unwrap();
        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.3, -0.3],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let first = take_finalize(&mut app);
        let pointer = Arc::as_ptr(&first.stereo);
        assert!(app.apply_worker_send_error(
            WorkerRequest::FinalizeCapture(first.clone()),
            WorkerSendError::WorkerBusy,
        ));
        assert!(app.maintain_capture());
        let retry = take_finalize(&mut app);
        assert!(Arc::ptr_eq(&first.stereo, &retry.stereo));
        assert_eq!(Arc::as_ptr(&retry.stereo), pointer);
        assert!(Arc::ptr_eq(
            app.pad(target).sample.as_ref().unwrap(),
            &old_sample
        ));
        assert_eq!(app.project_revision(), old_revision);

        assert!(app.apply_worker_result(finalized(
            &retry,
            Err(CaptureFinalizeError::Prepare("encode failed".to_owned())),
        )));
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
        app.retry_capture_finalization().unwrap();
        assert!(app.maintain_capture());
        let second = take_finalize(&mut app);
        assert!(Arc::ptr_eq(&retry.stereo, &second.stereo));
        assert!(
            app.apply_worker_result(finalized(&second, Ok(managed_capture(8, 48_000, b"retry")),))
        );
        assert!(app.maintain_capture());
        probe.fail_next_install();
        assert!(app.maintain_capture());
        assert_eq!(
            app.capture_session().phase(),
            Some(CapturePhase::ReadyToInstall)
        );
        assert_eq!(
            app.capture_session().managed_capture_id(),
            Some(ManagedCaptureId::new(8))
        );
        assert!(Arc::ptr_eq(
            app.pad(target).sample.as_ref().unwrap(),
            &old_sample
        ));
        assert_eq!(app.project_revision(), old_revision);
        assert!(app.maintain_capture());
        assert_eq!(app.project_revision(), old_revision + 1);
        assert!(!Arc::ptr_eq(
            app.pad(target).sample.as_ref().unwrap(),
            &old_sample
        ));
    }

    #[test]
    fn capture_transaction_rejects_dirty_project_state_empty_and_stale_results_without_mutation() {
        let (audio, _probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        install_imported(&mut app, pad(0), "draft.wav");
        app.editor_mut_for_test().toggle_reverse();
        assert_eq!(
            app.request_capture_with_limit_for_test(CaptureSource::Resample, 4),
            Err(CaptureError::DirtySampleDraft(app.editor.pad()))
        );
        app.discard_sample_draft();
        app.request_save_as("pending-project").unwrap();
        assert_eq!(
            app.request_capture_with_limit_for_test(CaptureSource::Resample, 4),
            Err(CaptureError::ProjectOperationPending)
        );
        app.pending_explicit_save = None;

        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));

        start_capture(&mut app, CaptureSource::Input, 4);
        probe.complete(completion(&app, CaptureSource::Input, Vec::new(), false));
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
        assert_eq!(app.project_revision(), 0);
        assert!(app.pad(pad(0)).sample.is_none());
        app.cancel_capture().unwrap();

        start_capture(&mut app, CaptureSource::Input, 4);
        probe.complete(completion(
            &app,
            CaptureSource::Input,
            vec![0.1, -0.1],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let request = take_finalize(&mut app);
        for stale in [
            WorkerResult::CaptureFinalized {
                token: request.token + 1,
                generation: request.generation,
                target: request.target,
                source: request.source,
                source_rate: request.source_rate,
                engine_rate: request.engine_rate,
                stereo: Arc::clone(&request.stereo),
                hard_limit: request.hard_limit,
                result: Err(CaptureFinalizeError::Prepare("stale token".to_owned())),
            },
            WorkerResult::CaptureFinalized {
                token: request.token,
                generation: request.generation + 1,
                target: request.target,
                source: request.source,
                source_rate: request.source_rate + 1,
                engine_rate: request.engine_rate,
                stereo: Arc::clone(&request.stereo),
                hard_limit: request.hard_limit,
                result: Err(CaptureFinalizeError::Prepare("stale fence".to_owned())),
            },
            WorkerResult::CaptureFinalized {
                token: request.token,
                generation: request.generation,
                target: pad(1),
                source: request.source,
                source_rate: request.source_rate,
                engine_rate: request.engine_rate,
                stereo: Arc::clone(&request.stereo),
                hard_limit: request.hard_limit,
                result: Err(CaptureFinalizeError::Prepare("stale target".to_owned())),
            },
            WorkerResult::CaptureFinalized {
                token: request.token,
                generation: request.generation,
                target: request.target,
                source: CaptureSource::Resample,
                source_rate: request.source_rate,
                engine_rate: request.engine_rate,
                stereo: Arc::clone(&request.stereo),
                hard_limit: request.hard_limit,
                result: Err(CaptureFinalizeError::Prepare("stale source".to_owned())),
            },
            WorkerResult::CaptureFinalized {
                token: request.token,
                generation: request.generation,
                target: request.target,
                source: request.source,
                source_rate: request.source_rate,
                engine_rate: request.engine_rate + 1,
                stereo: Arc::clone(&request.stereo),
                hard_limit: request.hard_limit,
                result: Err(CaptureFinalizeError::Prepare(
                    "stale engine rate".to_owned(),
                )),
            },
        ] {
            assert!(app.apply_worker_result(stale));
            assert!(!app.maintain_capture());
            assert_eq!(
                app.capture_session().phase(),
                Some(CapturePhase::Finalizing)
            );
            assert_eq!(app.capture_session().generation(), Some(request.generation));
        }
        assert_eq!(app.project_revision(), 0);
    }

    #[test]
    fn capture_transaction_each_stale_success_fence_preserves_the_pad_and_releases_its_exact_artifact()
     {
        #[derive(Debug, Clone, Copy)]
        enum Fence {
            Token,
            Generation,
            Target,
            Source,
            NativeRate,
            EngineRate,
            HardLimit,
            SourceArc,
        }

        for (index, fence) in [
            Fence::Token,
            Fence::Generation,
            Fence::Target,
            Fence::Source,
            Fence::NativeRate,
            Fence::EngineRate,
            Fence::HardLimit,
            Fence::SourceArc,
        ]
        .into_iter()
        .enumerate()
        {
            let (audio, probe) = CaptureAudio::new(48_000, 44_100);
            let mut app = App::with_audio(Box::new(audio));
            let target = pad(0);
            install_imported(&mut app, target, "old-stale.wav");
            let old_sample = Arc::clone(app.pad(target).sample.as_ref().unwrap());
            let old_source = app.pad(target).source.clone();
            let old_settings = app.pad(target).settings;
            let old_generation = app.pad(target).generation;
            let old_revision = app.project_revision();
            let old_fingerprint = app.sample_editor.commits[0].fingerprint;
            let old_recipe = app.sample_editor.commits[0].recipe;

            start_capture(&mut app, CaptureSource::Input, 4);
            probe.complete(completion(
                &app,
                CaptureSource::Input,
                vec![0.3, -0.3],
                false,
            ));
            assert!(app.maintain_capture());
            assert!(app.maintain_capture());
            let request = take_finalize(&mut app);
            let artifact_id = ManagedCaptureId::new(100 + index as u64);
            let mut token = request.token;
            let mut generation = request.generation;
            let mut stale_target = request.target;
            let mut source = request.source;
            let mut source_rate = request.source_rate;
            let mut engine_rate = request.engine_rate;
            let mut stereo = Arc::clone(&request.stereo);
            let mut hard_limit = request.hard_limit;
            match fence {
                Fence::Token => token += 1,
                Fence::Generation => generation += 1,
                Fence::Target => stale_target = pad(1),
                Fence::Source => source = CaptureSource::Resample,
                Fence::NativeRate => source_rate += 1,
                Fence::EngineRate => engine_rate += 1,
                Fence::HardLimit => hard_limit = !hard_limit,
                Fence::SourceArc => stereo = Arc::from(request.stereo.as_ref()),
            }
            let stale = WorkerResult::CaptureFinalized {
                token,
                generation,
                target: stale_target,
                source,
                source_rate,
                engine_rate,
                stereo,
                hard_limit,
                result: Ok(managed_capture(
                    artifact_id.get(),
                    request.engine_rate,
                    format!("stale-{index}").as_bytes(),
                )),
            };

            assert!(app.apply_worker_result(stale), "fence {fence:?}");
            assert!(!app.maintain_capture(), "fence {fence:?}");
            assert_eq!(
                app.capture_session().phase(),
                Some(CapturePhase::Finalizing),
                "fence {fence:?}"
            );
            assert_eq!(app.project_revision(), old_revision, "fence {fence:?}");
            assert_eq!(app.pad(target).source, old_source, "fence {fence:?}");
            assert_eq!(app.pad(target).settings, old_settings, "fence {fence:?}");
            assert_eq!(
                app.pad(target).generation,
                old_generation,
                "fence {fence:?}"
            );
            assert!(
                Arc::ptr_eq(app.pad(target).sample.as_ref().unwrap(), &old_sample),
                "fence {fence:?}"
            );
            assert_eq!(
                app.sample_editor.commits[0].fingerprint, old_fingerprint,
                "fence {fence:?}"
            );
            assert_eq!(
                app.sample_editor.commits[0].recipe, old_recipe,
                "fence {fence:?}"
            );

            app.cancel_capture().unwrap();
            assert!(app.maintain_capture(), "fence {fence:?}");
            assert_eq!(
                app.take_worker_requests(),
                [WorkerRequest::ReleaseManagedCapture { id: artifact_id }],
                "fence {fence:?}"
            );
        }
    }

    #[test]
    fn capture_transaction_changed_output_rate_refinalizes_only_input_and_device_loss_never_resumes()
     {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        install_imported(&mut app, pad(0), "old-device.wav");
        let old_sample = Arc::clone(app.pad(pad(0)).sample.as_ref().unwrap());
        let old_revision = app.project_revision();
        start_capture(&mut app, CaptureSource::Input, 4);
        probe.complete(completion(
            &app,
            CaptureSource::Input,
            vec![0.2, -0.2],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let request = take_finalize(&mut app);
        let generation = request.generation;
        probe.set_output_rate(44_100);
        assert!(app.apply_worker_result(finalized(
            &request,
            Ok(managed_capture(9, 48_000, b"old-rate")),
        )));
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().generation(), Some(generation + 1));
        assert_eq!(
            app.capture_session().phase(),
            Some(CapturePhase::Finalizing)
        );
        assert!(app.maintain_capture());
        let rerender = take_finalize(&mut app);
        assert_eq!(rerender.engine_rate, 44_100);
        assert!(Arc::ptr_eq(&request.stereo, &rerender.stereo));

        probe.fail_device(CaptureSource::Input, "input disconnected");
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
        assert!(app.audio.is_none());
        assert!(Arc::ptr_eq(
            app.pad(pad(0)).sample.as_ref().unwrap(),
            &old_sample
        ));
        assert_eq!(app.project_revision(), old_revision);
        let calls = probe.calls().len();
        let (replacement, replacement_probe) = CaptureAudio::new(44_100, 44_100);
        app.retry_default_device_with(|| Ok(Box::new(replacement)));
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
        assert!(replacement_probe.calls().is_empty());
        assert_eq!(probe.calls().len(), calls);
        assert!(Arc::ptr_eq(
            app.capture_source_pcm.as_ref().unwrap(),
            &rerender.stereo
        ));
        assert_eq!(
            app.capture_session().failure_cause(),
            Some(CaptureFailureCause::DeviceRuntime)
        );
        assert_eq!(
            app.retry_capture_finalization(),
            Err(CaptureError::RetryNotAllowed(
                CaptureFailureCause::DeviceRuntime
            ))
        );
        assert!(Arc::ptr_eq(
            app.capture_source_pcm.as_ref().unwrap(),
            &rerender.stereo
        ));

        app.cancel_capture().unwrap();
        assert!(Arc::ptr_eq(
            app.capture_source_pcm.as_ref().unwrap(),
            &rerender.stereo
        ));
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
        assert!(app.apply_worker_result(finalized(
            &rerender,
            Err(CaptureFinalizeError::Prepare(
                "discarded after device loss".to_owned(),
            )),
        )));
        assert!(app.maintain_capture());
        assert!(app.capture_source_pcm.is_none());
        assert_eq!(app.capture_session().phase(), None);
        let recovery = app.take_worker_requests();
        let [
            WorkerRequest::LoadSample {
                pad: recovery_pad,
                generation,
                purpose,
                path,
                engine_rate,
                ..
            },
        ] = recovery.as_slice()
        else {
            panic!("expected the old pad recovery request, got {recovery:?}")
        };
        assert!(app.apply_worker_result(loaded_result_with_purpose(
            *recovery_pad,
            *generation,
            *purpose,
            path.to_str().unwrap(),
            *engine_rate,
        )));
        assert!(app.maintain_audio());
        assert!(app.select_pad(1));
        start_capture(&mut app, CaptureSource::Input, 4);
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Recording));
        let fresh_token = app.capture_session().token().unwrap();
        assert!(replacement_probe.calls().ends_with(&[
            CaptureCall::Begin {
                token: fresh_token,
                target: pad(1),
                source: CaptureSource::Input,
                rate: 44_100,
                max_frames: 4,
            },
            CaptureCall::Start(CaptureSource::Input, fresh_token),
        ]));
    }

    #[test]
    fn capture_transaction_output_runtime_loss_is_terminal_for_the_retained_take() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        start_capture(&mut app, CaptureSource::Resample, 4);
        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.2, -0.2],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let request = take_finalize(&mut app);

        probe.fail_device(CaptureSource::Resample, "output capture disconnected");
        assert!(app.maintain_capture());
        assert_eq!(
            app.capture_session().failure_cause(),
            Some(CaptureFailureCause::DeviceRuntime)
        );
        assert!(Arc::ptr_eq(
            app.capture_source_pcm.as_ref().unwrap(),
            &request.stereo
        ));

        let (replacement, _) = CaptureAudio::new(48_000, 44_100);
        app.retry_default_device_with(|| Ok(Box::new(replacement)));
        assert_eq!(
            app.retry_capture_finalization(),
            Err(CaptureError::RetryNotAllowed(
                CaptureFailureCause::DeviceRuntime
            ))
        );
        assert!(Arc::ptr_eq(
            app.capture_source_pcm.as_ref().unwrap(),
            &request.stereo
        ));
    }

    #[test]
    fn capture_transaction_worker_error_is_retryable_but_explicit_discard_releases_the_source() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        start_capture(&mut app, CaptureSource::Input, 4);
        probe.complete(completion(
            &app,
            CaptureSource::Input,
            vec![0.4, -0.4],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let request = take_finalize(&mut app);
        assert!(app.apply_worker_result(finalized(
            &request,
            Err(CaptureFinalizeError::Prepare("encode failed".to_owned())),
        )));
        assert!(app.maintain_capture());
        assert_eq!(
            app.capture_session().failure_cause(),
            Some(CaptureFailureCause::WorkerFinalization)
        );
        assert!(app.capture_session().failure_is_retryable());
        assert!(Arc::ptr_eq(
            app.capture_source_pcm.as_ref().unwrap(),
            &request.stereo
        ));

        app.cancel_capture().unwrap();
        assert_eq!(app.capture_session().phase(), None);
        assert!(app.capture_source_pcm.is_none());
        assert!(app.capture_ready.is_none());
        assert!(app.pending_managed_releases.is_empty());

        start_capture(&mut app, CaptureSource::Input, 4);
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Recording));
    }

    #[test]
    fn capture_transaction_general_output_loss_marks_capture_failed_before_app_thread_teardown() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        install_imported(&mut app, pad(0), "old-output.wav");
        let old_sample = Arc::clone(app.pad(pad(0)).sample.as_ref().unwrap());
        let old_revision = app.project_revision();
        start_capture(&mut app, CaptureSource::Resample, 4);
        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.2, -0.2],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let request = take_finalize(&mut app);

        probe.fail_output_session("output disconnected");
        assert!(app.maintain_audio());

        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
        assert!(app.audio.is_none());
        assert!(Arc::ptr_eq(
            app.pad(pad(0)).sample.as_ref().unwrap(),
            &old_sample
        ));
        assert_eq!(app.project_revision(), old_revision);
        assert!(Arc::ptr_eq(
            app.capture_source_pcm.as_ref().unwrap(),
            &request.stereo
        ));

        let (replacement, _) = CaptureAudio::new(48_000, 44_100);
        app.retry_default_device_with(|| Ok(Box::new(replacement)));
        assert_eq!(
            app.capture_session().failure_cause(),
            Some(CaptureFailureCause::DeviceRuntime)
        );
        assert_eq!(
            app.retry_capture_finalization(),
            Err(CaptureError::RetryNotAllowed(
                CaptureFailureCause::DeviceRuntime
            ))
        );
        assert!(Arc::ptr_eq(
            app.capture_source_pcm.as_ref().unwrap(),
            &request.stereo
        ));
    }

    #[test]
    fn capture_transaction_resample_rate_mismatch_fails_without_silent_resampling() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        start_capture(&mut app, CaptureSource::Resample, 4);
        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.2, -0.2],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let request = take_finalize(&mut app);
        probe.set_output_rate(44_100);
        assert!(app.apply_worker_result(finalized(
            &request,
            Ok(managed_capture(10, 48_000, b"resample-old-rate")),
        )));
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
        assert_eq!(app.capture_session().generation(), Some(request.generation));
        assert!(app.pad(pad(0)).sample.is_none());
        assert_eq!(app.project_revision(), 0);
        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture {
                id: ManagedCaptureId::new(10),
            }]
        );
    }

    #[test]
    fn capture_snapshot_refuses_every_unresolved_phase() {
        for phase in [
            CapturePhase::Confirm,
            CapturePhase::Arming,
            CapturePhase::Recording,
            CapturePhase::Finalizing,
            CapturePhase::ReadyToInstall,
            CapturePhase::Failed,
        ] {
            let mut app = App::without_audio("offline");
            app.capture_session_mut()
                .begin(CaptureSource::Resample, pad(0), 48_000, 4)
                .unwrap();
            if phase != CapturePhase::Confirm {
                app.capture_session_mut().mark_arming().unwrap();
            }
            if !matches!(phase, CapturePhase::Confirm | CapturePhase::Arming) {
                app.capture_session_mut().mark_recording().unwrap();
            }
            if matches!(
                phase,
                CapturePhase::Finalizing | CapturePhase::ReadyToInstall
            ) {
                let candidate = completion(&app, CaptureSource::Resample, vec![0.2, -0.2], false);
                app.capture_session_mut()
                    .accept_completion(candidate)
                    .unwrap();
            }
            if phase == CapturePhase::ReadyToInstall {
                app.capture_session_mut().mark_ready_to_install().unwrap();
            }
            if phase == CapturePhase::Failed {
                app.capture_session_mut().mark_failed("failed").unwrap();
            }
            assert_eq!(
                app.project_snapshot(),
                Err(ProjectSnapshotError::UnresolvedCapture(phase)),
                "phase {phase:?}"
            );
        }
    }

    fn commit_capture(app: &mut App, probe: &CaptureProbe, id: u64) {
        start_capture(app, CaptureSource::Resample, 4);
        drive_ready(app, probe, CaptureSource::Resample, id, false);
        assert!(app.maintain_capture());
    }

    #[test]
    fn capture_snapshot_commits_managed_wav_immediately_and_replacement_release_survives_backpressure_and_mismatch()
     {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0);
        commit_capture(&mut app, &probe, 21);
        let snapshot = app.project_snapshot().unwrap();
        assert_eq!(
            snapshot.pads[0].source_path,
            PathBuf::from("managed-21.wav")
        );
        assert_eq!(snapshot.pads[0].fingerprint, fingerprint(b"captured"));
        assert_eq!(snapshot.pads[0].recipe, SampleEditRecipe::identity());
        let capture_generation = snapshot.pads[0].source_generation;
        assert_eq!(capture_generation, app.pad(target).generation);

        app.project_session = crate::ProjectSession::new(
            ProjectId::from_bytes([0x21; 16]),
            Some(PathBuf::from("named-project")),
            "Named",
            app.project_revision(),
        );
        let changed_settings = PadSettings {
            gain_db: -1.0,
            ..PadSettings::default()
        };
        app.update_pad_settings(target, changed_settings).unwrap();
        assert!(app.maintain_project(Instant::now() + Duration::from_secs(3)));
        let autosave_requests = app.take_worker_requests();
        let [WorkerRequest::SaveProject(autosave)] = autosave_requests.as_slice() else {
            panic!("expected autosave snapshot")
        };
        assert_eq!(autosave.request.kind, SaveKind::Recovery);
        assert_eq!(
            autosave.request.snapshot.pads[0].source_path,
            PathBuf::from("managed-21.wav")
        );
        assert_eq!(
            autosave.request.snapshot.pads[0].fingerprint,
            fingerprint(b"captured")
        );
        assert_eq!(
            autosave.request.snapshot.pads[0].recipe,
            SampleEditRecipe::identity()
        );
        assert_eq!(autosave.request.snapshot.pads[0].settings, changed_settings);
        assert_eq!(
            autosave.request.snapshot.pads[0].source_generation,
            capture_generation
        );
        assert!(app.apply_worker_result(save_result(autosave, Vec::new())));

        install_imported(&mut app, target, "replacement.wav");
        assert!(app.maintain_capture());
        let requests = app.take_worker_requests();
        assert_eq!(
            requests,
            [WorkerRequest::ReleaseManagedCapture {
                id: ManagedCaptureId::new(21),
            }]
        );
        assert!(app.apply_worker_send_error(
            requests.into_iter().next().unwrap(),
            WorkerSendError::WorkerBusy,
        ));
        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture {
                id: ManagedCaptureId::new(21),
            }]
        );
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: ManagedCaptureId::new(99),
                result: Ok(()),
            })
        );
        assert!(!app.maintain_capture());
        assert_eq!(
            app.managed_release_in_flight(),
            Some(ManagedCaptureId::new(21))
        );
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: ManagedCaptureId::new(21),
                result: Err(crate::capture_store::CaptureStoreError::NotLive {
                    id: ManagedCaptureId::new(21),
                }),
            })
        );
        assert!(app.maintain_capture());
        assert_eq!(app.managed_release_in_flight(), None);
        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture {
                id: ManagedCaptureId::new(21),
            }]
        );
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: ManagedCaptureId::new(21),
                result: Ok(()),
            })
        );
        assert!(app.maintain_capture());
        assert_eq!(app.managed_release_in_flight(), None);
    }

    fn save_result(
        request: &ProjectSaveWorkerRequest,
        mappings: Vec<ProjectAssetMapping>,
    ) -> WorkerResult {
        WorkerResult::ProjectSaved {
            token: request.token,
            kind: request.request.kind,
            project_id: request.request.snapshot.project_id,
            directory: request.request.directory.clone(),
            revision: request.request.snapshot.revision,
            result: Ok(SaveReceipt {
                directory: request.request.directory.clone(),
                kind: request.request.kind,
                project_id: request.request.snapshot.project_id,
                revision: request.request.snapshot.revision,
                canonical_toml: "saved".to_owned(),
                mappings,
            }),
        }
    }

    #[test]
    fn capture_snapshot_remove_and_explicit_mapping_each_release_the_exact_prior_managed_id() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0);
        commit_capture(&mut app, &probe, 31);
        let before_remove = app.project_revision();
        app.remove_pad_sample(target).unwrap();
        assert!(app.pad(target).sample.is_none());
        assert_eq!(app.project_revision(), before_remove + 1);
        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture {
                id: ManagedCaptureId::new(31),
            }]
        );
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: ManagedCaptureId::new(31),
                result: Ok(()),
            })
        );
        assert!(app.maintain_capture());

        commit_capture(&mut app, &probe, 32);
        let capture_snapshot = app.project_snapshot().unwrap();
        app.request_save_as("saved-project").unwrap();
        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::SaveProject(request)] = requests.as_slice() else {
            panic!("expected explicit save")
        };
        assert_eq!(request.request.kind, SaveKind::Explicit);
        assert_eq!(request.request.snapshot.revision, capture_snapshot.revision);
        assert_eq!(request.request.snapshot.pads, capture_snapshot.pads);
        let saved_pad = request.request.snapshot.pads[0].clone();
        assert!(app.apply_worker_result(save_result(
            request,
            vec![ProjectAssetMapping {
                pad: target,
                source_generation: saved_pad.source_generation,
                fingerprint: saved_pad.fingerprint,
                project_path: "saved-project/audio/captured.wav".into(),
            }],
        )));
        assert_eq!(
            app.pad(target).source.as_deref(),
            Some(Path::new("saved-project/audio/captured.wav"))
        );
        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture {
                id: ManagedCaptureId::new(32),
            }]
        );
    }

    #[test]
    fn capture_recovery_at_new_rate_preserves_managed_source_until_explicit_mapping_release() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0);
        start_capture(&mut app, CaptureSource::Resample, 4);
        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.25, -0.25],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let finalize = take_finalize(&mut app);
        let mut store = CaptureStore::new().unwrap();
        let capture = store
            .finalize(Arc::from([0.7_f32, -0.7, 0.4, -0.4]), 48_000)
            .unwrap();
        let managed_id = capture.id;
        let managed_path = capture.path.clone();
        assert!(app.apply_worker_result(finalized(&finalize, Ok(capture))));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), None);
        assert!(managed_path.is_file());

        probe.fail_output_session("output lost");
        assert!(app.maintain_audio());
        let (recovered_audio, _) = CaptureAudio::new(44_100, 44_100);
        assert!(app.retry_with(Box::new(recovered_audio)));
        let recovery_requests = app.take_worker_requests();
        let [
            WorkerRequest::LoadSample {
                pad,
                generation,
                purpose: LoadPurpose::Recovery,
                path,
                engine_rate: 44_100,
                recipe,
            },
        ] = recovery_requests.as_slice()
        else {
            panic!("expected one different-rate recovery request")
        };
        assert_eq!(*pad, target);
        assert_eq!(*recipe, SampleEditRecipe::identity());
        let recovery_path = path.clone();
        let recovery_generation = *generation;
        assert!(app.apply_worker_result(loaded_result_with_purpose(
            target,
            recovery_generation,
            LoadPurpose::Recovery,
            recovery_path.to_str().unwrap(),
            44_100,
        )));

        assert_eq!(
            app.pad(target).source.as_deref(),
            Some(managed_path.as_path())
        );
        assert_eq!(
            app.pad(target).sample.as_ref().unwrap().sample_rate(),
            44_100
        );
        assert_eq!(
            app.sample_editor.commits[super::pad_offset(target)].managed_capture,
            Some(managed_id)
        );
        assert!(app.pending_managed_releases.is_empty());
        assert!(managed_path.is_file());

        let snapshot = app.project_snapshot().unwrap();
        app.request_save_as("saved-recovered-project").unwrap();
        assert!(app.maintain_project(Instant::now()));
        let save_requests = app.take_worker_requests();
        let [WorkerRequest::SaveProject(save)] = save_requests.as_slice() else {
            panic!("expected explicit save after recovery")
        };
        assert_eq!(save.request.snapshot.revision, snapshot.revision);
        assert_eq!(save.request.snapshot.pads, snapshot.pads);
        let saved_pad = save.request.snapshot.pads[0].clone();
        assert!(app.apply_worker_result(save_result(
            save,
            vec![ProjectAssetMapping {
                pad: target,
                source_generation: saved_pad.source_generation,
                fingerprint: saved_pad.fingerprint,
                project_path: "saved-recovered-project/audio/captured.wav".into(),
            }],
        )));

        assert_eq!(
            app.pad(target).source.as_deref(),
            Some(Path::new("saved-recovered-project/audio/captured.wav"))
        );
        assert_eq!(
            app.sample_editor.commits[super::pad_offset(target)].managed_capture,
            None
        );
        assert!(managed_path.is_file());
        assert_eq!(
            app.pending_managed_releases
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            [managed_id]
        );

        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture { id: managed_id }]
        );
        store.release(managed_id).unwrap();
        assert!(!managed_path.exists());
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: managed_id,
                result: Ok(()),
            })
        );
        assert!(app.maintain_capture());
        assert!(!app.maintain_capture());
        assert!(app.take_worker_requests().is_empty());
        assert!(matches!(
            store.release(managed_id),
            Err(CaptureStoreError::NotLive { id }) if id == managed_id
        ));
    }

    #[derive(Debug, Clone, Copy)]
    enum CaptureTargetMutation {
        LoadAdmission,
        Settings,
        EditAdmission,
        EditWorkerCompletion,
    }

    fn finish_stale_capture_candidate(
        app: &mut App,
        probe: &CaptureProbe,
        target: PadId,
        expected_sample: &Arc<SampleBuffer>,
        managed_id: ManagedCaptureId,
    ) {
        let installs_before = probe
            .calls()
            .iter()
            .filter(|call| matches!(call, CaptureCall::Install(..)))
            .count();
        probe.complete(completion(
            app,
            CaptureSource::Resample,
            vec![0.25, -0.25],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let finalize = take_finalize(app);
        assert!(app.apply_worker_result(finalized(
            &finalize,
            Ok(managed_capture(
                managed_id.get(),
                finalize.engine_rate,
                b"stale target candidate",
            )),
        )));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());

        assert_eq!(
            app.capture_session().phase(),
            Some(CapturePhase::Failed),
            "a stale capture must remain explicitly discardable"
        );
        assert_eq!(
            app.capture_session().failure_cause(),
            Some(CaptureFailureCause::InvalidCapture)
        );
        assert!(Arc::ptr_eq(
            app.pad(target).sample.as_ref().unwrap(),
            expected_sample
        ));
        assert_eq!(
            probe
                .calls()
                .iter()
                .filter(|call| matches!(call, CaptureCall::Install(..)))
                .count(),
            installs_before,
            "stale capture reached audio admission"
        );
        assert_eq!(app.capture_session().managed_capture_id(), Some(managed_id));

        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture { id: managed_id }]
        );
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: managed_id,
                result: Ok(()),
            })
        );
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().managed_capture_id(), None);
        assert!(!app.maintain_capture());
        assert!(app.take_worker_requests().is_empty());
    }

    #[test]
    fn capture_target_fence_rejects_load_settings_edit_and_worker_completion_races() {
        for (index, mutation) in [
            CaptureTargetMutation::LoadAdmission,
            CaptureTargetMutation::Settings,
            CaptureTargetMutation::EditAdmission,
            CaptureTargetMutation::EditWorkerCompletion,
        ]
        .into_iter()
        .enumerate()
        {
            let (audio, probe) = CaptureAudio::new(48_000, 44_100);
            let mut app = App::with_audio(Box::new(audio));
            let target = pad(0);
            install_imported(&mut app, target, "original.wav");
            start_capture(&mut app, CaptureSource::Resample, 4);

            let expected_sample = match mutation {
                CaptureTargetMutation::LoadAdmission => {
                    let request = app.begin_load(target, "newer-load.wav").unwrap();
                    assert!(matches!(request, WorkerRequest::LoadSample { .. }));
                    Arc::clone(app.pad(target).sample.as_ref().unwrap())
                }
                CaptureTargetMutation::Settings => {
                    let settings = PadSettings {
                        gain_db: -6.0,
                        ..app.pad(target).settings
                    };
                    app.update_pad_settings(target, settings).unwrap();
                    assert_eq!(app.pad(target).settings, settings);
                    Arc::clone(app.pad(target).sample.as_ref().unwrap())
                }
                CaptureTargetMutation::EditAdmission
                | CaptureTargetMutation::EditWorkerCompletion => {
                    let recipe = SampleEditRecipe {
                        reversed: true,
                        ..SampleEditRecipe::identity()
                    };
                    app.request_sample_edit(target, recipe).unwrap();
                    let requests = app.take_worker_requests();
                    let [WorkerRequest::EditSample { generation, .. }] = requests.as_slice() else {
                        panic!("expected edit worker request for {mutation:?}")
                    };
                    if matches!(mutation, CaptureTargetMutation::EditWorkerCompletion) {
                        let offset = super::pad_offset(target);
                        let base_preview = Arc::clone(
                            &app.sample_editor.pending[offset]
                                .as_ref()
                                .unwrap()
                                .base_preview,
                        );
                        assert!(app.apply_worker_result(WorkerResult::Edited {
                            pad: target,
                            generation: *generation,
                            recipe,
                            result: Ok(RenderedSample {
                                base_preview,
                                rendered: Arc::new(
                                    SampleBuffer::new(48_000, vec![-0.4, 0.4]).unwrap(),
                                ),
                                rendered_preview: Arc::new(
                                    [PreviewColumn { min: -5, max: 5 }; EDIT_PREVIEW_COLUMNS],
                                ),
                            }),
                        }));
                        assert!(app.maintain_audio());
                        assert_eq!(app.committed_sample_recipe(target), Some(recipe));
                    }
                    Arc::clone(app.pad(target).sample.as_ref().unwrap())
                }
            };

            finish_stale_capture_candidate(
                &mut app,
                &probe,
                target,
                &expected_sample,
                ManagedCaptureId::new(400 + index as u64),
            );
        }
    }

    #[test]
    fn capture_target_fence_rejects_removal_before_audio_admission() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0);
        install_imported(&mut app, target, "original.wav");
        app.request_capture_with_limit_for_test(CaptureSource::Resample, 4)
            .unwrap();
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Confirm));
        app.remove_pad_sample(target).unwrap();
        let calls_before = probe.calls();

        assert!(app.confirm_capture().is_err());
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
        assert!(app.pad(target).sample.is_none());
        assert_eq!(probe.calls(), calls_before);
    }

    #[test]
    fn capture_target_fence_rechecks_ready_to_install_before_audio_admission() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0);
        install_imported(&mut app, target, "original.wav");
        let original = Arc::clone(app.pad(target).sample.as_ref().unwrap());
        start_capture(&mut app, CaptureSource::Resample, 4);
        drive_ready(&mut app, &probe, CaptureSource::Resample, 450, false);
        let settings = PadSettings {
            gain_db: -9.0,
            ..app.pad(target).settings
        };
        app.update_pad_settings(target, settings).unwrap();

        finish_ready_stale_capture(
            &mut app,
            &probe,
            target,
            &original,
            ManagedCaptureId::new(450),
        );
        assert_eq!(app.pad(target).settings, settings);
    }

    #[test]
    fn stale_ready_target_discard_waits_for_exact_release_retry_before_quit_or_open_advances() {
        for action in [ProjectAction::Quit, ProjectAction::Open] {
            let (audio, probe) = CaptureAudio::new(48_000, 44_100);
            let mut app = App::with_audio(Box::new(audio));
            let target = pad(0);
            install_imported(&mut app, target, "original.wav");
            start_capture(&mut app, CaptureSource::Resample, 4);
            probe.complete(completion(
                &app,
                CaptureSource::Resample,
                vec![0.25, -0.25],
                false,
            ));
            assert!(app.maintain_capture());
            assert!(app.maintain_capture());
            let finalize = take_finalize(&mut app);
            let mut store = CaptureStore::new().unwrap();
            let candidate = store.finalize(Arc::from([0.2_f32, -0.2]), 48_000).unwrap();
            let managed_id = candidate.id;
            let managed_path = candidate.path.clone();
            assert!(app.apply_worker_result(finalized(&finalize, Ok(candidate))));
            assert!(app.maintain_capture());
            assert_eq!(
                app.capture_session().phase(),
                Some(CapturePhase::ReadyToInstall)
            );

            app.update_pad_settings(
                target,
                PadSettings {
                    gain_db: -9.0,
                    ..app.pad(target).settings
                },
            )
            .unwrap();
            assert!(app.maintain_capture());
            assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
            assert_eq!(
                app.capture_session().managed_capture_id(),
                Some(managed_id),
                "the rejected artifact remains the capture lifecycle's exact release fence"
            );

            match action {
                ProjectAction::Quit => app.apply(InputAction::Quit),
                ProjectAction::Open => {
                    app.request_open_project_interactive("after-stale-ready-discard")
                }
            }
            app.apply_key(press(KeyCode::Backspace));
            assert_eq!(app.capture_session().phase(), None);
            assert_eq!(app.capture_discard_release_pending, Some(managed_id));
            assert_matrix_action_waiting(&app, action);

            assert!(app.maintain_capture());
            assert_eq!(
                app.take_worker_requests(),
                [WorkerRequest::ReleaseManagedCapture { id: managed_id }]
            );
            let sync_error = store
                .release_with_sync_for_test(managed_id, |_, _| {
                    Err(CaptureStoreError::Filesystem {
                        operation: "injected stale-target post-unlink directory sync failure",
                        path: PathBuf::from("injected"),
                        kind: std::io::ErrorKind::Other,
                    })
                })
                .unwrap_err();
            assert!(!managed_path.exists());
            assert!(
                app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                    id: managed_id,
                    result: Err(sync_error),
                })
            );
            assert!(app.maintain_capture());
            assert_eq!(app.capture_discard_release_pending, Some(managed_id));
            assert_matrix_action_waiting(&app, action);

            assert!(app.maintain_capture());
            assert_eq!(
                app.take_worker_requests(),
                [WorkerRequest::ReleaseManagedCapture { id: managed_id }]
            );
            store.release(managed_id).unwrap();
            assert!(
                app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                    id: managed_id,
                    result: Ok(()),
                })
            );
            assert!(app.maintain_capture());
            assert_eq!(app.capture_discard_release_pending, None);
            assert!(matches!(
                app.overlay(),
                Some(Overlay::UnsavedProject { action: shown }) if *shown == action
            ));
            assert_matrix_action_waiting(&app, action);
            assert!(matches!(
                store.release(managed_id),
                Err(CaptureStoreError::NotLive { id }) if id == managed_id
            ));
        }
    }

    fn finish_ready_stale_capture(
        app: &mut App,
        probe: &CaptureProbe,
        target: PadId,
        expected_sample: &Arc<SampleBuffer>,
        managed_id: ManagedCaptureId,
    ) {
        let installs_before = probe
            .calls()
            .iter()
            .filter(|call| matches!(call, CaptureCall::Install(..)))
            .count();
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
        assert!(Arc::ptr_eq(
            app.pad(target).sample.as_ref().unwrap(),
            expected_sample
        ));
        assert_eq!(
            probe
                .calls()
                .iter()
                .filter(|call| matches!(call, CaptureCall::Install(..)))
                .count(),
            installs_before
        );
        assert_eq!(app.capture_session().managed_capture_id(), Some(managed_id));
        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture { id: managed_id }]
        );
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: managed_id,
                result: Ok(()),
            })
        );
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().managed_capture_id(), None);
        assert!(!app.maintain_capture());
    }

    #[test]
    fn capture_transaction_public_requests_use_the_hard_limit_constant() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        app.request_resample().unwrap();
        assert!(probe.calls().iter().any(|call| matches!(
            call,
            CaptureCall::Begin {
                max_frames: MAX_CAPTURE_FRAMES,
                ..
            }
        )));
    }

    #[test]
    fn capture_request_rejects_oversized_frame_limit_without_mutating_empty_target() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0);
        let before = capture_admission_snapshot(&app, &probe, target);

        assert_eq!(
            app.request_capture_with_limit_for_test(
                CaptureSource::Resample,
                MAX_CAPTURE_FRAMES + 1,
            ),
            Err(oversized_frame_limit_error())
        );
        assert_eq!(capture_admission_snapshot(&app, &probe, target), before);
    }

    #[test]
    fn capture_request_rejects_oversized_frame_limit_without_mutating_occupied_target() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0);
        install_imported(&mut app, target, "occupied-limit.wav");
        let before = capture_admission_snapshot(&app, &probe, target);

        assert_eq!(
            app.request_capture_with_limit_for_test(CaptureSource::Input, MAX_CAPTURE_FRAMES + 1,),
            Err(oversized_frame_limit_error())
        );
        assert_eq!(capture_admission_snapshot(&app, &probe, target), before);
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Press)
    }

    fn release(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Release)
    }

    #[test]
    fn capture_lifecycle_confirmation_recording_controls_and_performance_routing_are_explicit() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        install_imported(&mut app, pad(0), "occupied-before-capture.wav");
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });

        app.request_capture_with_limit_for_test(CaptureSource::Resample, 96_000)
            .unwrap();
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Confirm));
        assert!(
            app.overlay().is_some(),
            "replacement confirmation must be modal"
        );

        app.apply_key(press(KeyCode::Enter));
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Recording));
        assert!(
            app.overlay().is_none(),
            "recording is a nonblocking presentation"
        );

        app.apply_key(press(KeyCode::Char('1')));
        assert!(app.is_pad_held(0));
        assert!(probe.calls().contains(&CaptureCall::Trigger(pad(0))));
        app.apply_key(release(KeyCode::Char('1')));
        assert!(!app.is_pad_held(0));
        assert!(probe.calls().contains(&CaptureCall::Release(pad(0))));

        app.apply_key(press(KeyCode::Tab));
        assert_eq!(app.workspace_view(), crate::WorkspaceView::Pattern);
        app.apply_key(press(KeyCode::Right));
        assert_eq!(app.patterns().cursor().step(), 1);
        app.apply_key(press(KeyCode::Tab));
        app.apply_key(press(KeyCode::Tab));
        app.apply_key(press(KeyCode::Tab));
        assert_eq!(app.workspace_view(), crate::WorkspaceView::Perform);

        app.apply_key(press(KeyCode::Enter));
        assert!(matches!(
            probe.calls().last(),
            Some(CaptureCall::Stop(_, _))
        ));
        assert!(app.overlay().is_some(), "stopping/finalizing must be modal");
    }

    #[test]
    fn capture_lifecycle_escape_confirms_discard_explicit_cancel_waits_for_ownership_and_palette_keys_remain_text()
     {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        start_capture(&mut app, CaptureSource::Input, 16);

        app.apply_key(press(KeyCode::Esc));
        assert!(app.overlay().is_some(), "Escape must ask before discarding");
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Recording));
        app.apply_key(press(KeyCode::Esc));
        assert!(app.overlay().is_none(), "second Escape keeps the take");
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Recording));

        app.open_palette();
        app.apply_terminal_event(Event::Paste("capture-cancel".to_owned()));
        app.apply_key(press(KeyCode::Enter));
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Recording));
        assert!(matches!(
            probe.calls().last(),
            Some(CaptureCall::Cancel(_, _))
        ));
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), None);

        let (audio, _) = CaptureAudio::new(48_000, 44_100);
        let mut command = App::with_audio(Box::new(audio));
        command.open_palette();
        command.apply_terminal_event(Event::Paste("resample".to_owned()));
        command.apply_key(press(KeyCode::Char('1')));
        assert_eq!(command.palette_text(), "resample1");
        assert_eq!(command.capture_session().phase(), None);
    }

    fn app_at_capture_phase(phase: CapturePhase) -> App {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        if phase == CapturePhase::Confirm {
            install_imported(&mut app, pad(0), "occupied.wav");
            app.request_capture_with_limit_for_test(CaptureSource::Resample, 8)
                .unwrap();
            return app;
        }
        start_capture(&mut app, CaptureSource::Resample, 8);
        if phase == CapturePhase::Recording {
            return app;
        }
        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.25, -0.25],
            false,
        ));
        assert!(app.maintain_capture());
        if phase == CapturePhase::Finalizing {
            return app;
        }
        assert!(app.maintain_capture());
        let request = take_finalize(&mut app);
        if phase == CapturePhase::Failed {
            assert!(app.apply_worker_result(finalized(
                &request,
                Err(CaptureFinalizeError::Prepare("encode failed".to_owned())),
            )));
            assert!(app.maintain_capture());
            return app;
        }
        assert!(app.apply_worker_result(finalized(
            &request,
            Ok(managed_capture(90, 48_000, b"ready")),
        )));
        assert!(app.maintain_capture());
        assert_eq!(phase, CapturePhase::ReadyToInstall);
        app
    }

    struct CaptureLifecycleMatrixFixture {
        app: App,
        probe: CaptureProbe,
    }

    #[derive(Debug, Clone, Copy)]
    enum CaptureLifecycleChoice {
        Finalize,
        Discard,
        Cancel,
    }

    impl CaptureLifecycleChoice {
        const fn key(self) -> KeyCode {
            match self {
                Self::Finalize => KeyCode::Enter,
                Self::Discard => KeyCode::Backspace,
                Self::Cancel => KeyCode::Esc,
            }
        }
    }

    fn capture_lifecycle_matrix_fixture(phase: CapturePhase) -> CaptureLifecycleMatrixFixture {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        install_imported(&mut app, pad(0), "matrix-occupied.wav");
        app.project_session
            .mark_explicit_saved(app.project_revision());
        app.request_capture_with_limit_for_test(CaptureSource::Resample, 8)
            .unwrap();
        if phase == CapturePhase::Confirm {
            return CaptureLifecycleMatrixFixture { app, probe };
        }
        app.confirm_capture().unwrap();
        if phase == CapturePhase::Recording {
            return CaptureLifecycleMatrixFixture { app, probe };
        }
        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.25, -0.25],
            false,
        ));
        assert!(app.maintain_capture());
        if phase == CapturePhase::Finalizing {
            return CaptureLifecycleMatrixFixture { app, probe };
        }
        assert!(app.maintain_capture());
        let request = take_finalize(&mut app);
        let result = if phase == CapturePhase::Failed {
            Err(CaptureFinalizeError::Prepare(
                "matrix worker failure".to_owned(),
            ))
        } else {
            Ok(managed_capture(301, 48_000, b"matrix-ready"))
        };
        assert!(app.apply_worker_result(finalized(&request, result)));
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), Some(phase));
        CaptureLifecycleMatrixFixture { app, probe }
    }

    fn begin_matrix_project_action(app: &mut App, action: ProjectAction) {
        match action {
            ProjectAction::Quit => app.apply(InputAction::Quit),
            ProjectAction::Open => app.request_open_project_interactive("matrix-open"),
        }
        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::ResolveCapture { action: shown }) if *shown == action
        ));
    }

    fn insert_matrix_async_boundary(
        fixture: &mut CaptureLifecycleMatrixFixture,
        phase: CapturePhase,
    ) {
        match phase {
            CapturePhase::Confirm | CapturePhase::Failed => {
                let _ = fixture.app.maintain_capture();
            }
            CapturePhase::Recording => {
                fixture.probe.complete(completion(
                    &fixture.app,
                    CaptureSource::Resample,
                    vec![0.5, -0.5],
                    false,
                ));
                assert!(fixture.app.maintain_capture());
            }
            CapturePhase::Finalizing => {
                assert!(fixture.app.maintain_capture());
                let request = take_finalize(&mut fixture.app);
                assert!(fixture.app.apply_worker_result(finalized(
                    &request,
                    Ok(managed_capture(302, 48_000, b"matrix-async")),
                )));
                assert!(fixture.app.maintain_capture());
            }
            CapturePhase::ReadyToInstall => {
                assert!(
                    !fixture.app.maintain_capture(),
                    "Ready capture must not install before the explicit lifecycle choice"
                );
            }
            CapturePhase::Arming => unreachable!("matrix excludes transient Arming"),
        }
    }

    fn assert_matrix_action_waiting(app: &App, action: ProjectAction) {
        assert!(!app.should_quit());
        assert!(app.project_open_stage().is_none());
        assert_eq!(
            app.pending_project_action
                .as_ref()
                .map(PendingProjectAction::label),
            Some(action),
        );
    }

    fn assert_matrix_action_continued(app: &App, action: ProjectAction) {
        assert_eq!(app.capture_session().phase(), None);
        assert_eq!(app.pending_project_action, None);
        match action {
            ProjectAction::Quit => assert!(app.should_quit()),
            ProjectAction::Open => assert!(app.project_open_stage().is_some()),
        }
    }

    fn finish_matrix_finalize(
        fixture: &mut CaptureLifecycleMatrixFixture,
        action: ProjectAction,
        managed_id: u64,
    ) {
        if fixture.app.capture_session().phase() == Some(CapturePhase::Recording) {
            fixture.probe.complete(completion(
                &fixture.app,
                CaptureSource::Resample,
                vec![0.75, -0.75],
                false,
            ));
            assert!(fixture.app.maintain_capture());
        }
        if fixture.app.capture_session().phase() == Some(CapturePhase::Finalizing) {
            assert!(fixture.app.maintain_capture());
            let request = take_finalize(&mut fixture.app);
            assert!(fixture.app.apply_worker_result(finalized(
                &request,
                Ok(managed_capture(managed_id, 48_000, b"matrix-finalize")),
            )));
            assert!(fixture.app.maintain_capture());
        }
        assert_eq!(
            fixture.app.capture_session().phase(),
            Some(CapturePhase::ReadyToInstall)
        );
        assert!(fixture.app.maintain_capture());
        assert_eq!(fixture.app.capture_session().phase(), None);
        assert!(matches!(
            fixture.app.overlay(),
            Some(super::Overlay::UnsavedProject { action: shown }) if *shown == action
        ));
        assert_matrix_action_waiting(&fixture.app, action);
    }

    #[test]
    fn capture_resolution_survives_async_completion_worker_result_and_ready_maintenance() {
        let mut fixture = capture_lifecycle_matrix_fixture(CapturePhase::Recording);
        begin_matrix_project_action(&mut fixture.app, ProjectAction::Open);
        insert_matrix_async_boundary(&mut fixture, CapturePhase::Recording);
        assert!(matches!(
            fixture.app.overlay(),
            Some(super::Overlay::ResolveCapture {
                action: ProjectAction::Open
            })
        ));

        assert!(fixture.app.maintain_capture());
        let request = take_finalize(&mut fixture.app);
        assert!(fixture.app.apply_worker_result(finalized(
            &request,
            Ok(managed_capture(303, 48_000, b"resolution-survives")),
        )));
        assert!(fixture.app.maintain_capture());
        assert_eq!(
            fixture.app.capture_session().phase(),
            Some(CapturePhase::ReadyToInstall)
        );
        assert!(matches!(
            fixture.app.overlay(),
            Some(super::Overlay::ResolveCapture {
                action: ProjectAction::Open
            })
        ));
        assert!(!fixture.app.maintain_capture());
        assert_eq!(
            fixture.app.capture_session().phase(),
            Some(CapturePhase::ReadyToInstall)
        );
    }

    #[test]
    fn capture_worker_ownership_survives_device_loss_until_exact_discard_result_returns() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        start_capture(&mut app, CaptureSource::Resample, 8);
        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.25, -0.25],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let request = take_finalize(&mut app);
        app.request_open_project_interactive("after-device-loss");
        app.apply_key(press(KeyCode::Enter));

        probe.fail_device(
            CaptureSource::Resample,
            "output disappeared during worker finalize",
        );
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
        app.apply_key(press(KeyCode::Backspace));
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));

        assert!(app.apply_worker_result(finalized(
            &request,
            Ok(managed_capture(304, 48_000, b"device-loss-discard")),
        )));
        assert!(app.maintain_capture());
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert!(
            app.pending_managed_releases
                .contains(&ManagedCaptureId::new(304))
        );
        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture {
                id: ManagedCaptureId::new(304),
            }]
        );
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: ManagedCaptureId::new(304),
                result: Ok(()),
            })
        );
        assert!(app.maintain_capture());
        assert_matrix_action_continued(&app, ProjectAction::Open);
    }

    #[test]
    fn recovered_device_does_not_admit_worker_result_after_capture_was_marked_failed() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        start_capture(&mut app, CaptureSource::Resample, 8);
        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.25, -0.25],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let request = take_finalize(&mut app);
        app.apply(InputAction::Quit);
        app.apply_key(press(KeyCode::Enter));

        probe.fail_device(
            CaptureSource::Resample,
            "output disappeared before worker return",
        );
        assert!(app.maintain_capture());
        let (replacement, _) = CaptureAudio::new(48_000, 44_100);
        app.retry_default_device_with(|| Ok(Box::new(replacement)));
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));

        assert!(app.apply_worker_result(finalized(
            &request,
            Ok(managed_capture(305, 48_000, b"failed-device-result")),
        )));
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Failed));
        assert_matrix_action_waiting(&app, ProjectAction::Quit);
        assert!(
            app.pending_managed_releases
                .contains(&ManagedCaptureId::new(305))
        );

        app.apply_key(press(KeyCode::Backspace));
        assert_matrix_action_continued(&app, ProjectAction::Quit);
    }

    #[test]
    fn capture_discard_waits_for_exact_managed_release_success_before_open_continues() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        start_capture(&mut app, CaptureSource::Resample, 8);
        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.25, -0.25],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let finalize = take_finalize(&mut app);
        app.request_open_project_interactive("after-exact-discard-release");
        app.apply_key(press(KeyCode::Backspace));

        for index in 0..WORKER_CHANNEL_CAPACITY {
            app.pending_worker_requests
                .push(WorkerRequest::ReleaseManagedCapture {
                    id: ManagedCaptureId::new(700 + index as u64),
                });
        }
        let discard_id = ManagedCaptureId::new(601);
        assert!(app.apply_worker_result(finalized(
            &finalize,
            Ok(managed_capture(601, 48_000, b"discard-release-fence")),
        )));
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), None);
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert!(!app.maintain_capture());
        assert_matrix_action_waiting(&app, ProjectAction::Open);

        app.pending_worker_requests.clear();
        assert!(app.maintain_capture());
        let [release] = app.take_worker_requests().try_into().unwrap();
        assert_eq!(
            release,
            WorkerRequest::ReleaseManagedCapture { id: discard_id }
        );

        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: ManagedCaptureId::new(999),
                result: Ok(()),
            })
        );
        assert!(!app.maintain_capture());
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert_eq!(app.managed_release_in_flight(), Some(discard_id));

        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: discard_id,
                result: Err(crate::capture_store::CaptureStoreError::NotLive { id: discard_id }),
            })
        );
        assert!(app.maintain_capture());
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert_eq!(app.managed_release_in_flight(), None);

        assert!(app.maintain_capture());
        let [release] = app.take_worker_requests().try_into().unwrap();
        assert_eq!(
            release,
            WorkerRequest::ReleaseManagedCapture { id: discard_id }
        );
        assert!(app.apply_worker_send_error(release, WorkerSendError::WorkerBusy));
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert_eq!(app.managed_release_in_flight(), None);

        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture { id: discard_id }]
        );
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: discard_id,
                result: Ok(()),
            })
        );
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert_eq!(app.managed_release_in_flight(), Some(discard_id));
        assert!(app.maintain_capture());
        assert_matrix_action_continued(&app, ProjectAction::Open);
    }

    fn app_waiting_for_exact_discard_release(id: u64) -> App {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        start_capture(&mut app, CaptureSource::Resample, 8);
        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.25, -0.25],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let finalize = take_finalize(&mut app);
        app.cancel_capture().unwrap();
        assert!(app.apply_worker_result(finalized(
            &finalize,
            Ok(managed_capture(id, 48_000, b"late-release-fence")),
        )));
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), None);
        app
    }

    #[test]
    fn late_quit_and_open_wait_for_the_existing_exact_discard_release() {
        for (index, action) in [ProjectAction::Quit, ProjectAction::Open]
            .into_iter()
            .enumerate()
        {
            let discard_id = ManagedCaptureId::new(602 + index as u64);
            let mut app = app_waiting_for_exact_discard_release(discard_id.get());

            match action {
                ProjectAction::Quit => app.apply(InputAction::Quit),
                ProjectAction::Open => app.request_open_project_interactive("late-open"),
            }

            assert_matrix_action_waiting(&app, action);
            assert!(matches!(
                app.overlay(),
                Some(super::Overlay::CaptureProgress {
                    action: Some(shown),
                    discarding: true,
                }) if *shown == action
            ));
            assert!(app.maintain_capture());
            assert_eq!(
                app.take_worker_requests(),
                [WorkerRequest::ReleaseManagedCapture { id: discard_id }]
            );
            assert!(
                app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                    id: discard_id,
                    result: Ok(()),
                })
            );
            assert_matrix_action_waiting(&app, action);
            assert!(app.maintain_capture());
            assert_matrix_action_continued(&app, action);
        }
    }

    #[test]
    fn new_capture_is_rejected_while_exact_discard_release_is_pending() {
        let mut app = app_waiting_for_exact_discard_release(604);

        assert!(matches!(
            app.request_capture_with_limit_for_test(CaptureSource::Input, 8),
            Err(CaptureError::AlreadyActive)
        ));
        assert_eq!(app.capture_session().phase(), None);
        assert!(
            app.pending_managed_releases
                .contains(&ManagedCaptureId::new(604))
        );
    }

    fn app_with_ready_managed_capture(id: u64) -> App {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        start_capture(&mut app, CaptureSource::Resample, 8);
        drive_ready(&mut app, &probe, CaptureSource::Resample, id, false);
        app
    }

    fn assert_new_capture_admission_fenced(app: &mut App) {
        assert!(matches!(
            app.request_capture_with_limit_for_test(CaptureSource::Input, 8),
            Err(CaptureError::AlreadyActive)
        ));
    }

    #[test]
    fn ready_capture_discard_waits_through_every_exact_release_boundary() {
        let discard_id = ManagedCaptureId::new(605);
        let mut app = app_with_ready_managed_capture(discard_id.get());
        app.request_open_project_interactive("after-ready-discard-release");
        for index in 0..WORKER_CHANNEL_CAPACITY {
            app.pending_worker_requests
                .push(WorkerRequest::ReleaseManagedCapture {
                    id: ManagedCaptureId::new(800 + index as u64),
                });
        }

        app.apply_key(press(KeyCode::Backspace));

        assert_eq!(app.capture_session().phase(), None);
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert_new_capture_admission_fenced(&mut app);
        assert!(!app.maintain_capture());
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert_new_capture_admission_fenced(&mut app);

        app.pending_worker_requests.clear();
        assert!(app.maintain_capture());
        let [release] = app.take_worker_requests().try_into().unwrap();
        assert_eq!(
            release,
            WorkerRequest::ReleaseManagedCapture { id: discard_id }
        );
        assert_new_capture_admission_fenced(&mut app);

        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: ManagedCaptureId::new(999),
                result: Ok(()),
            })
        );
        assert!(!app.maintain_capture());
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert_eq!(app.managed_release_in_flight(), Some(discard_id));
        assert_new_capture_admission_fenced(&mut app);

        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: discard_id,
                result: Err(crate::capture_store::CaptureStoreError::NotLive { id: discard_id }),
            })
        );
        assert!(app.maintain_capture());
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert_eq!(app.managed_release_in_flight(), None);
        assert_new_capture_admission_fenced(&mut app);

        assert!(app.maintain_capture());
        let [release] = app.take_worker_requests().try_into().unwrap();
        assert_eq!(
            release,
            WorkerRequest::ReleaseManagedCapture { id: discard_id }
        );
        assert!(app.apply_worker_send_error(release, WorkerSendError::WorkerBusy));
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert_eq!(app.managed_release_in_flight(), None);
        assert_new_capture_admission_fenced(&mut app);

        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture { id: discard_id }]
        );
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: discard_id,
                result: Ok(()),
            })
        );
        assert_matrix_action_waiting(&app, ProjectAction::Open);
        assert_eq!(app.managed_release_in_flight(), Some(discard_id));
        assert_new_capture_admission_fenced(&mut app);
        assert!(app.maintain_capture());
        assert_matrix_action_continued(&app, ProjectAction::Open);
    }

    #[test]
    fn standalone_ready_capture_cancel_fences_a_new_take_until_exact_release_success() {
        let discard_id = ManagedCaptureId::new(606);
        let mut app = app_with_ready_managed_capture(discard_id.get());

        app.cancel_capture().unwrap();

        assert_eq!(app.capture_session().phase(), None);
        assert_new_capture_admission_fenced(&mut app);
        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture { id: discard_id }]
        );
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: discard_id,
                result: Ok(()),
            })
        );
        assert_new_capture_admission_fenced(&mut app);
        assert!(app.maintain_capture());
        app.request_capture_with_limit_for_test(CaptureSource::Input, 8)
            .unwrap();
        assert_eq!(app.capture_session().phase(), Some(CapturePhase::Recording));
    }

    #[test]
    fn capture_lifecycle_async_choice_matrix_preserves_intent_ownership_and_continuation() {
        let phases = [
            CapturePhase::Confirm,
            CapturePhase::Recording,
            CapturePhase::Finalizing,
            CapturePhase::ReadyToInstall,
            CapturePhase::Failed,
        ];
        let choices = [
            CaptureLifecycleChoice::Finalize,
            CaptureLifecycleChoice::Discard,
            CaptureLifecycleChoice::Cancel,
        ];
        let actions = [ProjectAction::Quit, ProjectAction::Open];
        let mut managed_id = 400;

        for action in actions {
            for phase in phases {
                for choice in choices {
                    managed_id += 1;
                    let mut fixture = capture_lifecycle_matrix_fixture(phase);
                    begin_matrix_project_action(&mut fixture.app, action);
                    insert_matrix_async_boundary(&mut fixture, phase);
                    assert!(
                        matches!(
                            fixture.app.overlay(),
                            Some(super::Overlay::ResolveCapture { action: shown }) if *shown == action
                        ),
                        "async boundary erased {action:?}/{phase:?}/{choice:?}",
                    );
                    assert_matrix_action_waiting(&fixture.app, action);

                    let before_choice_phase = fixture.app.capture_session().phase();
                    let was_retryable = fixture.app.capture_session().failure_is_retryable();
                    let ready_discard_id = match phase {
                        CapturePhase::Finalizing => Some(ManagedCaptureId::new(302)),
                        CapturePhase::ReadyToInstall => Some(ManagedCaptureId::new(301)),
                        _ => None,
                    };
                    fixture.app.apply_key(press(choice.key()));
                    match choice {
                        CaptureLifecycleChoice::Cancel => {
                            assert_eq!(
                                fixture.app.capture_session().phase(),
                                before_choice_phase,
                                "Cancel failed to preserve {action:?}/{phase:?}",
                            );
                            assert_eq!(fixture.app.pending_project_action, None);
                            assert!(!fixture.app.should_quit());
                            assert!(fixture.app.project_open_stage().is_none());
                        }
                        CaptureLifecycleChoice::Discard => {
                            if let Some(discard_id) = ready_discard_id {
                                assert_eq!(fixture.app.capture_session().phase(), None);
                                assert_matrix_action_waiting(&fixture.app, action);
                                assert!(fixture.app.maintain_capture());
                                assert_eq!(
                                    fixture.app.take_worker_requests(),
                                    [WorkerRequest::ReleaseManagedCapture { id: discard_id }]
                                );
                                assert!(fixture.app.apply_worker_result(
                                    WorkerResult::ManagedCaptureReleased {
                                        id: discard_id,
                                        result: Ok(()),
                                    }
                                ));
                                assert_matrix_action_waiting(&fixture.app, action);
                                assert!(fixture.app.maintain_capture());
                                assert_matrix_action_continued(&fixture.app, action);
                            } else {
                                assert_matrix_action_continued(&fixture.app, action);
                            }
                        }
                        CaptureLifecycleChoice::Finalize => {
                            if phase == CapturePhase::Failed {
                                assert!(was_retryable, "matrix Failed fixture must be retryable");
                            }
                            finish_matrix_finalize(&mut fixture, action, managed_id);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn capture_lifecycle_quit_and_open_cancel_preserve_every_unresolved_phase() {
        for phase in [
            CapturePhase::Confirm,
            CapturePhase::Recording,
            CapturePhase::Finalizing,
            CapturePhase::ReadyToInstall,
            CapturePhase::Failed,
        ] {
            for action in [ProjectAction::Quit, ProjectAction::Open] {
                let mut app = app_at_capture_phase(phase);
                match action {
                    ProjectAction::Quit => app.apply(InputAction::Quit),
                    ProjectAction::Open => app.request_open_project_interactive("next-project"),
                }
                assert!(!app.should_quit(), "{action:?} bypassed {phase:?}");
                assert!(
                    app.project_open_stage().is_none(),
                    "open bypassed {phase:?}"
                );
                assert!(
                    app.overlay().is_some(),
                    "{action:?} did not offer capture choices"
                );

                app.apply_key(press(KeyCode::Esc));
                assert_eq!(app.capture_session().phase(), Some(phase));
                assert!(!app.should_quit());
                assert!(app.project_open_stage().is_none());
            }
        }
    }

    #[test]
    fn capture_lifecycle_finalize_waits_for_worker_and_audio_then_enters_existing_project_resolution()
     {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));
        start_capture(&mut app, CaptureSource::Resample, 8);
        app.request_open_project_interactive("next-project");
        app.apply_key(press(KeyCode::Enter));
        assert!(
            !probe
                .calls()
                .iter()
                .any(|call| matches!(call, CaptureCall::Install(..)))
        );
        assert!(app.project_open_stage().is_none());

        probe.complete(completion(
            &app,
            CaptureSource::Resample,
            vec![0.25, -0.25],
            false,
        ));
        assert!(app.maintain_capture());
        assert!(app.maintain_capture());
        let request = take_finalize(&mut app);
        assert!(app.apply_worker_result(finalized(
            &request,
            Ok(managed_capture(91, 48_000, b"lifecycle")),
        )));
        assert!(app.maintain_capture());
        probe.fail_next_install();
        assert!(app.maintain_capture());
        assert!(app.project_open_stage().is_none());
        assert_eq!(
            app.capture_session().phase(),
            Some(CapturePhase::ReadyToInstall)
        );

        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), None);
        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::UnsavedProject {
                action: ProjectAction::Open
            })
        ));
        assert!(app.project_open_stage().is_none());
    }

    #[test]
    fn capture_lifecycle_discard_waits_for_callback_or_worker_ownership_before_continuing() {
        let (audio, _probe) = CaptureAudio::new(48_000, 44_100);
        let mut recording = App::with_audio(Box::new(audio));
        start_capture(&mut recording, CaptureSource::Input, 8);
        recording.apply(InputAction::Quit);
        recording.apply_key(press(KeyCode::Backspace));
        assert!(!recording.should_quit());
        assert_eq!(
            recording.capture_session().phase(),
            Some(CapturePhase::Recording)
        );
        assert!(recording.maintain_capture());
        assert!(recording.should_quit());

        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut worker = App::with_audio(Box::new(audio));
        start_capture(&mut worker, CaptureSource::Resample, 8);
        probe.complete(completion(
            &worker,
            CaptureSource::Resample,
            vec![0.25, -0.25],
            false,
        ));
        assert!(worker.maintain_capture());
        assert!(worker.maintain_capture());
        let request = take_finalize(&mut worker);
        worker.request_open_project_interactive("next-project");
        worker.apply_key(press(KeyCode::Backspace));
        assert!(worker.project_open_stage().is_none());
        assert_eq!(
            worker.capture_session().phase(),
            Some(CapturePhase::Finalizing)
        );

        assert!(worker.apply_worker_result(finalized(
            &request,
            Ok(managed_capture(92, 48_000, b"discarded")),
        )));
        assert!(worker.maintain_capture());
        assert_eq!(worker.capture_session().phase(), None);
        assert!(worker.project_open_stage().is_none());
        assert!(
            worker
                .pending_managed_releases
                .contains(&ManagedCaptureId::new(92))
        );
        assert!(worker.maintain_capture());
        assert_eq!(
            worker.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture {
                id: ManagedCaptureId::new(92),
            }]
        );
        assert!(
            worker.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: ManagedCaptureId::new(92),
                result: Ok(()),
            })
        );
        assert!(worker.maintain_capture());
        assert!(worker.project_open_stage().is_some());
    }

    #[test]
    fn capture_lifecycle_retries_post_unlink_sync_failure_once_before_quit_or_open_continues() {
        for action in [ProjectAction::Quit, ProjectAction::Open] {
            let (audio, probe) = CaptureAudio::new(48_000, 44_100);
            let mut app = App::with_audio(Box::new(audio));
            start_capture(&mut app, CaptureSource::Resample, 8);
            probe.complete(completion(
                &app,
                CaptureSource::Resample,
                vec![0.25, -0.25],
                false,
            ));
            assert!(app.maintain_capture());
            assert!(app.maintain_capture());
            let finalize = take_finalize(&mut app);
            let mut store = CaptureStore::new().unwrap();
            let candidate = store.finalize(Arc::from([0.2_f32, -0.2]), 48_000).unwrap();
            let managed_id = candidate.id;
            let managed_path = candidate.path.clone();
            assert!(app.apply_worker_result(finalized(&finalize, Ok(candidate))));
            assert!(app.maintain_capture());
            assert_eq!(
                app.capture_session().phase(),
                Some(CapturePhase::ReadyToInstall)
            );

            match action {
                ProjectAction::Quit => app.apply(InputAction::Quit),
                ProjectAction::Open => {
                    app.request_open_project_interactive("next-project-after-discard")
                }
            }
            app.apply_key(press(KeyCode::Backspace));
            assert_eq!(app.capture_session().phase(), None);
            assert!(!app.should_quit());
            assert!(app.project_open_stage().is_none());

            assert!(app.maintain_capture());
            assert_eq!(
                app.take_worker_requests(),
                [WorkerRequest::ReleaseManagedCapture { id: managed_id }]
            );
            let sync_error = store
                .release_with_sync_for_test(managed_id, |_, _| {
                    Err(CaptureStoreError::Filesystem {
                        operation: "injected post-unlink directory sync failure",
                        path: PathBuf::from("injected"),
                        kind: std::io::ErrorKind::Other,
                    })
                })
                .unwrap_err();
            assert!(!managed_path.exists());
            assert!(
                app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                    id: managed_id,
                    result: Err(sync_error),
                })
            );
            assert!(app.maintain_capture());
            assert!(!app.should_quit());
            assert!(app.project_open_stage().is_none());

            assert!(app.maintain_capture());
            assert_eq!(
                app.take_worker_requests(),
                [WorkerRequest::ReleaseManagedCapture { id: managed_id }]
            );
            store.release(managed_id).unwrap();
            assert!(
                app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                    id: managed_id,
                    result: Ok(()),
                })
            );
            assert!(app.maintain_capture());
            assert_eq!(app.managed_release_in_flight(), None);
            assert!(app.capture_discard_release_pending.is_none());

            match action {
                ProjectAction::Quit => assert!(app.should_quit()),
                ProjectAction::Open => assert!(app.project_open_stage().is_some()),
            }
            assert!(
                app.take_worker_requests()
                    .iter()
                    .all(|request| !matches!(request, WorkerRequest::ReleaseManagedCapture { .. }))
            );
            assert!(matches!(
                store.release(managed_id),
                Err(CaptureStoreError::NotLive { id }) if id == managed_id
            ));
        }
    }

    #[test]
    fn capture_lifecycle_failed_finalize_is_cancelable_and_only_typed_worker_failure_retries() {
        let mut retryable = app_at_capture_phase(CapturePhase::Failed);
        let before_generation = retryable.capture_session().generation();
        retryable.apply(InputAction::Quit);
        retryable.apply_key(press(KeyCode::Enter));
        assert_eq!(
            retryable.capture_session().generation(),
            before_generation.and_then(|value| value.checked_add(1))
        );
        assert_eq!(
            retryable.capture_session().phase(),
            Some(CapturePhase::Finalizing)
        );

        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut device = App::with_audio(Box::new(audio));
        start_capture(&mut device, CaptureSource::Input, 8);
        probe.fail_device(CaptureSource::Input, "input device disappeared");
        assert!(device.maintain_capture());
        let before = (
            device.capture_session().generation(),
            device.capture_session().failure_cause(),
        );
        device.apply(InputAction::Quit);
        device.apply_key(press(KeyCode::Enter));
        assert_eq!(
            (
                device.capture_session().generation(),
                device.capture_session().failure_cause(),
            ),
            before
        );
        device.apply_key(press(KeyCode::Esc));
        assert_eq!(device.capture_session().phase(), Some(CapturePhase::Failed));
        assert!(!device.should_quit());
        device.apply_key(press(KeyCode::Char('c')));
        assert_eq!(device.capture_session().phase(), None);
    }

    #[test]
    fn capture_lifecycle_stop_all_and_held_pad_release_pass_through_every_capture_presentation() {
        let mut presentations = Vec::new();
        for phase in [
            CapturePhase::Confirm,
            CapturePhase::Recording,
            CapturePhase::Finalizing,
            CapturePhase::ReadyToInstall,
            CapturePhase::Failed,
        ] {
            presentations.push((phase, app_at_capture_phase(phase)));
        }
        let mut discard = app_at_capture_phase(CapturePhase::Recording);
        discard.apply_key(press(KeyCode::Esc));
        presentations.push((CapturePhase::Recording, discard));
        let mut lifecycle = app_at_capture_phase(CapturePhase::Recording);
        lifecycle.apply(InputAction::Quit);
        presentations.push((CapturePhase::Recording, lifecycle));

        for (phase, mut app) in presentations {
            app.set_keyboard_capabilities(crate::KeyboardCapabilities {
                release_events: true,
            });
            app.apply(InputAction::PadPress(0));
            assert!(app.is_pad_held(0), "setup failed for {phase:?}");
            app.apply_key(release(KeyCode::Char('1')));
            assert!(!app.is_pad_held(0), "release was swallowed for {phase:?}");

            app.apply(InputAction::PadPress(0));
            assert!(app.is_pad_held(0), "second setup failed for {phase:?}");
            app.apply_key(KeyEvent::new_with_kind(
                KeyCode::Esc,
                KeyModifiers::SHIFT,
                KeyEventKind::Press,
            ));
            assert!(!app.is_pad_held(0), "Stop All was swallowed for {phase:?}");
            assert_eq!(app.capture_session().phase(), Some(phase));
        }
    }

    #[test]
    fn capture_lifecycle_every_palette_command_routes_to_the_typed_capture_api() {
        let (audio, probe) = CaptureAudio::new(48_000, 44_100);
        let mut app = App::with_audio(Box::new(audio));

        app.open_palette();
        app.apply_terminal_event(Event::Paste("record-input".to_owned()));
        app.apply_key(press(KeyCode::Enter));
        assert_eq!(app.capture_session().source(), Some(CaptureSource::Input));

        app.open_palette();
        app.apply_terminal_event(Event::Paste("capture-stop".to_owned()));
        app.apply_key(press(KeyCode::Enter));
        assert!(matches!(
            probe.calls().last(),
            Some(CaptureCall::Stop(_, _))
        ));

        app.open_palette();
        app.apply_terminal_event(Event::Paste("capture-cancel".to_owned()));
        app.apply_key(press(KeyCode::Enter));
        assert!(matches!(
            probe.calls().last(),
            Some(CaptureCall::Cancel(_, _))
        ));
        assert!(app.maintain_capture());
        assert_eq!(app.capture_session().phase(), None);

        app.open_palette();
        app.apply_terminal_event(Event::Paste("resample".to_owned()));
        app.apply_key(press(KeyCode::Enter));
        assert_eq!(
            app.capture_session().source(),
            Some(CaptureSource::Resample)
        );
    }
}

impl fmt::Display for ProjectSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Untitled => formatter.write_str("untitled project requires Save As"),
            Self::OperationPending => formatter.write_str("a project operation is already pending"),
            Self::Snapshot(error) => error.fmt(formatter),
            Self::Entropy(error) => {
                write!(formatter, "could not generate project identity: {error}")
            }
            Self::TokenExhausted => formatter.write_str("project operation token is exhausted"),
        }
    }
}

impl std::error::Error for ProjectSaveError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSaveFailure {
    pub kind: SaveKind,
    pub error: ProjectStoreError,
}

#[derive(Debug, Clone)]
struct PendingProjectSave {
    descriptor: crate::ProjectOperationDescriptor,
    snapshot: ProjectSaveSnapshot,
    save_as: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryCleanup {
    token: ProjectToken,
    directory: PathBuf,
    project_id: ProjectId,
    revision: u64,
}

#[derive(Debug, Clone)]
enum InFlightProjectOperation {
    Save(Box<PendingProjectSave>),
    Cleanup(RecoveryCleanup),
}

struct StagedProjectPad {
    path: PathBuf,
    settings: PadSettings,
    mix: PadMixSettings,
    loaded: crate::LoadedSample,
}

enum ProjectAdmission {
    MidiOwners,
    StopAll,
    Master,
    Pads(usize),
    RestoreCommittedMaster { end_pad: usize },
    RestoreCommittedPads { next_pad: usize, end_pad: usize },
    Patterns(usize),
    Complete,
}

struct ProjectOpenCandidate {
    progress: ProjectOpenStage,
    document: ProjectDocument,
    patterns: PatternWorkspace,
    staged_pads: [Option<Box<StagedProjectPad>>; PAD_VIEW_COUNT],
    next_decode: usize,
    decode_in_flight: Option<(PadId, u64)>,
    stage_generation: u64,
    engine_rate: u32,
    saved_revision: u64,
    restored_recovery: bool,
    admission: ProjectAdmission,
}

enum ProjectOpenOperation {
    Probing {
        progress: ProjectOpenStage,
        worker_queued: bool,
    },
    ChoosingRecovery(Box<ProjectRecoveryChoiceState>),
    Staging(Box<ProjectOpenCandidate>),
}

struct ProjectRecoveryChoiceState {
    progress: ProjectOpenStage,
    explicit: Option<ProjectDocument>,
    recovery: ProjectDocument,
    discard_requested: bool,
    discard_queued: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviewColumn {
    pub min: i8,
    pub max: i8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PadLoadState {
    Empty,
    WaitingForDevice,
    Loading,
    Ready,
    Error(String),
}

pub struct PadView {
    pub source: Option<PathBuf>,
    pub label: String,
    pub settings: PadSettings,
    pub generation: u64,
    pub state: PadLoadState,
    pub sample: Option<Arc<SampleBuffer>>,
    pub preview: [PreviewColumn; PREVIEW_COLUMNS],
    pub active: bool,
}

impl Default for PadView {
    fn default() -> Self {
        Self {
            source: None,
            label: String::new(),
            settings: PadSettings::default(),
            generation: 0,
            state: PadLoadState::Empty,
            sample: None,
            preview: [PreviewColumn::default(); PREVIEW_COLUMNS],
            active: false,
        }
    }
}

enum PendingLoadPhase {
    AwaitingWorker,
    WorkerQueued,
    Ready(crate::loader::LoadedSample),
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingLoadKind {
    User,
    Recovery,
}

impl PendingLoadKind {
    fn purpose(self) -> LoadPurpose {
        match self {
            Self::User => LoadPurpose::User,
            Self::Recovery => LoadPurpose::Recovery,
        }
    }
}

struct PendingLoad {
    path: PathBuf,
    phase: PendingLoadPhase,
    kind: PendingLoadKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEditStatus {
    Idle,
    AwaitingWorker,
    Rendering,
    ReadyToInstall,
    Failed,
    GenerationExhausted,
    UndoAvailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleEditRequestError {
    InvalidRecipe(String),
    AudioUnavailable(String),
    LoadPending,
    EmptyPad,
    NoUndo,
    RecoveryPending,
    GenerationExhausted,
    ProjectRevisionExhausted,
}

impl fmt::Display for SampleEditRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecipe(error) | Self::AudioUnavailable(error) => {
                formatter.write_str(error)
            }
            Self::LoadPending => formatter.write_str("sample load is pending"),
            Self::EmptyPad => formatter.write_str("pad has no committed sample to edit"),
            Self::NoUndo => formatter.write_str("no sample edit to undo"),
            Self::RecoveryPending => {
                formatter.write_str("sample is waiting for device-rate recovery")
            }
            Self::GenerationExhausted => formatter.write_str("sample edit generation is exhausted"),
            Self::ProjectRevisionExhausted => formatter.write_str("project revision is exhausted"),
        }
    }
}

impl std::error::Error for SampleEditRequestError {}

enum PendingEditPhase {
    AwaitingWorker,
    WorkerQueued,
    Ready(RenderedSample),
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingEditKind {
    Apply,
    Undo,
}

struct PendingEdit {
    generation: u64,
    base: Arc<SampleBuffer>,
    base_preview: EditPreview,
    recipe: SampleEditRecipe,
    kind: PendingEditKind,
    phase: PendingEditPhase,
}

struct SampleEditCheckpoint {
    base: Arc<SampleBuffer>,
    rendered: Arc<SampleBuffer>,
    recipe: SampleEditRecipe,
    base_preview: EditPreview,
    rendered_preview: EditPreview,
}

struct SampleCommit {
    base: Option<Arc<SampleBuffer>>,
    source_generation: u64,
    fingerprint: Option<SourceFingerprint>,
    recipe: SampleEditRecipe,
    base_preview: Option<EditPreview>,
    rendered_preview: Option<EditPreview>,
    managed_capture: Option<ManagedCaptureId>,
}

impl Default for SampleCommit {
    fn default() -> Self {
        Self {
            base: None,
            source_generation: 0,
            fingerprint: None,
            recipe: SampleEditRecipe::identity(),
            base_preview: None,
            rendered_preview: None,
            managed_capture: None,
        }
    }
}

struct SampleEditorState {
    commits: [SampleCommit; PAD_VIEW_COUNT],
    generations: [u64; PAD_VIEW_COUNT],
    pending: [Option<Box<PendingEdit>>; PAD_VIEW_COUNT],
    deferred_results: [Option<Box<WorkerResult>>; PAD_VIEW_COUNT],
    undo: [Option<Box<SampleEditCheckpoint>>; PAD_VIEW_COUNT],
    generation_exhausted: [bool; PAD_VIEW_COUNT],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Help,
    Palette,
    FilePicker,
    DeviceError(String),
    ProjectOpenProgress,
    ResolveSampleDraft {
        pad: PadId,
        action: ProjectAction,
    },
    UnsavedProject {
        action: ProjectAction,
    },
    ProjectLifecycleProgress {
        action: ProjectAction,
    },
    ProjectSaveProgress,
    ProjectError {
        title: String,
        message: String,
    },
    ClearPattern {
        slot: PatternSlotId,
        event_count: usize,
    },
    ApplySample {
        pad: PadId,
        before_frames: usize,
        after_frames: usize,
    },
    DiscardSample {
        pad: PadId,
    },
    CaptureConfirm,
    CaptureDiscard,
    ResolveCapture {
        action: ProjectAction,
    },
    CaptureProgress {
        action: Option<ProjectAction>,
        discarding: bool,
    },
    CaptureFailed {
        action: Option<ProjectAction>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectAction {
    Open,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingProjectAction {
    Open(PathBuf),
    Quit,
}

impl PendingProjectAction {
    const fn label(&self) -> ProjectAction {
        match self {
            Self::Open(_) => ProjectAction::Open,
            Self::Quit => ProjectAction::Quit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectLifecycleWait {
    ChoosingCapture,
    CaptureFinalize,
    CaptureDiscard,
    SampleApply,
    ChoosingProject,
    Saving {
        token: ProjectToken,
        action_revision: u64,
    },
    DiscardingRecovery {
        cleanup: RecoveryCleanup,
        action_revision: u64,
    },
}

#[derive(Debug, Clone, Copy)]
struct PendingPatternTransport {
    playing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MidiOwnedVoice {
    pad: PadId,
    trigger_id: LiveCommandId,
}

#[derive(Clone)]
struct ApplySampleContext {
    pad: PadId,
    pad_generation: u64,
    source: Option<PathBuf>,
    base_frames: usize,
    base_rate: u32,
}

struct CaptureTargetFence {
    target: PadId,
    pad_generation: u64,
    edit_generation: u64,
    source: Option<PathBuf>,
    settings: PadSettings,
    mix: PadMixSettings,
    sample: Option<Arc<SampleBuffer>>,
    preview: [PreviewColumn; PREVIEW_COLUMNS],
    base: Option<Arc<SampleBuffer>>,
    source_generation: u64,
    fingerprint: Option<SourceFingerprint>,
    recipe: SampleEditRecipe,
    base_preview: Option<EditPreview>,
    rendered_preview: Option<EditPreview>,
    managed_capture: Option<ManagedCaptureId>,
}

pub struct App {
    active_bank: BankId,
    selected_pad: usize,
    pads: [PadView; PAD_VIEW_COUNT],
    pad_mixes: [PadMixSettings; PAD_VIEW_COUNT],
    master_mix: MasterMixSettings,
    patterns: PatternWorkspace,
    mixer_cursor: MixerCursor,
    capture_session: crate::CaptureSession,
    capture_target_fence: Option<CaptureTargetFence>,
    capture_source_pcm: Option<Arc<[f32]>>,
    capture_hard_limit: bool,
    capture_worker_request: Option<FinalizeCaptureRequest>,
    capture_ready: Option<ManagedCapture>,
    capture_status: Option<CaptureStatus>,
    capture_discard_pending: bool,
    capture_worker_results: VecDeque<WorkerResult>,
    pending_managed_releases: VecDeque<ManagedCaptureId>,
    managed_release_in_flight: Option<ManagedCaptureId>,
    capture_discard_release_pending: Option<ManagedCaptureId>,
    midi_service: Option<MidiService>,
    audio: Option<Box<dyn AudioPort>>,
    audio_format: Option<(u32, u16)>,
    held_pad_by_key: [Option<PadId>; PADS_PER_BANK as usize],
    midi_settings: MidiSettings,
    midi_learn_target: Option<PadId>,
    midi_owned_pads: Box<[Option<MidiOwnedVoice>; MIDI_OWNERSHIP_COUNT]>,
    overlay: Option<Overlay>,
    palette: LineEditor,
    palette_error: Option<String>,
    current_dir: PathBuf,
    file_picker: FilePicker,
    pending_worker_requests: Vec<WorkerRequest>,
    recovery_cursor: Option<usize>,
    pending_loads: [Option<Box<PendingLoad>>; PAD_VIEW_COUNT],
    committed_recovery_loads: [Option<Box<PendingLoad>>; PAD_VIEW_COUNT],
    recovery_generations: [u64; PAD_VIEW_COUNT],
    reinstall_pending: [bool; PAD_VIEW_COUNT],
    current_session_bound: [bool; PAD_VIEW_COUNT],
    sample_editor: Box<SampleEditorState>,
    editor: SampleEditor,
    apply_sample_context: Option<ApplySampleContext>,
    edit_result_advanced: bool,
    device_retry_requests: usize,
    keyboard_capabilities: KeyboardCapabilities,
    status: String,
    audio_unavailable_message: Option<String>,
    telemetry: Telemetry,
    meter_left: f32,
    meter_right: f32,
    recorded_ack_count: usize,
    pattern_submission_count: usize,
    pending_pattern_transport: Option<PendingPatternTransport>,
    should_quit: bool,
    project_session: ProjectSession,
    next_project_token: u64,
    pending_explicit_save: Option<PendingProjectSave>,
    pending_autosave_save: Option<PendingProjectSave>,
    in_flight_project: Option<InFlightProjectOperation>,
    pending_recovery_cleanup: VecDeque<RecoveryCleanup>,
    save_as_identity: Option<(PathBuf, ProjectId, String)>,
    project_save_error: Option<ProjectSaveFailure>,
    recovery_cleanup_warning: Option<ProjectStoreError>,
    autosave_retry_clock_pending: bool,
    autosave_retry_since: Option<Instant>,
    project_open: Option<ProjectOpenOperation>,
    project_open_error: Option<ProjectOpenError>,
    pending_project_action: Option<PendingProjectAction>,
    project_lifecycle_wait: Option<ProjectLifecycleWait>,
}

impl CaptureTargetFence {
    fn capture(app: &App, target: PadId) -> Self {
        let offset = pad_offset(target);
        let view = &app.pads[offset];
        let commit = &app.sample_editor.commits[offset];
        Self {
            target,
            pad_generation: view.generation,
            edit_generation: app.sample_editor.generations[offset],
            source: view.source.clone(),
            settings: view.settings,
            mix: app.pad_mixes[offset],
            sample: view.sample.clone(),
            preview: view.preview,
            base: commit.base.clone(),
            source_generation: commit.source_generation,
            fingerprint: commit.fingerprint,
            recipe: commit.recipe,
            base_preview: commit.base_preview.clone(),
            rendered_preview: commit.rendered_preview.clone(),
            managed_capture: commit.managed_capture,
        }
    }

    fn matches(&self, app: &App) -> bool {
        let offset = pad_offset(self.target);
        let view = &app.pads[offset];
        let commit = &app.sample_editor.commits[offset];
        self.pad_generation == view.generation
            && self.edit_generation == app.sample_editor.generations[offset]
            && self.source == view.source
            && self.settings == view.settings
            && self.mix == app.pad_mixes[offset]
            && Self::same_arc(&self.sample, &view.sample)
            && self.preview == view.preview
            && Self::same_arc(&self.base, &commit.base)
            && self.source_generation == commit.source_generation
            && self.fingerprint == commit.fingerprint
            && self.recipe == commit.recipe
            && Self::same_arc(&self.base_preview, &commit.base_preview)
            && Self::same_arc(&self.rendered_preview, &commit.rendered_preview)
            && self.managed_capture == commit.managed_capture
    }

    fn same_arc<T: ?Sized>(left: &Option<Arc<T>>, right: &Option<Arc<T>>) -> bool {
        match (left, right) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        }
    }
}

impl App {
    pub fn with_audio(audio: Box<dyn AudioPort>) -> Self {
        Self::new(Some(audio), None)
    }

    pub fn without_audio(error: impl Into<String>) -> Self {
        let error = error.into();
        Self::new(None, Some(error))
    }

    pub fn with_audio_and_midi(audio: Box<dyn AudioPort>, midi_service: MidiService) -> Self {
        let mut app = Self::with_audio(audio);
        app.midi_service = Some(midi_service);
        app
    }

    fn new(mut audio: Option<Box<dyn AudioPort>>, mut audio_error: Option<String>) -> Self {
        if let Some(candidate) = audio.as_mut()
            && let Err(error) = candidate.update_master_mix(MasterMixSettings::default())
        {
            audio = None;
            audio_error = Some(error);
        }
        let overlay = audio_error.clone().map(Overlay::DeviceError);
        let audio_format = audio
            .as_ref()
            .map(|audio| (audio.sample_rate(), audio.channels()));
        let pattern_sample_rate = audio_format.map_or(48_000, |(sample_rate, _)| sample_rate);
        let current_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from(std::path::MAIN_SEPARATOR_STR));
        Self {
            active_bank: BankId::new(0).expect("bank zero is valid"),
            selected_pad: 0,
            pads: array::from_fn(|_| PadView::default()),
            pad_mixes: [PadMixSettings::default(); PAD_VIEW_COUNT],
            master_mix: MasterMixSettings::default(),
            patterns: PatternWorkspace::new(pattern_sample_rate),
            mixer_cursor: MixerCursor::default(),
            capture_session: crate::CaptureSession::default(),
            capture_target_fence: None,
            capture_source_pcm: None,
            capture_hard_limit: false,
            capture_worker_request: None,
            capture_ready: None,
            capture_status: None,
            capture_discard_pending: false,
            capture_worker_results: VecDeque::new(),
            pending_managed_releases: VecDeque::new(),
            managed_release_in_flight: None,
            capture_discard_release_pending: None,
            audio,
            audio_format,
            held_pad_by_key: [None; PADS_PER_BANK as usize],
            midi_settings: MidiSettings::default(),
            midi_learn_target: None,
            midi_owned_pads: Box::new([None; MIDI_OWNERSHIP_COUNT]),
            midi_service: None,
            overlay,
            palette: LineEditor::default(),
            palette_error: None,
            file_picker: FilePicker::new(current_dir.clone()),
            current_dir,
            pending_worker_requests: Vec::new(),
            recovery_cursor: None,
            pending_loads: array::from_fn(|_| None),
            committed_recovery_loads: array::from_fn(|_| None),
            recovery_generations: [0; PAD_VIEW_COUNT],
            reinstall_pending: [false; PAD_VIEW_COUNT],
            current_session_bound: [false; PAD_VIEW_COUNT],
            sample_editor: Box::new(SampleEditorState {
                commits: array::from_fn(|_| SampleCommit::default()),
                generations: [0; PAD_VIEW_COUNT],
                pending: array::from_fn(|_| None),
                deferred_results: array::from_fn(|_| None),
                undo: array::from_fn(|_| None),
                generation_exhausted: [false; PAD_VIEW_COUNT],
            }),
            editor: SampleEditor::open_empty(PadId::first(), PadSettings::default()),
            apply_sample_context: None,
            edit_result_advanced: false,
            device_retry_requests: 0,
            keyboard_capabilities: KeyboardCapabilities::default(),
            status: audio_error.clone().unwrap_or_default(),
            audio_unavailable_message: audio_error,
            telemetry: Telemetry {
                active_pads: [0; 3],
                rendered_frame: 0,
                last_triggered_frame: None,
                peak_left: 0.0,
                peak_right: 0.0,
                active_voices: 0,
                late_commands: 0,
                invalid_commands: 0,
                command_overflows: 0,
                pattern_slot: None,
                pattern_generation: None,
                pattern_playing: false,
                pattern_recording: false,
                pattern_origin: None,
                pattern_playhead: 0,
                pattern_loop_count: 0,
                pattern_overflows: 0,
                live_ack_overflows: 0,
            },
            meter_left: 0.0,
            meter_right: 0.0,
            recorded_ack_count: 0,
            pattern_submission_count: 0,
            pending_pattern_transport: None,
            should_quit: false,
            project_session: ProjectSession::new(
                ProjectId::from_bytes([0; 16]),
                None,
                "Untitled",
                0,
            ),
            next_project_token: 1,
            pending_explicit_save: None,
            pending_autosave_save: None,
            in_flight_project: None,
            pending_recovery_cleanup: VecDeque::with_capacity(WORKER_CHANNEL_CAPACITY),
            save_as_identity: None,
            project_save_error: None,
            recovery_cleanup_warning: None,
            autosave_retry_clock_pending: false,
            autosave_retry_since: None,
            project_open: None,
            project_open_error: None,
            pending_project_action: None,
            project_lifecycle_wait: None,
        }
    }

    pub const fn capture_session(&self) -> &crate::CaptureSession {
        &self.capture_session
    }

    pub const fn capture_session_mut(&mut self) -> &mut crate::CaptureSession {
        &mut self.capture_session
    }

    pub const fn capture_status_view(&self) -> Option<CaptureStatus> {
        self.capture_status
    }

    fn capture_target_fence_for(&self, target: PadId) -> CaptureTargetFence {
        CaptureTargetFence::capture(self, target)
    }

    fn capture_target_fence_matches(&self) -> bool {
        let Some(fence) = self.capture_target_fence.as_ref() else {
            return false;
        };
        self.capture_session.target() == Some(fence.target) && fence.matches(self)
    }

    fn mark_capture_target_changed(&mut self) -> CaptureError {
        let target = self
            .capture_session
            .target()
            .expect("target fence requires an active capture");
        let error = CaptureError::TargetChanged(target);
        let message = error.to_string();
        let _ = self
            .capture_session
            .mark_failed_with_cause(CaptureFailureCause::InvalidCapture, message.clone());
        self.status = message;
        self.overlay = Some(Overlay::CaptureFailed {
            action: self.pending_capture_project_action(),
        });
        error
    }

    pub fn request_resample(&mut self) -> Result<(), CaptureError> {
        self.request_capture_with_frame_limit(CaptureSource::Resample, MAX_CAPTURE_FRAMES)
    }

    pub fn request_input_recording(&mut self) -> Result<(), CaptureError> {
        self.request_capture_with_frame_limit(CaptureSource::Input, MAX_CAPTURE_FRAMES)
    }

    /// Requests a capture with an explicit bounded frame limit.
    ///
    /// This follows the same admission, confirmation, finalization, and transactional install
    /// workflow as the interactive capture commands. The limit is validated before any capture
    /// session or presentation state changes, which lets constrained hosts choose a smaller
    /// deterministic bound while the built-in commands continue to use [`MAX_CAPTURE_FRAMES`].
    pub fn request_capture_with_frame_limit(
        &mut self,
        source: CaptureSource,
        max_frames: usize,
    ) -> Result<(), CaptureError> {
        self.request_capture(source, max_frames)
    }

    #[cfg(test)]
    pub(crate) fn request_capture_with_limit_for_test(
        &mut self,
        source: CaptureSource,
        max_frames: usize,
    ) -> Result<(), CaptureError> {
        self.request_capture_with_frame_limit(source, max_frames)
    }

    fn request_capture(
        &mut self,
        source: CaptureSource,
        max_frames: usize,
    ) -> Result<(), CaptureError> {
        if max_frames > MAX_CAPTURE_FRAMES {
            return Err(CaptureError::Command(
                sampler_audio::CaptureError::FrameLimitTooLarge { max_frames },
            ));
        }
        if self.capture_session.phase().is_some() || self.capture_discard_release_pending.is_some()
        {
            return Err(CaptureError::AlreadyActive);
        }
        if self.editor.is_dirty() {
            return Err(CaptureError::DirtySampleDraft(self.editor.pad()));
        }
        if self.project_open.is_some()
            || self.pending_explicit_save.is_some()
            || self.pending_autosave_save.is_some()
            || self.in_flight_project.is_some()
            || self.project_session.in_flight().is_some()
            || !self.pending_recovery_cleanup.is_empty()
            || self.pending_project_action.is_some()
            || self.project_lifecycle_wait.is_some()
        {
            return Err(CaptureError::ProjectOperationPending);
        }
        if let Some(offset) = (0..PAD_VIEW_COUNT).find(|offset| {
            self.pending_loads[*offset]
                .as_ref()
                .is_some_and(|pending| !matches!(pending.phase, PendingLoadPhase::Failed))
                || self.committed_recovery_loads[*offset]
                    .as_ref()
                    .is_some_and(|pending| !matches!(pending.phase, PendingLoadPhase::Failed))
                || self.sample_editor.pending[*offset]
                    .as_ref()
                    .is_some_and(|pending| !matches!(pending.phase, PendingEditPhase::Failed))
        }) {
            return Err(CaptureError::SampleOperationPending(pad_from_offset(
                offset,
            )));
        }
        let target = self
            .selected_pad_id()
            .ok_or(CaptureError::NoActiveCapture)?;
        let audio = self.audio.as_mut().ok_or(CaptureError::AudioUnavailable)?;
        if audio.capture_support() != crate::audio::CaptureSupport::Available {
            return Err(CaptureError::Unsupported);
        }
        let source_rate = audio.capture_source_rate(source)?;
        let target_fence = self.capture_target_fence_for(target);
        self.capture_session
            .begin(source, target, source_rate, max_frames)?;
        self.capture_target_fence = Some(target_fence);
        if self.pads[pad_offset(target)].sample.is_none() {
            self.confirm_capture()?;
        } else {
            self.overlay = Some(Overlay::CaptureConfirm);
            self.status = "Confirm replacement before capture starts".to_owned();
        }
        Ok(())
    }

    pub fn confirm_capture(&mut self) -> Result<(), CaptureError> {
        let phase = self
            .capture_session
            .phase()
            .ok_or(CaptureError::NoActiveCapture)?;
        let token = self.capture_session.token().expect("active token");
        let source = self.capture_session.source().expect("active source");
        if phase == CapturePhase::Confirm {
            if !self.capture_target_fence_matches() {
                return Err(self.mark_capture_target_changed());
            }
            let buffer = CaptureBuffer::try_new(
                token,
                self.capture_session.target().expect("active target"),
                source,
                self.capture_session.source_rate().expect("active rate"),
                self.capture_session
                    .max_frames()
                    .expect("active frame limit"),
            )
            .map_err(CaptureError::Command)?;
            let audio = self.audio.as_mut().ok_or(CaptureError::AudioUnavailable)?;
            if let Err(failure) = audio.begin_capture(buffer) {
                let error = failure.error().clone();
                drop(failure.into_command());
                return Err(error);
            }
            self.capture_session.mark_arming()?;
        } else if phase != CapturePhase::Arming {
            return Err(CaptureError::IllegalTransition {
                from: phase,
                to: CapturePhase::Arming,
            });
        }
        let audio = self.audio.as_mut().ok_or(CaptureError::AudioUnavailable)?;
        if let Err(failure) = audio.start_capture(source, token) {
            let error = failure.error().clone();
            drop(failure.into_command());
            return Err(error);
        }
        self.capture_session.mark_recording()?;
        self.capture_status = None;
        self.overlay = None;
        self.status = "Capture recording · Enter stops · Esc reviews discard".to_owned();
        Ok(())
    }

    pub fn stop_capture(&mut self) -> Result<(), CaptureError> {
        let phase = self
            .capture_session
            .phase()
            .ok_or(CaptureError::NoActiveCapture)?;
        if phase != CapturePhase::Recording {
            return Err(CaptureError::IllegalTransition {
                from: phase,
                to: CapturePhase::Finalizing,
            });
        }
        let source = self.capture_session.source().expect("active source");
        let token = self.capture_session.token().expect("active token");
        let audio = self.audio.as_mut().ok_or(CaptureError::AudioUnavailable)?;
        if let Err(failure) = audio.stop_capture(source, token) {
            let error = failure.error().clone();
            drop(failure.into_command());
            return Err(error);
        }
        self.overlay = Some(Overlay::CaptureProgress {
            action: self.pending_capture_project_action(),
            discarding: false,
        });
        self.status = "Stopping capture and waiting for exact callback ownership…".to_owned();
        Ok(())
    }

    pub fn cancel_capture(&mut self) -> Result<(), CaptureError> {
        let phase = self
            .capture_session
            .phase()
            .ok_or(CaptureError::NoActiveCapture)?;
        if matches!(phase, CapturePhase::Arming | CapturePhase::Recording)
            && let Some(audio) = self.audio.as_mut()
        {
            let source = self.capture_session.source().expect("active source");
            let token = self.capture_session.token().expect("active token");
            if let Err(failure) = audio.cancel_capture(source, token) {
                let error = failure.error().clone();
                drop(failure.into_command());
                return Err(error);
            }
            self.capture_discard_pending = true;
            self.overlay = Some(Overlay::CaptureProgress {
                action: self.pending_capture_project_action(),
                discarding: true,
            });
            self.status = "Discarding capture after callback ownership returns…".to_owned();
            return Ok(());
        }
        if self.capture_worker_request.is_some() {
            self.capture_discard_pending = true;
            self.overlay = Some(Overlay::CaptureProgress {
                action: self.pending_capture_project_action(),
                discarding: true,
            });
            self.status = "Discarding capture after exact worker ownership returns…".to_owned();
            return Ok(());
        }
        self.finish_capture_discard();
        Ok(())
    }

    pub fn retry_capture_finalization(&mut self) -> Result<(), CaptureError> {
        if self.capture_session.failure_is_retryable() && self.capture_source_pcm.is_none() {
            return Err(CaptureError::RetryCompletionMissing);
        }
        self.capture_session
            .retry_finalization_with_next_generation()?;
        debug_assert!(self.capture_worker_request.is_none());
        if let Some(candidate) = self.capture_ready.take() {
            self.queue_managed_release(candidate.id);
        }
        self.capture_session.set_managed_capture_id(None)?;
        self.overlay = Some(Overlay::CaptureProgress {
            action: self.pending_capture_project_action(),
            discarding: false,
        });
        self.status = "Retrying capture finalization…".to_owned();
        Ok(())
    }

    fn pending_capture_project_action(&self) -> Option<ProjectAction> {
        matches!(
            self.project_lifecycle_wait,
            Some(
                ProjectLifecycleWait::ChoosingCapture
                    | ProjectLifecycleWait::CaptureFinalize
                    | ProjectLifecycleWait::CaptureDiscard
            )
        )
        .then(|| {
            self.pending_project_action
                .as_ref()
                .map(PendingProjectAction::label)
        })
        .flatten()
    }

    pub fn maintain_capture(&mut self) -> bool {
        let changed = self.maintain_capture_once();
        if changed && self.capture_resolution_choice_pending() {
            self.present_capture_resolution_choice();
        }
        changed
    }

    fn maintain_capture_once(&mut self) -> bool {
        if let Some(result) = self.capture_worker_results.pop_front() {
            return self.apply_capture_worker_result(result);
        }
        if self.capture_discard_pending
            && matches!(
                self.capture_session.phase(),
                Some(
                    CapturePhase::Confirm
                        | CapturePhase::Finalizing
                        | CapturePhase::ReadyToInstall
                        | CapturePhase::Failed
                )
            )
            && self.capture_worker_request.is_none()
        {
            self.finish_capture_discard();
            return true;
        }
        if self.capture_ready.is_some()
            && self.capture_session.phase() == Some(CapturePhase::ReadyToInstall)
            && !self.capture_resolution_choice_pending()
        {
            return self.install_ready_capture();
        }
        if self.capture_session.phase() == Some(CapturePhase::Finalizing)
            && self.capture_worker_request.is_none()
        {
            return self.queue_capture_finalization();
        }
        if self.capture_session.phase() == Some(CapturePhase::Finalizing)
            && self.capture_worker_request.is_some()
        {
            if self.capture_discard_pending {
                if self.poll_capture_runtime_error_once() {
                    return true;
                }
                return self.queue_one_managed_release();
            }
            return self.poll_capture_runtime_error_once();
        }
        if self.capture_session.phase() == Some(CapturePhase::Recording) {
            return self.poll_capture_once();
        }
        self.queue_one_managed_release()
    }

    fn poll_capture_runtime_error_once(&mut self) -> bool {
        let source = self
            .capture_session
            .source()
            .expect("active capture source");
        let Some(error) = self
            .audio
            .as_mut()
            .and_then(|audio| audio.capture_runtime_error(source))
        else {
            return false;
        };
        let message = error.to_string();
        let _ = self
            .capture_session
            .mark_failed_with_cause(CaptureFailureCause::DeviceRuntime, message.clone());
        self.fail_audio(message);
        true
    }

    fn poll_capture_once(&mut self) -> bool {
        let source = self.capture_session.source().expect("recording source");
        let status_changed = self
            .audio
            .as_mut()
            .and_then(|audio| audio.capture_status(source))
            .filter(|status| {
                self.capture_session.token() == Some(status.token)
                    && self.capture_session.target() == Some(status.target)
                    && self.capture_session.source() == Some(status.source)
                    && self.capture_session.max_frames() == Some(status.max_frames)
                    && status.frames <= status.max_frames
                    && status.peak.is_finite()
            })
            .is_some_and(|status| {
                let changed = self.capture_status != Some(status);
                self.capture_status = Some(status);
                changed
            });
        let mut maintenance = match self.audio.as_mut() {
            Some(audio) => audio.poll_capture_maintenance(),
            None => return status_changed,
        };
        if let Some(error) = maintenance.runtime_error(source).cloned() {
            let message = error.to_string();
            let _ = self
                .capture_session
                .mark_failed_with_cause(CaptureFailureCause::DeviceRuntime, message.clone());
            self.fail_audio(message);
            return true;
        }
        let Some(outcome) = maintenance.take_completion(source) else {
            return status_changed;
        };
        match outcome {
            CaptureOutcome::Cancelled(buffer) => {
                let exact = self.capture_session.token() == Some(buffer.token())
                    && self.capture_session.target() == Some(buffer.target())
                    && self.capture_session.source() == Some(buffer.source())
                    && self.capture_session.source_rate() == Some(buffer.sample_rate());
                if exact {
                    self.finish_capture_discard();
                    true
                } else {
                    false
                }
            }
            CaptureOutcome::Completed(completion) => {
                if completion.stereo.is_empty() {
                    match self.capture_session.accept_completion(completion) {
                        Ok(()) => {
                            let _ = self
                                .capture_session
                                .mark_failed(CaptureError::EmptyCapture.to_string());
                            self.status = CaptureError::EmptyCapture.to_string();
                            self.overlay = Some(Overlay::CaptureFailed {
                                action: self.pending_capture_project_action(),
                            });
                            true
                        }
                        Err(_) => false,
                    }
                } else if self.capture_session.accept_completion(completion).is_ok() {
                    let completion = self
                        .capture_session
                        .completion()
                        .expect("accepted completion remains retained");
                    self.capture_hard_limit = completion.hard_limit;
                    self.capture_status = Some(CaptureStatus {
                        token: completion.token,
                        source: completion.source,
                        target: completion.target,
                        state: CaptureState::Idle,
                        frames: completion.stereo.len() / 2,
                        max_frames: self
                            .capture_session
                            .max_frames()
                            .expect("accepted capture frame limit"),
                        peak: completion.peak,
                        hard_limit: completion.hard_limit,
                    });
                    let stereo = self
                        .capture_session
                        .take_completion_stereo()
                        .expect("accepted completion owns stereo");
                    self.capture_source_pcm = Some(Arc::from(stereo.into_boxed_slice()));
                    debug_assert!(self.capture_worker_request.is_none());
                    self.overlay = Some(Overlay::CaptureProgress {
                        action: self.pending_capture_project_action(),
                        discarding: false,
                    });
                    self.status = "Finalizing capture on the worker…".to_owned();
                    true
                } else {
                    false
                }
            }
        }
    }

    fn queue_capture_finalization(&mut self) -> bool {
        if self.pending_worker_requests.len() >= WORKER_CHANNEL_CAPACITY {
            return false;
        }
        let Some(stereo) = self.capture_source_pcm.as_ref().map(Arc::clone) else {
            let _ = self
                .capture_session
                .mark_failed("capture source ownership is missing");
            self.overlay = Some(Overlay::CaptureFailed {
                action: self.pending_capture_project_action(),
            });
            return true;
        };
        let Some(engine_rate) = self.audio.as_ref().map(|audio| audio.sample_rate()) else {
            let _ = self.capture_session.mark_failed_with_cause(
                CaptureFailureCause::DeviceRuntime,
                CaptureError::AudioUnavailable.to_string(),
            );
            self.overlay = Some(Overlay::CaptureFailed {
                action: self.pending_capture_project_action(),
            });
            return true;
        };
        let request = FinalizeCaptureRequest {
            token: self.capture_session.token().expect("finalizing token"),
            generation: self
                .capture_session
                .generation()
                .expect("finalizing generation"),
            target: self.capture_session.target().expect("finalizing target"),
            source: self.capture_session.source().expect("finalizing source"),
            source_rate: self.capture_session.source_rate().expect("finalizing rate"),
            engine_rate,
            stereo,
            hard_limit: self.capture_hard_limit,
        };
        self.capture_worker_request = Some(request.clone());
        self.pending_worker_requests
            .push(WorkerRequest::FinalizeCapture(request));
        true
    }

    fn apply_capture_worker_result(&mut self, result: WorkerResult) -> bool {
        match result {
            WorkerResult::ManagedCaptureReleased { id, result } => {
                if self.managed_release_in_flight != Some(id) {
                    return false;
                }
                self.managed_release_in_flight = None;
                match result {
                    Ok(()) => {
                        if self.capture_discard_release_pending == Some(id) {
                            self.capture_discard_release_pending = None;
                            self.complete_capture_discard();
                        } else if self.capture_session.managed_capture_id() == Some(id) {
                            let _ = self.capture_session.set_managed_capture_id(None);
                        }
                        true
                    }
                    Err(error) => {
                        self.pending_managed_releases.push_front(id);
                        self.status = error.to_string();
                        true
                    }
                }
            }
            WorkerResult::CaptureFinalized {
                token,
                generation,
                target,
                source,
                source_rate,
                engine_rate,
                stereo,
                hard_limit,
                result,
            } => {
                let exact = self.capture_worker_request.as_ref().is_some_and(|request| {
                    request.token == token
                        && request.generation == generation
                        && request.target == target
                        && request.source == source
                        && request.source_rate == source_rate
                        && request.engine_rate == engine_rate
                        && request.hard_limit == hard_limit
                        && Arc::ptr_eq(&request.stereo, &stereo)
                });
                if !exact {
                    if let Ok(candidate) = result {
                        self.queue_managed_release(candidate.id);
                    }
                    return false;
                }
                self.capture_worker_request = None;
                if self.capture_discard_pending {
                    match result {
                        Ok(candidate) => {
                            self.wait_for_capture_discard_release(candidate.id);
                        }
                        Err(_) => self.finish_capture_discard(),
                    }
                    return true;
                }
                if self.capture_session.phase() == Some(CapturePhase::Failed) {
                    if let Ok(candidate) = result {
                        self.queue_managed_release(candidate.id);
                    }
                    self.restore_capture_presentation();
                    return true;
                }
                let current_rate = self.audio.as_ref().map(|audio| audio.sample_rate());
                if current_rate != Some(engine_rate) {
                    if let Ok(candidate) = result {
                        self.queue_managed_release(candidate.id);
                    }
                    if source == CaptureSource::Input && current_rate.is_some() {
                        match self.capture_session.advance_finalization_generation() {
                            Ok(_) => return true,
                            Err(error) => {
                                let _ = self.capture_session.mark_failed(error.to_string());
                                self.status = error.to_string();
                                self.overlay = Some(Overlay::CaptureFailed {
                                    action: self.pending_capture_project_action(),
                                });
                                return true;
                            }
                        }
                    }
                    let message = if source == CaptureSource::Resample {
                        format!(
                            "resampled output capture rate {source_rate} does not match engine rate {}",
                            current_rate.unwrap_or(0)
                        )
                    } else {
                        CaptureError::AudioUnavailable.to_string()
                    };
                    let cause = if current_rate.is_none() {
                        CaptureFailureCause::DeviceRuntime
                    } else {
                        CaptureFailureCause::InvalidCapture
                    };
                    let _ = self
                        .capture_session
                        .mark_failed_with_cause(cause, message.clone());
                    self.status = message;
                    self.overlay = Some(Overlay::CaptureFailed {
                        action: self.pending_capture_project_action(),
                    });
                    return true;
                }
                if source == CaptureSource::Resample && source_rate != engine_rate {
                    if let Ok(candidate) = result {
                        self.queue_managed_release(candidate.id);
                    }
                    let message = format!(
                        "resampled output capture rate {source_rate} does not match engine rate {engine_rate}"
                    );
                    let _ = self.capture_session.mark_failed(message.clone());
                    self.status = message;
                    self.overlay = Some(Overlay::CaptureFailed {
                        action: self.pending_capture_project_action(),
                    });
                    return true;
                }
                match result {
                    Err(error) => {
                        let message = error.to_string();
                        let _ = self.capture_session.mark_failed_with_cause(
                            CaptureFailureCause::WorkerFinalization,
                            message.clone(),
                        );
                        self.status = message;
                        self.overlay = Some(Overlay::CaptureFailed {
                            action: self.pending_capture_project_action(),
                        });
                        true
                    }
                    Ok(candidate)
                        if candidate.sample.rendered.sample_rate() == engine_rate
                            && candidate.sample.fingerprint == candidate.fingerprint
                            && candidate.path.extension().is_some_and(|ext| ext == "wav") =>
                    {
                        let id = candidate.id;
                        self.capture_ready = Some(candidate);
                        self.capture_session
                            .set_managed_capture_id(Some(id))
                            .expect("active capture accepts managed identity");
                        self.capture_session
                            .mark_ready_to_install()
                            .expect("exact finalization advances to ready");
                        self.overlay = Some(Overlay::CaptureProgress {
                            action: self.pending_capture_project_action(),
                            discarding: false,
                        });
                        self.status =
                            "Capture finalized; waiting for exact audio admission…".to_owned();
                        true
                    }
                    Ok(candidate) => {
                        self.queue_managed_release(candidate.id);
                        let message = "finalized capture tuple is inconsistent".to_owned();
                        let _ = self.capture_session.mark_failed(message.clone());
                        self.status = message;
                        self.overlay = Some(Overlay::CaptureFailed {
                            action: self.pending_capture_project_action(),
                        });
                        true
                    }
                }
            }
            _ => false,
        }
    }

    fn install_ready_capture(&mut self) -> bool {
        let Some(candidate) = self.capture_ready.take() else {
            return false;
        };
        if !self.capture_target_fence_matches() {
            self.queue_managed_release(candidate.id);
            self.mark_capture_target_changed();
            return true;
        }
        let source = self.capture_session.source().expect("ready source");
        let current_rate = self.audio.as_ref().map(|audio| audio.sample_rate());
        if current_rate != Some(candidate.sample.rendered.sample_rate()) {
            let id = candidate.id;
            self.queue_managed_release(id);
            let _ = self.capture_session.set_managed_capture_id(None);
            if source == CaptureSource::Input && current_rate.is_some() {
                if let Err(error) = self.capture_session.advance_finalization_generation() {
                    let _ = self.capture_session.mark_failed(error.to_string());
                    self.status = error.to_string();
                    self.overlay = Some(Overlay::CaptureFailed {
                        action: self.pending_capture_project_action(),
                    });
                }
            } else {
                let message = "resample capture cannot be rerendered at a different output rate";
                let _ = self.capture_session.mark_failed(message);
                self.status = message.to_owned();
                self.overlay = Some(Overlay::CaptureFailed {
                    action: self.pending_capture_project_action(),
                });
            }
            return true;
        }
        if self.ensure_project_mutation_available().is_err() {
            self.capture_ready = Some(candidate);
            self.status = CaptureError::ProjectRevisionExhausted.to_string();
            return true;
        }
        let target = self.capture_session.target().expect("ready target");
        let offset = pad_offset(target);
        let settings = self.pads[offset].settings;
        let install = self
            .audio
            .as_mut()
            .expect("matching ready rate requires audio")
            .install(
                target,
                Arc::clone(&candidate.sample.rendered),
                settings,
                self.pad_mixes[offset],
            );
        if let Err(error) = install {
            self.capture_ready = Some(candidate);
            self.status = error;
            return true;
        }

        let generation = self.capture_session.generation().expect("ready generation");
        self.retire_managed_capture_at(offset);
        let label = candidate
            .path
            .file_name()
            .unwrap_or(candidate.path.as_os_str())
            .to_string_lossy()
            .into_owned();
        self.pads[offset].source = Some(candidate.path);
        self.pads[offset].label = label;
        self.pads[offset].generation = generation;
        self.pads[offset].state = PadLoadState::Ready;
        self.pads[offset].sample = Some(candidate.sample.rendered);
        self.pads[offset].preview =
            crate::loader::downsample_preview(&candidate.sample.rendered_preview);
        self.sample_editor.commits[offset].base = Some(candidate.sample.base);
        self.sample_editor.commits[offset].source_generation = generation;
        self.sample_editor.commits[offset].fingerprint = Some(candidate.fingerprint);
        self.sample_editor.commits[offset].recipe = candidate.sample.recipe;
        self.sample_editor.commits[offset].base_preview = Some(candidate.sample.base_preview);
        self.sample_editor.commits[offset].rendered_preview =
            Some(candidate.sample.rendered_preview);
        self.sample_editor.commits[offset].managed_capture = Some(candidate.id);
        self.sample_editor.undo[offset] = None;
        self.current_session_bound[offset] = true;
        self.reinstall_pending[offset] = false;
        self.status = if self.capture_hard_limit {
            "Captured sample installed · MAX".to_owned()
        } else {
            "Captured sample installed".to_owned()
        };
        self.commit_project_mutation();
        let _ = self.capture_session.discard();
        self.clear_capture_transaction_fields();
        self.overlay = None;
        self.refresh_editor_for_offset(offset);
        if self.project_lifecycle_wait == Some(ProjectLifecycleWait::CaptureFinalize) {
            self.project_lifecycle_wait = None;
            self.advance_project_action();
        }
        true
    }

    fn clear_capture_transaction_fields(&mut self) {
        debug_assert!(self.capture_worker_request.is_none());
        self.capture_target_fence = None;
        self.capture_source_pcm = None;
        self.capture_hard_limit = false;
        self.capture_ready = None;
        self.capture_status = None;
        self.capture_discard_pending = false;
    }

    fn discard_capture_transaction(&mut self) {
        if let Some(candidate) = self.capture_ready.take() {
            self.queue_managed_release(candidate.id);
        }
        let _ = self.capture_session.discard();
        self.clear_capture_transaction_fields();
    }

    fn finish_capture_discard(&mut self) {
        debug_assert!(self.capture_discard_release_pending.is_none());
        if let Some(candidate) = self.capture_ready.take() {
            self.wait_for_capture_discard_release(candidate.id);
            return;
        }
        if let Some(id) = self.capture_session.managed_capture_id() {
            self.wait_for_capture_discard_release(id);
            return;
        }
        self.discard_capture_transaction();
        self.complete_capture_discard();
    }

    fn wait_for_capture_discard_release(&mut self, id: ManagedCaptureId) {
        debug_assert!(self.capture_discard_release_pending.is_none());
        self.discard_capture_transaction();
        self.capture_discard_release_pending = Some(id);
        self.queue_managed_release(id);
        self.overlay = Some(Overlay::CaptureProgress {
            action: self.pending_capture_project_action(),
            discarding: true,
        });
        self.status = "Discarding capture and releasing exact managed artifact…".to_owned();
    }

    fn complete_capture_discard(&mut self) {
        self.overlay = None;
        self.status = "Capture discarded; prior pad unchanged".to_owned();
        if self.project_lifecycle_wait == Some(ProjectLifecycleWait::CaptureDiscard) {
            self.project_lifecycle_wait = None;
            self.advance_project_action();
        }
    }

    fn queue_managed_release(&mut self, id: ManagedCaptureId) {
        if self.managed_release_in_flight == Some(id) || self.pending_managed_releases.contains(&id)
        {
            return;
        }
        self.pending_managed_releases.push_back(id);
    }

    fn retire_managed_capture_at(&mut self, offset: usize) {
        if let Some(id) = self.sample_editor.commits[offset].managed_capture.take() {
            self.queue_managed_release(id);
        }
    }

    fn queue_one_managed_release(&mut self) -> bool {
        if self.managed_release_in_flight.is_some()
            || self.pending_worker_requests.len() >= WORKER_CHANNEL_CAPACITY
        {
            return false;
        }
        let Some(id) = self.pending_managed_releases.pop_front() else {
            return false;
        };
        self.pending_worker_requests
            .push(WorkerRequest::ReleaseManagedCapture { id });
        self.managed_release_in_flight = Some(id);
        true
    }

    pub fn managed_release_in_flight(&self) -> Option<ManagedCaptureId> {
        self.managed_release_in_flight
    }

    pub fn remove_pad_sample(&mut self, pad: PadId) -> Result<(), String> {
        let offset = pad_offset(pad);
        if self.pads[offset].sample.is_none() {
            return Ok(());
        }
        self.ensure_project_mutation_available()?;
        self.audio
            .as_mut()
            .ok_or_else(|| "audio device is unavailable".to_owned())?
            .remove_sample(pad)?;
        self.retire_managed_capture_at(offset);
        let settings = self.pads[offset].settings;
        self.pads[offset] = PadView {
            settings,
            generation: self.pads[offset].generation.saturating_add(1),
            ..PadView::default()
        };
        self.sample_editor.commits[offset] = SampleCommit::default();
        self.invalidate_pending_edit(offset);
        self.sample_editor.undo[offset] = None;
        self.current_session_bound[offset] = false;
        self.reinstall_pending[offset] = false;
        self.commit_project_mutation();
        self.refresh_editor_for_offset(offset);
        Ok(())
    }

    pub fn apply(&mut self, action: InputAction) {
        if self.should_quit && !matches!(action, InputAction::StopAll | InputAction::PadRelease(_))
        {
            return;
        }
        if self.project_open_is_admitting()
            && !matches!(action, InputAction::StopAll | InputAction::PadRelease(_))
        {
            return;
        }
        match action {
            InputAction::PadPress(index) => self.press_pad(index),
            InputAction::PadRelease(index) => self.release_pad(index),
            InputAction::PadStop(index) => self.stop_pad(index),
            InputAction::BankDelta(delta) => self.change_bank(delta),
            InputAction::StopAll => self.stop_all(),
            InputAction::Quit => self.begin_project_action(PendingProjectAction::Quit),
        }
    }

    pub fn apply_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.apply_key(key),
            Event::Paste(text) if self.overlay == Some(Overlay::Palette) && !text.is_empty() => {
                self.palette.insert_str(&text);
                self.palette_error = None;
            }
            _ => {}
        }
    }

    fn blocking_pad_overlay(&self) -> bool {
        matches!(
            self.overlay,
            Some(
                Overlay::Palette
                    | Overlay::ProjectOpenProgress
                    | Overlay::ResolveSampleDraft { .. }
                    | Overlay::UnsavedProject { .. }
                    | Overlay::ProjectLifecycleProgress { .. }
                    | Overlay::ProjectSaveProgress
                    | Overlay::ProjectError { .. }
                    | Overlay::CaptureConfirm
                    | Overlay::CaptureDiscard
                    | Overlay::ResolveCapture { .. }
                    | Overlay::CaptureProgress { .. }
                    | Overlay::CaptureFailed { .. }
            )
        )
    }

    fn midi_trigger_is_fenced(&self) -> bool {
        self.should_quit || self.project_open_is_admitting() || self.blocking_pad_overlay()
    }

    pub const fn midi_settings(&self) -> MidiSettings {
        self.midi_settings
    }

    pub const fn midi_learn_target(&self) -> Option<PadId> {
        self.midi_learn_target
    }

    pub fn arm_midi_learn(&mut self) {
        self.midi_learn_target = self.selected_pad_id();
    }

    pub fn cancel_midi_learn(&mut self) {
        self.midi_learn_target = None;
    }

    pub fn maintain_midi_service(&mut self, now: Instant) -> bool {
        let result = self
            .midi_service
            .as_mut()
            .map(|service| service.maintain(now));
        match result {
            Some(Ok(Some(MidiServiceEvent::PortDisappeared(port)))) => {
                self.cancel_midi_learn();
                self.status = match self.release_all_midi_owners() {
                    Ok(()) => format!("MIDI port disappeared: {}", port.name()),
                    Err(error) => format!(
                        "MIDI port disappeared: {}; held-note release failed: {error}",
                        port.name()
                    ),
                };
                true
            }
            Some(Err(error)) => {
                self.status = error.to_string();
                true
            }
            _ => false,
        }
    }

    fn list_midi_ports(&mut self) -> Result<(), String> {
        let service = self
            .midi_service
            .as_mut()
            .ok_or("MIDI input service is unavailable")?;
        service.refresh_ports().map_err(|error| error.to_string())?;
        self.status = if service.ports().is_empty() {
            "MIDI ports: none".to_owned()
        } else {
            service
                .ports()
                .iter()
                .map(|port| format!("#{} {}", port.index(), port.name()))
                .collect::<Vec<_>>()
                .join(" · ")
        };
        Ok(())
    }

    fn connect_midi_port(&mut self, index: usize) -> Result<(), String> {
        let (prepared, replacing) = {
            let service = self
                .midi_service
                .as_mut()
                .ok_or("MIDI input service is unavailable")?;
            let replacing = service.connected_port().is_some();
            let prepared = service
                .prepare_connection(index)
                .map_err(|error| error.to_string())?;
            (prepared, replacing)
        };
        let needs_reconciliation = replacing || self.midi_owned_pads.iter().any(Option::is_some);
        if needs_reconciliation {
            self.release_all_midi_owners()?;
        }
        let service = self
            .midi_service
            .as_mut()
            .expect("the MIDI service that prepared a connection still exists");
        service.commit_connection(prepared);
        if needs_reconciliation {
            self.cancel_midi_learn();
        }
        self.status = format!("MIDI connected: #{index}");
        Ok(())
    }

    fn disconnect_midi_port(&mut self) -> Result<(), String> {
        self.release_all_midi_owners()?;
        self.cancel_midi_learn();
        if let Some(service) = self.midi_service.as_mut() {
            service.disconnect();
        }
        Ok(())
    }

    fn release_all_midi_owners(&mut self) -> Result<(), String> {
        self.release_midi_owners_where(|_, _| true)
    }

    pub fn update_midi_channel(&mut self, channel: MidiChannelFilter) -> Result<(), String> {
        self.update_midi_settings(self.midi_settings.with_channel(channel))
    }

    pub fn unmap_selected_midi(&mut self) -> Result<(), String> {
        let target = self
            .selected_pad_id()
            .expect("the selected pad is always in the active bank");
        let candidate = self
            .midi_settings
            .unmap(target.bank(), target.index())
            .map_err(|error| error.to_string())?;
        self.update_midi_settings(candidate)
    }

    pub fn reset_active_midi_bank(&mut self) -> Result<(), String> {
        self.update_midi_settings(self.midi_settings.reset_bank(self.active_bank))
    }

    fn learn_midi_note(&mut self, note: MidiNote) -> Result<(), String> {
        let Some(target) = self.midi_learn_target else {
            return Ok(());
        };
        let candidate = self
            .midi_settings
            .learn_swap(target.bank(), target.index(), note)
            .map_err(|error| error.to_string())?;
        self.update_midi_settings(candidate)?;
        self.midi_learn_target = None;
        Ok(())
    }

    fn update_midi_settings(&mut self, candidate: MidiSettings) -> Result<(), String> {
        if candidate == self.midi_settings {
            return Ok(());
        }
        self.ensure_project_mutation_available()?;
        self.release_midi_owners_where(|owner, owned| {
            let channel = sampler_core::MidiChannel::new((owner / MIDI_NOTE_COUNT + 1) as u8)
                .expect("MIDI ownership channel is bounded");
            let note = MidiNote::new((owner % MIDI_NOTE_COUNT) as u8)
                .expect("MIDI ownership note is bounded");
            let remains_mapped = candidate.channel().accepts(channel)
                && candidate.bank(owned.pad.bank()).owner(note) == Some(owned.pad.index());
            !remains_mapped
        })?;
        self.midi_settings = candidate;
        self.commit_project_mutation();
        Ok(())
    }

    fn release_midi_owners_where(
        &mut self,
        mut should_release: impl FnMut(usize, MidiOwnedVoice) -> bool,
    ) -> Result<(), String> {
        let releases = self
            .midi_owned_pads
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(owner, owned)| {
                owned
                    .filter(|owned| should_release(owner, *owned))
                    .map(|owned| (owner, owned))
            })
            .collect::<Vec<_>>();
        if releases.is_empty() {
            return Ok(());
        }
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return Err(self.status.clone());
        };
        let commands = audio
            .release_owned_live_batch(
                &releases
                    .iter()
                    .map(|(_, owned)| (owned.pad, owned.trigger_id))
                    .collect::<Vec<_>>(),
            )
            .inspect_err(|error| {
                self.status = error.clone();
            })?;
        assert_eq!(
            commands.len(),
            releases.len(),
            "audio release batch must return one command id per owner"
        );
        for ((owner, _), command) in releases.into_iter().zip(commands) {
            self.midi_owned_pads[owner] = None;
            if self.patterns.is_recording() {
                self.patterns
                    .note_live_release(MIDI_RECORDING_KEY_OFFSET + owner, command);
            }
        }
        Ok(())
    }

    pub fn apply_midi_event(&mut self, event: MidiEvent) {
        match event {
            MidiEvent::NoteOn {
                channel,
                note,
                velocity: 0,
            }
            | MidiEvent::NoteOff { channel, note } => {
                let owner = midi_owner_index(channel.get(), note.get());
                let _ = self.release_midi_owner(owner);
            }
            MidiEvent::NoteOn {
                channel,
                note,
                velocity,
            } => {
                if self.midi_trigger_is_fenced() || !self.midi_settings.channel().accepts(channel) {
                    return;
                }
                if let Err(error) = self.learn_midi_note(note) {
                    self.status = error;
                }
                let Some(pad_index) = self.midi_settings.bank(self.active_bank).owner(note) else {
                    return;
                };
                let owner = midi_owner_index(channel.get(), note.get());
                if self.midi_owned_pads[owner].is_some() && !self.release_midi_owner(owner) {
                    return;
                }
                let pad = PadId::new(self.active_bank, pad_index)
                    .expect("MIDI map contains a valid pad index");
                let _ = self.select_pad(usize::from(pad_index));
                if self.patterns.view() == WorkspaceView::Pattern {
                    let step = self.patterns.cursor().step();
                    self.patterns.move_cursor_to(pad, step);
                }
                let Some(audio) = self.audio.as_mut() else {
                    self.report_audio_unavailable();
                    return;
                };
                let velocity = f32::from(velocity) / 127.0;
                let result = audio.trigger_live_tracked(pad, velocity);
                match result {
                    Ok(command) => {
                        self.midi_owned_pads[owner] = Some(MidiOwnedVoice {
                            pad,
                            trigger_id: command,
                        });
                        if self.patterns.is_recording() {
                            let records_duration =
                                self.pads[pad_offset(pad)].settings.mode != PlaybackMode::OneShot;
                            self.patterns.note_live_trigger_with_duration(
                                MIDI_RECORDING_KEY_OFFSET + owner,
                                command,
                                pad,
                                velocity,
                                records_duration,
                            );
                        }
                    }
                    Err(error) => self.status = error,
                }
            }
        }
    }

    pub fn apply_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Repeat {
            return;
        }
        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Esc
            && key.modifiers == KeyModifiers::NONE
        {
            self.cancel_midi_learn();
        }
        if self.should_quit {
            if let Some(action @ (InputAction::StopAll | InputAction::PadRelease(_))) =
                map_key(key, self.keyboard_capabilities)
            {
                self.apply(action);
            }
            return;
        }
        if matches!(self.overlay, Some(Overlay::DeviceError(_)))
            && key.kind == KeyEventKind::Press
            && key.modifiers == KeyModifiers::NONE
            && matches!(key.code, KeyCode::Char('r' | 'R'))
        {
            self.device_retry_requests = self.device_retry_requests.saturating_add(1);
            return;
        }
        if let Some(action) = map_key(key, self.keyboard_capabilities) {
            let captures_pad_keys = self.blocking_pad_overlay();
            match action {
                InputAction::Quit | InputAction::StopAll | InputAction::PadRelease(_) => {
                    self.apply(action);
                    return;
                }
                InputAction::PadPress(_) | InputAction::PadStop(_) if captures_pad_keys => {}
                InputAction::PadPress(_) | InputAction::PadStop(_) => {
                    self.apply(action);
                    return;
                }
                InputAction::BankDelta(_) => {}
            }
        }
        if self.audio.is_none() && is_explicit_device_retry(key) {
            self.device_retry_requests = self.device_retry_requests.saturating_add(1);
            return;
        }

        if self.apply_capture_key(key) {
            return;
        }

        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Esc
            && key.modifiers == KeyModifiers::NONE
            && self.overlay.is_some()
        {
            self.cancel_overlay();
            return;
        }

        match self.overlay.as_ref() {
            Some(Overlay::DeviceError(_)) => self.apply_device_error_key(key),
            Some(Overlay::ProjectOpenProgress) => self.apply_project_open_key(key),
            Some(Overlay::ResolveSampleDraft { .. }) => self.apply_sample_resolution_key(key),
            Some(Overlay::UnsavedProject { .. }) => self.apply_unsaved_project_key(key),
            Some(Overlay::ProjectLifecycleProgress { .. }) => {}
            Some(Overlay::ProjectSaveProgress) => {}
            Some(Overlay::ProjectError { .. }) => self.apply_project_error_key(key),
            Some(Overlay::Palette) => self.apply_palette_key(key),
            Some(Overlay::FilePicker) => self.apply_picker_key(key),
            Some(Overlay::Help) => self.apply_help_key(key),
            Some(Overlay::ClearPattern { .. }) => self.apply_clear_pattern_key(key),
            Some(Overlay::ApplySample { .. }) => self.apply_sample_apply_key(key),
            Some(Overlay::DiscardSample { .. }) => self.apply_sample_discard_key(key),
            Some(
                Overlay::CaptureConfirm
                | Overlay::CaptureDiscard
                | Overlay::ResolveCapture { .. }
                | Overlay::CaptureProgress { .. }
                | Overlay::CaptureFailed { .. },
            ) => {}
            None => self.apply_workspace_key(key),
        }
    }

    pub fn active_bank(&self) -> BankId {
        self.active_bank
    }

    pub fn selected_pad(&self) -> usize {
        self.selected_pad
    }

    pub fn pads(&self) -> &[PadView; PAD_VIEW_COUNT] {
        &self.pads
    }

    pub fn audio_format(&self) -> Option<(u32, u16)> {
        self.audio_format
    }

    pub fn is_pad_held(&self, index: usize) -> bool {
        let Some(held) = self.held_pad_by_key.get(index).copied().flatten() else {
            return false;
        };
        held.bank() == self.active_bank && usize::from(held.index()) == index
    }

    pub fn release_events_available(&self) -> bool {
        self.keyboard_capabilities.release_events
    }

    pub fn telemetry(&self) -> Telemetry {
        self.telemetry
    }

    pub fn patterns(&self) -> &PatternWorkspace {
        &self.patterns
    }

    pub fn workspace_view(&self) -> WorkspaceView {
        self.patterns.view()
    }

    pub fn sample_editor(&self) -> &SampleEditor {
        &self.editor
    }

    pub fn recorded_ack_count(&self) -> usize {
        self.recorded_ack_count
    }

    pub fn maintain_audio_pattern_submissions(&self) -> usize {
        self.pattern_submission_count
    }

    pub fn meter_levels(&self) -> (f32, f32) {
        (self.meter_left, self.meter_right)
    }

    pub fn project_revision(&self) -> u64 {
        self.project_session.current_revision()
    }

    pub fn request_save(&mut self) -> Result<(), ProjectSaveError> {
        let Some(directory) = self.project_session.directory().map(Path::to_owned) else {
            return Err(ProjectSaveError::Untitled);
        };
        self.ensure_project_request_available()?;
        let snapshot = self
            .project_snapshot()
            .map_err(ProjectSaveError::Snapshot)?;
        let token = self.allocate_project_token()?;
        self.pending_explicit_save = Some(PendingProjectSave {
            descriptor: crate::ProjectOperationDescriptor {
                token,
                kind: SaveKind::Explicit,
                project_id: snapshot.project_id,
                directory,
                revision: snapshot.revision,
            },
            snapshot,
            save_as: false,
        });
        self.cancel_midi_learn();
        Ok(())
    }

    pub fn request_save_as(
        &mut self,
        directory: impl Into<PathBuf>,
    ) -> Result<(), ProjectSaveError> {
        self.ensure_project_request_available()?;
        let directory = directory.into();
        let (project_id, name) = match &self.save_as_identity {
            Some((previous, project_id, name)) if previous == &directory => {
                (*project_id, name.clone())
            }
            _ => {
                let mut bytes = [0_u8; 16];
                getrandom::fill(&mut bytes)
                    .map_err(|error| ProjectSaveError::Entropy(error.to_string()))?;
                if bytes == [0; 16] {
                    bytes[15] = 1;
                }
                let name = directory
                    .file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or("Untitled")
                    .to_owned();
                let project_id = ProjectId::from_bytes(bytes);
                self.save_as_identity = Some((directory.clone(), project_id, name.clone()));
                (project_id, name)
            }
        };
        let mut snapshot = self
            .project_snapshot()
            .map_err(ProjectSaveError::Snapshot)?;
        snapshot.project_id = project_id;
        snapshot.name = name;
        let token = self.allocate_project_token()?;
        self.pending_explicit_save = Some(PendingProjectSave {
            descriptor: crate::ProjectOperationDescriptor {
                token,
                kind: SaveKind::Explicit,
                project_id,
                directory,
                revision: snapshot.revision,
            },
            snapshot,
            save_as: true,
        });
        self.cancel_midi_learn();
        Ok(())
    }

    pub fn maintain_project(&mut self, now: Instant) -> bool {
        if self.project_open.is_some() {
            return self.maintain_project_open(now);
        }
        let mut changed = false;
        if self.autosave_retry_clock_pending {
            self.autosave_retry_clock_pending = false;
            self.autosave_retry_since = Some(now);
            changed = true;
        }
        if self.pending_autosave_save.as_ref().is_some_and(|pending| {
            pending.descriptor.revision < self.project_session.current_revision()
        }) {
            self.pending_autosave_save = None;
            self.project_session.set_pending_autosave(None);
            changed = true;
        }

        if self.in_flight_project.is_none()
            && self.pending_project_action.is_none()
            && self.pending_explicit_save.is_none()
            && self.pending_autosave_save.as_ref().is_none_or(|pending| {
                pending.descriptor.revision < self.project_session.current_revision()
            })
            && self.project_session.directory().is_some()
            && self.project_session.current_revision() > self.project_session.autosaved_revision()
            && self.project_session.current_revision() > self.project_session.saved_revision()
        {
            let quiet_since = match (
                self.autosave_retry_since,
                self.project_session.dirty_since(),
            ) {
                (Some(retry), Some(dirty)) => Some(retry.max(dirty)),
                (retry, dirty) => retry.or(dirty),
            };
            if quiet_since
                .is_some_and(|since| now.saturating_duration_since(since) >= AUTOSAVE_DEBOUNCE)
                && let Ok(snapshot) = self.project_snapshot()
                && let Ok(token) = self.allocate_project_token()
            {
                let descriptor = crate::ProjectOperationDescriptor {
                    token,
                    kind: SaveKind::Recovery,
                    project_id: snapshot.project_id,
                    directory: self
                        .project_session
                        .directory()
                        .expect("named project checked above")
                        .to_owned(),
                    revision: snapshot.revision,
                };
                self.project_session
                    .set_pending_autosave(Some(crate::AutosaveDescriptor {
                        revision: descriptor.revision,
                    }));
                self.pending_autosave_save = Some(PendingProjectSave {
                    descriptor,
                    snapshot,
                    save_as: false,
                });
                changed = true;
            }
        }

        if self.in_flight_project.is_some()
            || self.pending_worker_requests.len() >= WORKER_CHANNEL_CAPACITY
        {
            return changed;
        }

        if let Some(save) = self.pending_explicit_save.take() {
            self.enqueue_project_save(save);
            return true;
        }
        if let Some(cleanup) = self.pending_recovery_cleanup.pop_front() {
            self.pending_worker_requests
                .push(WorkerRequest::DiscardRecovery {
                    token: cleanup.token,
                    directory: cleanup.directory.clone(),
                    project_id: cleanup.project_id,
                    revision: cleanup.revision,
                });
            self.in_flight_project = Some(InFlightProjectOperation::Cleanup(cleanup));
            return true;
        }
        if let Some(save) = self.pending_autosave_save.take() {
            self.project_session.set_pending_autosave(None);
            self.enqueue_project_save(save);
            return true;
        }
        changed
    }

    pub fn request_open_project(
        &mut self,
        directory: impl Into<PathBuf>,
    ) -> Result<ProjectToken, ProjectOpenError> {
        if self.project_open.is_some()
            || self.in_flight_project.is_some()
            || self.pending_explicit_save.is_some()
            || self.pending_autosave_save.is_some()
        {
            return Err(ProjectOpenError::OperationPending);
        }
        self.project_snapshot()
            .map_err(|error| ProjectOpenError::UnresolvedState(error.to_string()))?;
        let token = self
            .allocate_project_token()
            .map_err(|_| ProjectOpenError::TokenExhausted)?;
        self.cancel_midi_learn();
        let directory = directory.into();
        self.project_open_error = None;
        let progress = ProjectOpenStage {
            token,
            directory: directory.clone(),
            project_id: None,
            revision: None,
            phase: ProjectOpenPhase::Probing,
            staged_pads: 0,
            total_pads: 0,
            admitted_actions: 0,
            total_actions: 0,
        };
        let worker_queued =
            self.queue_worker_request(WorkerRequest::ProbeProject { token, directory });
        self.project_open = Some(ProjectOpenOperation::Probing {
            progress,
            worker_queued,
        });
        self.overlay = Some(Overlay::ProjectOpenProgress);
        self.status = "Validating project metadata…".to_owned();
        Ok(token)
    }

    pub fn request_open_project_interactive(&mut self, directory: impl Into<PathBuf>) {
        self.begin_project_action(PendingProjectAction::Open(directory.into()));
    }

    fn begin_project_action(&mut self, action: PendingProjectAction) {
        if self.pending_project_action.is_some() || self.project_open.is_some() {
            self.status = "a project lifecycle operation is already pending".to_owned();
            return;
        }
        self.cancel_midi_learn();
        self.pending_project_action = Some(action);
        self.advance_project_action();
    }

    fn advance_project_action(&mut self) {
        let Some(action) = self.pending_project_action.as_ref() else {
            return;
        };
        let label = action.label();
        if self.capture_discard_release_pending.is_some() {
            self.project_lifecycle_wait = Some(ProjectLifecycleWait::CaptureDiscard);
            self.overlay = Some(Overlay::CaptureProgress {
                action: Some(label),
                discarding: true,
            });
            self.status = "Discarding capture and releasing exact managed artifact…".to_owned();
            return;
        }
        if self.capture_session.phase().is_some() {
            self.project_lifecycle_wait = Some(ProjectLifecycleWait::ChoosingCapture);
            self.present_capture_resolution_choice();
            return;
        }
        if self.editor.is_dirty() {
            self.project_lifecycle_wait = None;
            self.overlay = Some(Overlay::ResolveSampleDraft {
                pad: self.editor.pad(),
                action: label,
            });
            self.status = "Resolve the un-applied sample draft first".to_owned();
            return;
        }
        if self.project_session.current_revision() != self.project_session.saved_revision() {
            self.project_lifecycle_wait = Some(ProjectLifecycleWait::ChoosingProject);
            self.overlay = Some(Overlay::UnsavedProject { action: label });
            self.status = "Current project has unsaved changes".to_owned();
            return;
        }
        self.complete_project_action();
    }

    fn complete_project_action(&mut self) {
        let Some(action) = self.pending_project_action.take() else {
            return;
        };
        self.project_lifecycle_wait = None;
        match action {
            PendingProjectAction::Quit => {
                self.overlay = None;
                self.should_quit = true;
            }
            PendingProjectAction::Open(directory) => {
                if let Err(error) = self.request_open_project(directory) {
                    let message = error.to_string();
                    self.status = message.clone();
                    self.overlay = Some(Overlay::ProjectError {
                        title: "OPEN PROJECT ERROR".to_owned(),
                        message,
                    });
                }
            }
        }
    }

    fn cancel_project_action(&mut self) {
        self.pending_project_action = None;
        self.project_lifecycle_wait = None;
        self.overlay = None;
        self.status = "Project action cancelled".to_owned();
    }

    fn cancel_project_action_preserving_capture(&mut self) {
        self.pending_project_action = None;
        self.project_lifecycle_wait = None;
        self.status = "Project action cancelled; capture preserved".to_owned();
        self.restore_capture_presentation();
    }

    fn capture_resolution_choice_pending(&self) -> bool {
        self.project_lifecycle_wait == Some(ProjectLifecycleWait::ChoosingCapture)
            && self.pending_project_action.is_some()
            && self.capture_session.phase().is_some()
    }

    fn present_capture_resolution_choice(&mut self) {
        let Some(action) = self
            .pending_project_action
            .as_ref()
            .map(PendingProjectAction::label)
        else {
            return;
        };
        self.overlay = Some(Overlay::ResolveCapture { action });
        self.status = "Resolve the active capture before the project action".to_owned();
    }

    fn restore_capture_presentation(&mut self) {
        if self.capture_resolution_choice_pending() {
            self.present_capture_resolution_choice();
            return;
        }
        let action = self.pending_capture_project_action();
        self.overlay = match self.capture_session.phase() {
            Some(CapturePhase::Confirm) => Some(Overlay::CaptureConfirm),
            Some(CapturePhase::Recording) => None,
            Some(CapturePhase::Finalizing | CapturePhase::ReadyToInstall) => {
                Some(Overlay::CaptureProgress {
                    action,
                    discarding: self.capture_discard_pending,
                })
            }
            Some(CapturePhase::Failed) => Some(Overlay::CaptureFailed { action }),
            Some(CapturePhase::Arming) => Some(Overlay::CaptureProgress {
                action,
                discarding: self.capture_discard_pending,
            }),
            None => None,
        };
    }

    fn apply_capture_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press || key.modifiers != KeyModifiers::NONE {
            return false;
        }
        match self.overlay.clone() {
            Some(Overlay::CaptureConfirm) => {
                match key.code {
                    KeyCode::Enter => {
                        if let Err(error) = self.confirm_capture() {
                            self.status = error.to_string();
                            self.overlay = Some(Overlay::CaptureFailed { action: None });
                        }
                    }
                    KeyCode::Esc => self.finish_capture_discard(),
                    _ => {}
                }
                true
            }
            Some(Overlay::CaptureDiscard) => {
                match key.code {
                    KeyCode::Enter => {
                        if let Err(error) = self.cancel_capture() {
                            self.status = error.to_string();
                        }
                    }
                    KeyCode::Esc => {
                        self.overlay = None;
                        self.status =
                            "Capture preserved · Enter stops · Esc reviews discard".to_owned();
                    }
                    _ => {}
                }
                true
            }
            Some(Overlay::ResolveCapture { .. }) => {
                match key.code {
                    KeyCode::Enter => self.finalize_capture_for_project_action(),
                    KeyCode::Backspace => {
                        self.project_lifecycle_wait = Some(ProjectLifecycleWait::CaptureDiscard);
                        if let Err(error) = self.cancel_capture() {
                            self.status = error.to_string();
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('c' | 'C') => {
                        self.cancel_project_action_preserving_capture()
                    }
                    _ => {}
                }
                true
            }
            Some(Overlay::CaptureProgress { .. }) => true,
            Some(Overlay::CaptureFailed { action }) => {
                match key.code {
                    KeyCode::Char('r' | 'R') | KeyCode::Enter
                        if self.capture_session.failure_is_retryable() =>
                    {
                        if action.is_some() {
                            self.project_lifecycle_wait =
                                Some(ProjectLifecycleWait::CaptureFinalize);
                        }
                        if let Err(error) = self.retry_capture_finalization() {
                            self.status = error.to_string();
                        }
                    }
                    KeyCode::Char('d' | 'D') | KeyCode::Backspace => {
                        if action.is_some() {
                            self.project_lifecycle_wait =
                                Some(ProjectLifecycleWait::CaptureDiscard);
                        }
                        if let Err(error) = self.cancel_capture() {
                            self.status = error.to_string();
                        }
                    }
                    KeyCode::Esc | KeyCode::Char('c' | 'C') if action.is_some() => {
                        self.cancel_project_action_preserving_capture()
                    }
                    KeyCode::Char('c' | 'C') => {
                        if let Err(error) = self.cancel_capture() {
                            self.status = error.to_string();
                        }
                    }
                    _ => {
                        if key.code == KeyCode::Enter {
                            self.status = self.capture_session.failure_cause().map_or_else(
                                || "Capture finalization cannot be retried".to_owned(),
                                |cause| format!("Capture failure {cause:?} requires a fresh take"),
                            );
                        }
                    }
                }
                true
            }
            Some(_) => false,
            None if self.capture_session.phase() == Some(CapturePhase::Recording) => {
                match key.code {
                    KeyCode::Enter => {
                        if let Err(error) = self.stop_capture() {
                            self.status = error.to_string();
                        }
                    }
                    KeyCode::Esc => {
                        self.overlay = Some(Overlay::CaptureDiscard);
                        self.status = "Confirm whether to discard the active capture".to_owned();
                    }
                    _ => return false,
                }
                true
            }
            None => false,
        }
    }

    fn finalize_capture_for_project_action(&mut self) {
        self.project_lifecycle_wait = Some(ProjectLifecycleWait::CaptureFinalize);
        let result = match self.capture_session.phase() {
            Some(CapturePhase::Confirm) => self.confirm_capture(),
            Some(CapturePhase::Recording) => self.stop_capture(),
            Some(CapturePhase::Finalizing | CapturePhase::ReadyToInstall) => {
                self.restore_capture_presentation();
                Ok(())
            }
            Some(CapturePhase::Failed) => self.retry_capture_finalization(),
            Some(CapturePhase::Arming) => Ok(()),
            None => Err(CaptureError::NoActiveCapture),
        };
        if let Err(error) = result {
            self.project_lifecycle_wait = Some(ProjectLifecycleWait::ChoosingCapture);
            self.status = error.to_string();
            self.overlay = Some(Overlay::CaptureFailed {
                action: self
                    .pending_project_action
                    .as_ref()
                    .map(PendingProjectAction::label),
            });
        }
    }

    fn reconfirm_project_action(&mut self, status: &str) {
        self.project_lifecycle_wait = Some(ProjectLifecycleWait::ChoosingProject);
        let action = self
            .pending_project_action
            .as_ref()
            .map_or(ProjectAction::Quit, PendingProjectAction::label);
        self.overlay = Some(Overlay::UnsavedProject { action });
        self.status = status.to_owned();
    }

    fn apply_sample_resolution_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Enter => {
                let Some(SampleEditorIntent::Apply { pad, recipe }) = self.editor.request_apply()
                else {
                    self.status = "Sample draft cannot be applied right now".to_owned();
                    return;
                };
                match self.request_sample_edit(pad, recipe) {
                    Ok(()) => {
                        self.editor.observe_pending();
                        self.project_lifecycle_wait = Some(ProjectLifecycleWait::SampleApply);
                        let action = self
                            .pending_project_action
                            .as_ref()
                            .map_or(ProjectAction::Quit, PendingProjectAction::label);
                        self.overlay = Some(Overlay::ProjectLifecycleProgress { action });
                        self.status = "Applying sample draft before project action…".to_owned();
                    }
                    Err(error) => {
                        self.editor.cancel_confirmation();
                        self.status = error.to_string();
                    }
                }
            }
            KeyCode::Backspace => {
                self.editor.confirm_discard();
                self.sync_editor_to_selected_pad();
                self.advance_project_action();
            }
            _ => {}
        }
    }

    fn apply_unsaved_project_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Char('y' | 'Y') => self.save_before_project_action(),
            KeyCode::Char('n' | 'N') => self.discard_before_project_action(),
            _ => {}
        }
    }

    fn fail_project_sample_apply(&mut self, pad: PadId) {
        if self.project_lifecycle_wait != Some(ProjectLifecycleWait::SampleApply) {
            return;
        }
        self.project_lifecycle_wait = None;
        let Some(action) = self
            .pending_project_action
            .as_ref()
            .map(PendingProjectAction::label)
        else {
            return;
        };
        self.overlay = Some(Overlay::ResolveSampleDraft { pad, action });
    }

    fn save_before_project_action(&mut self) {
        match self.request_save() {
            Ok(()) => {
                let token = self
                    .pending_explicit_save
                    .as_ref()
                    .map(|save| save.descriptor.token)
                    .expect("accepted explicit save owns a token");
                self.project_lifecycle_wait = Some(ProjectLifecycleWait::Saving {
                    token,
                    action_revision: self.project_session.current_revision(),
                });
                let action = self
                    .pending_project_action
                    .as_ref()
                    .map_or(ProjectAction::Quit, PendingProjectAction::label);
                self.overlay = Some(Overlay::ProjectLifecycleProgress { action });
                self.status = "Saving project before continuing…".to_owned();
            }
            Err(ProjectSaveError::Untitled) => {
                self.status =
                    "Untitled project: use save-as <directory> before continuing".to_owned();
            }
            Err(error) => self.status = error.to_string(),
        }
    }

    fn discard_before_project_action(&mut self) {
        let recovery_revision = self.project_session.autosaved_revision();
        if recovery_revision <= self.project_session.saved_revision() {
            self.complete_project_action();
            return;
        }
        let Some(directory) = self.project_session.directory().map(Path::to_owned) else {
            self.complete_project_action();
            return;
        };
        if self.pending_recovery_cleanup.len() >= WORKER_CHANNEL_CAPACITY
            || self.in_flight_project.is_some()
        {
            self.status = "project worker is busy; recovery discard is still pending".to_owned();
            return;
        }
        let token = match self.allocate_project_token() {
            Ok(token) => token,
            Err(error) => {
                self.status = error.to_string();
                return;
            }
        };
        let cleanup = RecoveryCleanup {
            token,
            directory,
            project_id: self.project_session.project_id(),
            revision: recovery_revision,
        };
        self.pending_recovery_cleanup.push_back(cleanup.clone());
        self.project_lifecycle_wait = Some(ProjectLifecycleWait::DiscardingRecovery {
            cleanup,
            action_revision: self.project_session.current_revision(),
        });
        let action = self
            .pending_project_action
            .as_ref()
            .map_or(ProjectAction::Quit, PendingProjectAction::label);
        self.overlay = Some(Overlay::ProjectLifecycleProgress { action });
        self.status = "Discarding newer recovery before continuing…".to_owned();
    }

    pub fn project_open_stage(&self) -> Option<&ProjectOpenStage> {
        match self.project_open.as_ref()? {
            ProjectOpenOperation::Probing { progress, .. } => Some(progress),
            ProjectOpenOperation::ChoosingRecovery(choice) => Some(&choice.progress),
            ProjectOpenOperation::Staging(candidate) => Some(&candidate.progress),
        }
    }

    pub fn project_open_error(&self) -> Option<&ProjectOpenError> {
        self.project_open_error.as_ref()
    }

    fn fail_project_open(&mut self, error: ProjectOpenError) {
        self.project_open = None;
        let message = error.to_string();
        self.overlay = Some(Overlay::ProjectError {
            title: "OPEN PROJECT ERROR".to_owned(),
            message: message.clone(),
        });
        self.status = message;
        self.project_open_error = Some(error);
    }

    pub fn cancel_project_open(&mut self) -> Result<(), ProjectOpenError> {
        let Some(operation) = self.project_open.as_ref() else {
            return Err(ProjectOpenError::OperationPending);
        };
        if matches!(operation, ProjectOpenOperation::Staging(candidate) if candidate.progress.phase == ProjectOpenPhase::Admitting)
            || matches!(operation, ProjectOpenOperation::ChoosingRecovery(choice) if choice.discard_queued)
        {
            return Err(ProjectOpenError::CancellationLocked);
        }
        self.project_open = None;
        self.project_open_error = None;
        if self.overlay == Some(Overlay::ProjectOpenProgress) {
            self.overlay = None;
        }
        self.status = "Project open cancelled".to_owned();
        Ok(())
    }

    fn maintain_project_open(&mut self, now: Instant) -> bool {
        let Some(mut operation) = self.project_open.take() else {
            return false;
        };
        let mut changed = false;
        if let ProjectOpenOperation::Staging(candidate) = &mut operation
            && candidate.decode_in_flight.is_none()
            && candidate.next_decode == candidate.document.pads.len()
            && matches!(candidate.admission, ProjectAdmission::MidiOwners)
        {
            match self.release_all_midi_owners() {
                Ok(()) => {
                    self.cancel_midi_learn();
                    candidate.progress.admitted_actions += 1;
                    candidate.admission = ProjectAdmission::StopAll;
                    changed = true;
                }
                Err(error) => self.status = error,
            }
        }
        match &mut operation {
            ProjectOpenOperation::Probing {
                progress,
                worker_queued,
            } => {
                if !*worker_queued && self.pending_worker_requests.len() < WORKER_CHANNEL_CAPACITY {
                    self.pending_worker_requests
                        .push(WorkerRequest::ProbeProject {
                            token: progress.token,
                            directory: progress.directory.clone(),
                        });
                    *worker_queued = true;
                    changed = true;
                }
            }
            ProjectOpenOperation::ChoosingRecovery(choice) => {
                if choice.discard_requested
                    && !choice.discard_queued
                    && self.pending_worker_requests.len() < WORKER_CHANNEL_CAPACITY
                {
                    self.pending_worker_requests
                        .push(WorkerRequest::DiscardRecovery {
                            token: choice.progress.token,
                            directory: choice.progress.directory.clone(),
                            project_id: choice.recovery.project_id,
                            revision: choice.recovery.revision,
                        });
                    choice.discard_queued = true;
                    changed = true;
                }
            }
            ProjectOpenOperation::Staging(candidate) => {
                if candidate.decode_in_flight.is_none()
                    && candidate.next_decode < candidate.document.pads.len()
                    && self.pending_worker_requests.len() < WORKER_CHANNEL_CAPACITY
                {
                    let Some((engine_rate, _)) = self.audio_format else {
                        self.project_open = Some(operation);
                        return false;
                    };
                    let pad = &candidate.document.pads[candidate.next_decode];
                    let path = candidate.progress.directory.join(&pad.audio_path);
                    self.pending_worker_requests
                        .push(WorkerRequest::StageProjectSample(Box::new(
                            StageProjectSampleRequest {
                                token: candidate.progress.token,
                                generation: candidate.stage_generation,
                                pad: pad.pad,
                                revision: candidate.document.revision,
                                path,
                                project_directory: candidate.progress.directory.clone(),
                                asset_path: pad.audio_path.clone(),
                                expected_digest: pad.asset_digest,
                                engine_rate,
                                recipe: pad.recipe,
                            },
                        )));
                    candidate.decode_in_flight = Some((pad.pad, candidate.stage_generation));
                    changed = true;
                } else if candidate.decode_in_flight.is_none()
                    && candidate.next_decode == candidate.document.pads.len()
                {
                    let Some(audio) = self.audio.as_mut() else {
                        self.project_open = Some(operation);
                        return false;
                    };
                    match candidate.admission {
                        ProjectAdmission::StopAll => match audio.stop_all() {
                            Ok(()) => {
                                candidate.progress.phase = ProjectOpenPhase::Admitting;
                                candidate.progress.admitted_actions += 1;
                                candidate.admission = ProjectAdmission::Master;
                                self.held_pad_by_key.fill(None);
                                changed = true;
                            }
                            Err(error) => self.status = error,
                        },
                        ProjectAdmission::Master => {
                            match audio.update_master_mix(candidate.document.master_mix) {
                                Ok(()) => {
                                    candidate.progress.admitted_actions += 1;
                                    candidate.admission = ProjectAdmission::Pads(0);
                                    changed = true;
                                }
                                Err(error) => self.status = error,
                            }
                        }
                        ProjectAdmission::Pads(offset) => {
                            let pad = pad_from_offset(offset);
                            let result =
                                if let Some(staged) = candidate.staged_pads[offset].as_ref() {
                                    audio
                                        .install(
                                            pad,
                                            Arc::clone(&staged.loaded.rendered),
                                            staged.settings,
                                            staged.mix,
                                        )
                                        .map(|_| ())
                                } else {
                                    audio.remove_sample(pad)
                                };
                            match result {
                                Ok(()) => {
                                    let next = offset + 1;
                                    candidate.progress.admitted_actions += 1;
                                    candidate.admission = if next == PAD_VIEW_COUNT {
                                        ProjectAdmission::Patterns(0)
                                    } else {
                                        ProjectAdmission::Pads(next)
                                    };
                                    changed = true;
                                }
                                Err(error) => {
                                    if let Err(rollback_error) =
                                        audio.update_master_mix(self.master_mix)
                                    {
                                        candidate.admission =
                                            ProjectAdmission::RestoreCommittedMaster {
                                                end_pad: offset,
                                            };
                                        self.status = format!(
                                            "Project admission failed ({error}); audio rollback failed: restore master mixer: {rollback_error}"
                                        );
                                    } else if let Err((next_pad, rollback_error)) =
                                        restore_committed_audio_pads(
                                            audio.as_mut(),
                                            &self.pads,
                                            &self.pad_mixes,
                                            &self.current_session_bound,
                                            0,
                                            offset,
                                        )
                                    {
                                        candidate.admission =
                                            ProjectAdmission::RestoreCommittedPads {
                                                next_pad,
                                                end_pad: offset,
                                            };
                                        self.status = format!(
                                            "Project admission failed ({error}); audio rollback failed: {rollback_error}"
                                        );
                                    } else {
                                        candidate.admission = ProjectAdmission::StopAll;
                                        candidate.progress.admitted_actions = 1;
                                        self.status = format!(
                                            "Project admission failed ({error}); committed audio restored"
                                        );
                                    }
                                }
                            }
                        }
                        ProjectAdmission::RestoreCommittedMaster { end_pad } => {
                            match audio.update_master_mix(self.master_mix) {
                                Ok(()) => {
                                    candidate.admission = ProjectAdmission::RestoreCommittedPads {
                                        next_pad: 0,
                                        end_pad,
                                    };
                                    changed = true;
                                }
                                Err(error) => {
                                    self.status = format!(
                                        "Audio rollback failed: restore master mixer: {error}"
                                    );
                                }
                            }
                        }
                        ProjectAdmission::RestoreCommittedPads { next_pad, end_pad } => {
                            match restore_committed_audio_pads(
                                audio.as_mut(),
                                &self.pads,
                                &self.pad_mixes,
                                &self.current_session_bound,
                                next_pad,
                                end_pad,
                            ) {
                                Ok(()) => {
                                    candidate.admission = ProjectAdmission::StopAll;
                                    candidate.progress.admitted_actions = 1;
                                    self.status =
                                        "Committed audio restored; restarting project admission"
                                            .to_owned();
                                    changed = true;
                                }
                                Err((next_pad, error)) => {
                                    candidate.admission = ProjectAdmission::RestoreCommittedPads {
                                        next_pad,
                                        end_pad,
                                    };
                                    self.status = format!("Audio rollback failed: {error}");
                                }
                            }
                        }
                        ProjectAdmission::Patterns(submitted) => {
                            let maintenance =
                                candidate.patterns.maintain(audio.as_mut(), self.telemetry);
                            if maintenance.submitted_slot.is_some() {
                                let next = submitted + 1;
                                candidate.progress.admitted_actions += 1;
                                candidate.admission = if next == sampler_core::PATTERN_SLOT_COUNT {
                                    ProjectAdmission::Complete
                                } else {
                                    ProjectAdmission::Patterns(next)
                                };
                                changed = true;
                            }
                            if let Some(status) = maintenance.status {
                                self.status = pattern_status_text(&status);
                            }
                        }
                        ProjectAdmission::MidiOwners | ProjectAdmission::Complete => {}
                    }
                }
            }
        }
        if matches!(&operation, ProjectOpenOperation::Staging(candidate) if matches!(candidate.admission, ProjectAdmission::Complete))
        {
            let ProjectOpenOperation::Staging(candidate) = operation else {
                unreachable!()
            };
            self.commit_project_open(candidate, now);
            return true;
        }
        self.project_open = Some(operation);
        changed
    }

    fn project_open_is_admitting(&self) -> bool {
        matches!(
            self.project_open.as_ref(),
            Some(ProjectOpenOperation::Staging(candidate))
                if candidate.progress.phase == ProjectOpenPhase::Admitting
        )
    }

    fn commit_project_open(&mut self, mut candidate: Box<ProjectOpenCandidate>, now: Instant) {
        let managed_to_release: Vec<_> = self
            .sample_editor
            .commits
            .iter_mut()
            .filter_map(|commit| commit.managed_capture.take())
            .collect();
        for id in managed_to_release {
            self.queue_managed_release(id);
        }
        let mut pads: [PadView; PAD_VIEW_COUNT] = array::from_fn(|_| PadView::default());
        let mut commits: [SampleCommit; PAD_VIEW_COUNT] =
            array::from_fn(|_| SampleCommit::default());
        let mut pad_mixes = [PadMixSettings::default(); PAD_VIEW_COUNT];
        for offset in 0..PAD_VIEW_COUNT {
            let Some(staged) = candidate.staged_pads[offset].take() else {
                continue;
            };
            let StagedProjectPad {
                path,
                settings,
                mix,
                loaded,
            } = *staged;
            let label = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned();
            pads[offset] = PadView {
                source: Some(path),
                label,
                settings,
                generation: 1,
                state: PadLoadState::Ready,
                sample: Some(loaded.rendered),
                preview: crate::loader::downsample_preview(&loaded.rendered_preview),
                active: false,
            };
            commits[offset] = SampleCommit {
                base: Some(loaded.base),
                source_generation: 1,
                fingerprint: Some(loaded.fingerprint),
                recipe: loaded.recipe,
                base_preview: Some(loaded.base_preview),
                rendered_preview: Some(loaded.rendered_preview),
                managed_capture: None,
            };
            pad_mixes[offset] = mix;
        }

        let dirty_since = candidate.restored_recovery.then_some(now);
        let autosaved_revision = if candidate.restored_recovery {
            candidate.document.revision
        } else {
            candidate.saved_revision
        };
        self.pads = pads;
        self.pad_mixes = pad_mixes;
        self.master_mix = candidate.document.master_mix;
        self.midi_settings = candidate.document.midi;
        self.patterns = candidate.patterns;
        self.project_session = ProjectSession::opened(
            candidate.document.project_id,
            candidate.progress.directory,
            candidate.document.name,
            candidate.document.revision,
            candidate.saved_revision,
            autosaved_revision,
            dirty_since,
        );
        self.pending_loads.fill_with(|| None);
        self.committed_recovery_loads.fill_with(|| None);
        self.reinstall_pending.fill(false);
        self.current_session_bound = array::from_fn(|index| self.pads[index].sample.is_some());
        *self.sample_editor = SampleEditorState {
            commits,
            generations: [1; PAD_VIEW_COUNT],
            pending: array::from_fn(|_| None),
            deferred_results: array::from_fn(|_| None),
            undo: array::from_fn(|_| None),
            generation_exhausted: [false; PAD_VIEW_COUNT],
        };
        self.active_bank = BankId::new(0).expect("first bank is valid");
        self.selected_pad = 0;
        self.apply_sample_context = None;
        self.held_pad_by_key.fill(None);
        self.pending_pattern_transport = None;
        self.editor = SampleEditor::open_empty(PadId::first(), self.pads[0].settings);
        self.sync_editor_to_selected_pad();
        self.overlay = None;
        self.project_open_error = None;
        self.status = format!("Opened {}", self.project_session.name());
    }

    fn apply_project_probe(
        &mut self,
        token: ProjectToken,
        directory: PathBuf,
        result: Result<ProjectProbe, ProjectStoreError>,
    ) -> bool {
        let Some(ProjectOpenOperation::Probing {
            progress,
            worker_queued: true,
        }) = self.project_open.as_ref()
        else {
            return false;
        };
        if progress.token != token || progress.directory != directory {
            return false;
        }
        self.project_open = None;
        let probe = match result {
            Ok(probe) => probe,
            Err(error) => {
                self.fail_project_open(ProjectOpenError::Probe(error));
                return true;
            }
        };
        let explicit = match probe.explicit {
            Some(Ok(document)) => Some(document),
            Some(Err(error)) => {
                if probe
                    .recovery
                    .as_ref()
                    .is_none_or(|recovery| recovery.is_err())
                {
                    self.fail_project_open(ProjectOpenError::Probe(error));
                    return true;
                }
                None
            }
            None => None,
        };
        let recovery = match probe.recovery {
            Some(Ok(document)) => Some(document),
            Some(Err(error)) => {
                self.fail_project_open(ProjectOpenError::Probe(error));
                return true;
            }
            None => None,
        };

        if let (Some(explicit), Some(recovery)) = (&explicit, &recovery)
            && explicit.project_id != recovery.project_id
        {
            self.fail_project_open(ProjectOpenError::RecoveryMismatch);
            return true;
        }
        if let Some(recovery) = recovery
            && explicit
                .as_ref()
                .is_none_or(|explicit| recovery.revision > explicit.revision)
        {
            let progress = ProjectOpenStage {
                token,
                directory: probe.directory,
                project_id: Some(recovery.project_id),
                revision: Some(recovery.revision),
                phase: ProjectOpenPhase::AwaitingRecoveryChoice,
                staged_pads: 0,
                total_pads: recovery.pads.len(),
                admitted_actions: 0,
                total_actions: 3 + PAD_VIEW_COUNT + sampler_core::PATTERN_SLOT_COUNT,
            };
            self.project_open = Some(ProjectOpenOperation::ChoosingRecovery(Box::new(
                ProjectRecoveryChoiceState {
                    progress,
                    explicit,
                    recovery,
                    discard_requested: false,
                    discard_queued: false,
                },
            )));
            self.status = "A newer recovery is available".to_owned();
            return true;
        }
        let Some(document) = explicit else {
            self.fail_project_open(ProjectOpenError::NoUsableDocument);
            return true;
        };
        match self.build_project_open_candidate(
            token,
            probe.directory,
            document.clone(),
            document.revision,
            false,
        ) {
            Ok(operation) => self.project_open = Some(operation),
            Err(error) => {
                self.fail_project_open(error);
                return true;
            }
        }
        self.status = "Staging project audio…".to_owned();
        true
    }

    fn build_project_open_candidate(
        &self,
        token: ProjectToken,
        directory: PathBuf,
        mut document: ProjectDocument,
        saved_revision: u64,
        restored_recovery: bool,
    ) -> Result<ProjectOpenOperation, ProjectOpenError> {
        let Some((sample_rate, _)) = self.audio_format else {
            return Err(ProjectOpenError::AudioUnavailable);
        };
        document
            .master_mix
            .validate()
            .map_err(|error| ProjectOpenError::InvalidDocument(error.to_string()))?;
        for pad in &document.pads {
            pad.settings
                .validate()
                .map_err(|error| ProjectOpenError::InvalidDocument(error.to_string()))?;
            pad.mix
                .validate()
                .map_err(|error| ProjectOpenError::InvalidDocument(error.to_string()))?;
        }
        let mut patterns = PatternWorkspace::new(sample_rate);
        patterns
            .replace_project_patterns(document.patterns.clone())
            .map_err(|error| ProjectOpenError::InvalidPatterns(error.to_string()))?;
        patterns
            .rebuild_sample_rate(sample_rate)
            .map_err(|error| ProjectOpenError::InvalidPatterns(error.to_string()))?;
        document.pads.sort_by_key(|pad| pad_offset(pad.pad));
        let progress = ProjectOpenStage {
            token,
            directory,
            project_id: Some(document.project_id),
            revision: Some(document.revision),
            phase: ProjectOpenPhase::Staging,
            staged_pads: 0,
            total_pads: document.pads.len(),
            admitted_actions: 0,
            total_actions: 3 + PAD_VIEW_COUNT + sampler_core::PATTERN_SLOT_COUNT,
        };
        Ok(ProjectOpenOperation::Staging(Box::new(
            ProjectOpenCandidate {
                progress,
                document,
                patterns,
                staged_pads: array::from_fn(|_| None),
                next_decode: 0,
                decode_in_flight: None,
                stage_generation: 0,
                engine_rate: sample_rate,
                saved_revision,
                restored_recovery,
                admission: ProjectAdmission::MidiOwners,
            },
        )))
    }

    pub fn choose_project_recovery(
        &mut self,
        choice: RecoveryChoice,
    ) -> Result<(), ProjectOpenError> {
        let Some(ProjectOpenOperation::ChoosingRecovery(state)) = self.project_open.take() else {
            return Err(ProjectOpenError::OperationPending);
        };
        let ProjectRecoveryChoiceState {
            progress,
            explicit,
            recovery,
            discard_requested,
            discard_queued,
        } = *state;
        if discard_queued {
            self.project_open = Some(ProjectOpenOperation::ChoosingRecovery(Box::new(
                ProjectRecoveryChoiceState {
                    progress,
                    explicit,
                    recovery,
                    discard_requested,
                    discard_queued,
                },
            )));
            return Err(ProjectOpenError::CancellationLocked);
        }
        match choice {
            RecoveryChoice::Cancel => {
                self.overlay = None;
                self.status = "Project open cancelled".to_owned();
            }
            RecoveryChoice::Restore => {
                let saved_revision = explicit
                    .as_ref()
                    .map_or_else(|| recovery.revision.saturating_sub(1), |doc| doc.revision);
                let candidate = self.build_project_open_candidate(
                    progress.token,
                    progress.directory,
                    recovery,
                    saved_revision,
                    true,
                );
                match candidate {
                    Ok(candidate) => self.project_open = Some(candidate),
                    Err(error) => {
                        self.fail_project_open(error.clone());
                        return Err(error);
                    }
                }
                self.status = "Staging recovered project audio…".to_owned();
            }
            RecoveryChoice::Discard => {
                if explicit.is_none() {
                    self.project_open = Some(ProjectOpenOperation::ChoosingRecovery(Box::new(
                        ProjectRecoveryChoiceState {
                            progress,
                            explicit,
                            recovery,
                            discard_requested,
                            discard_queued,
                        },
                    )));
                    return Err(ProjectOpenError::NoUsableDocument);
                }
                let discard_queued = if self.pending_worker_requests.len() < WORKER_CHANNEL_CAPACITY
                {
                    self.pending_worker_requests
                        .push(WorkerRequest::DiscardRecovery {
                            token: progress.token,
                            directory: progress.directory.clone(),
                            project_id: recovery.project_id,
                            revision: recovery.revision,
                        });
                    true
                } else {
                    false
                };
                self.project_open = Some(ProjectOpenOperation::ChoosingRecovery(Box::new(
                    ProjectRecoveryChoiceState {
                        progress,
                        explicit,
                        recovery,
                        discard_requested: true,
                        discard_queued,
                    },
                )));
                self.status = "Discarding exact recovery…".to_owned();
            }
        }
        Ok(())
    }

    fn apply_project_sample_staged(&mut self, result: WorkerResult) -> bool {
        let WorkerResult::ProjectSampleStaged {
            token,
            generation,
            pad,
            revision,
            path,
            recipe,
            result,
        } = result
        else {
            return false;
        };
        let Some(ProjectOpenOperation::Staging(candidate)) = self.project_open.as_ref() else {
            return false;
        };
        let Some(expected) = candidate.document.pads.get(candidate.next_decode) else {
            return false;
        };
        let expected_settings = expected.settings;
        let expected_mix = expected.mix;
        let expected_path = candidate.progress.directory.join(&expected.audio_path);
        if candidate.progress.token != token
            || candidate.decode_in_flight != Some((pad, generation))
            || candidate.stage_generation != generation
            || expected.pad != pad
            || candidate.document.revision != revision
            || expected_path != path
            || expected.recipe != recipe
        {
            return false;
        }

        let loaded = match result {
            Ok(loaded)
                if loaded.fingerprint.digest == expected.asset_digest
                    && loaded.recipe == recipe
                    && self.audio_format.is_some_and(|(sample_rate, _)| {
                        loaded.rendered.sample_rate() == sample_rate
                    }) =>
            {
                loaded
            }
            Ok(loaded) => {
                let stage_error = if loaded.fingerprint.digest != expected.asset_digest {
                    ProjectStageError::AssetDigestChanged
                } else if loaded.recipe != recipe {
                    ProjectStageError::RecipeContextChanged
                } else {
                    ProjectStageError::AudioDeviceRateChanged
                };
                self.fail_project_open(ProjectOpenError::Stage {
                    pad,
                    error: stage_error,
                });
                return true;
            }
            Err(error) => {
                self.fail_project_open(ProjectOpenError::Stage {
                    pad,
                    error: ProjectStageError::Load(error),
                });
                return true;
            }
        };

        let Some(ProjectOpenOperation::Staging(candidate)) = self.project_open.as_mut() else {
            return false;
        };
        candidate.staged_pads[pad_offset(pad)] = Some(Box::new(StagedProjectPad {
            path,
            settings: expected_settings,
            mix: expected_mix,
            loaded,
        }));
        candidate.next_decode += 1;
        candidate.decode_in_flight = None;
        candidate.progress.staged_pads = candidate.next_decode;
        self.status = format!(
            "Staged {}/{} project samples",
            candidate.progress.staged_pads, candidate.progress.total_pads
        );
        true
    }

    fn apply_project_recovery_discarded(
        &mut self,
        token: ProjectToken,
        directory: PathBuf,
        project_id: ProjectId,
        revision: u64,
        result: Result<(), ProjectStoreError>,
    ) -> Option<bool> {
        let ProjectOpenOperation::ChoosingRecovery(choice) = self.project_open.as_ref()? else {
            return Some(false);
        };
        if !choice.discard_requested
            || !choice.discard_queued
            || choice.progress.token != token
            || choice.progress.directory != directory
            || choice.recovery.project_id != project_id
            || choice.recovery.revision != revision
        {
            return Some(false);
        }
        let explicit = choice
            .explicit
            .clone()
            .expect("recovery discard is offered only with an explicit document");
        self.project_open = None;
        if let Err(error) = result {
            self.fail_project_open(ProjectOpenError::RecoveryDiscard(error));
            return Some(true);
        }
        match self.build_project_open_candidate(
            token,
            directory,
            explicit.clone(),
            explicit.revision,
            false,
        ) {
            Ok(operation) => {
                self.project_open = Some(operation);
                self.status = "Staging project audio…".to_owned();
            }
            Err(error) => {
                self.fail_project_open(error);
            }
        }
        Some(true)
    }

    fn restore_busy_project_recovery_discard(
        &mut self,
        token: ProjectToken,
        directory: &Path,
        project_id: ProjectId,
        revision: u64,
        error: WorkerSendError,
    ) -> Option<bool> {
        let ProjectOpenOperation::ChoosingRecovery(choice) = self.project_open.as_mut()? else {
            return Some(false);
        };
        if !choice.discard_requested
            || choice.progress.token != token
            || choice.progress.directory != directory
            || choice.recovery.project_id != project_id
            || choice.recovery.revision != revision
        {
            return Some(false);
        }
        if error == WorkerSendError::WorkerBusy {
            choice.discard_queued = false;
        } else {
            self.project_open = None;
            self.overlay = None;
        }
        Some(true)
    }

    pub fn project_save_error(&self) -> Option<&ProjectSaveFailure> {
        self.project_save_error.as_ref()
    }

    pub fn recovery_cleanup_warning(&self) -> Option<&ProjectStoreError> {
        self.recovery_cleanup_warning.as_ref()
    }

    pub fn project_header(&self) -> String {
        let identity = if self.project_session.directory().is_some() {
            self.project_session.name().to_owned()
        } else {
            "UNTITLED".to_owned()
        };
        let truth = match self.project_session.status() {
            crate::ProjectStatus::Clean => "SAVED",
            crate::ProjectStatus::Modified => "MODIFIED",
            crate::ProjectStatus::Saving(SaveKind::Explicit) => "SAVING",
            crate::ProjectStatus::Saving(SaveKind::Recovery) => "AUTOSAVING",
        };
        let mut header = format!("{identity} · {truth}");
        if self.project_session.pending_autosave().is_some() || self.pending_autosave_save.is_some()
        {
            header.push_str(" · AUTOSAVE PENDING");
        }
        if let Some(failure) = &self.project_save_error {
            let label = if failure.kind == SaveKind::Recovery {
                "AUTOSAVE ERROR"
            } else {
                "SAVE ERROR"
            };
            header.push_str(&format!(" · {label}: {}", failure.error));
        }
        if let Some(warning) = &self.recovery_cleanup_warning {
            header.push_str(&format!(" · RECOVERY CLEANUP WARNING: {warning}"));
        }
        header
    }

    fn ensure_project_request_available(&self) -> Result<(), ProjectSaveError> {
        if self.pending_explicit_save.is_some()
            || self.in_flight_project.is_some()
            || self.pending_recovery_cleanup.len() >= WORKER_CHANNEL_CAPACITY
        {
            Err(ProjectSaveError::OperationPending)
        } else {
            Ok(())
        }
    }

    fn allocate_project_token(&mut self) -> Result<ProjectToken, ProjectSaveError> {
        let token = ProjectToken::new(self.next_project_token);
        self.next_project_token = self
            .next_project_token
            .checked_add(1)
            .ok_or(ProjectSaveError::TokenExhausted)?;
        Ok(token)
    }

    fn enqueue_project_save(&mut self, save: PendingProjectSave) {
        let descriptor = save.descriptor.clone();
        self.pending_worker_requests
            .push(WorkerRequest::SaveProject(Box::new(
                ProjectSaveWorkerRequest {
                    token: descriptor.token,
                    request: ProjectSaveRequest {
                        directory: descriptor.directory.clone(),
                        save_as: save.save_as,
                        kind: descriptor.kind,
                        snapshot: save.snapshot.clone(),
                    },
                },
            )));
        self.project_session.set_in_flight(Some(descriptor));
        self.in_flight_project = Some(InFlightProjectOperation::Save(Box::new(save)));
    }

    pub fn project_snapshot(&self) -> Result<ProjectSaveSnapshot, ProjectSnapshotError> {
        if let Some(phase) = self.capture_session.phase() {
            return Err(ProjectSnapshotError::UnresolvedCapture(phase));
        }
        if let Some(operation) = self.project_session.in_flight() {
            return Err(ProjectSnapshotError::PendingProjectOperation(
                operation.token,
            ));
        }
        if self.editor.is_dirty() {
            return Err(ProjectSnapshotError::DirtySampleDraft(self.editor.pad()));
        }
        let mut pads = Vec::with_capacity(PAD_VIEW_COUNT);
        for offset in 0..PAD_VIEW_COUNT {
            let pad = pad_from_offset(offset);
            if self.pending_loads[offset]
                .as_ref()
                .is_some_and(|pending| !matches!(pending.phase, PendingLoadPhase::Failed))
                || self.committed_recovery_loads[offset]
                    .as_ref()
                    .is_some_and(|pending| !matches!(pending.phase, PendingLoadPhase::Failed))
            {
                return Err(ProjectSnapshotError::PendingSampleLoad(pad));
            }
            if self.sample_editor.pending[offset]
                .as_ref()
                .is_some_and(|pending| !matches!(pending.phase, PendingEditPhase::Failed))
            {
                return Err(ProjectSnapshotError::PendingSampleEdit(pad));
            }
            if self.pads[offset].sample.is_none() {
                continue;
            }
            let Some(source_path) = self.pads[offset].source.clone() else {
                return Err(ProjectSnapshotError::UnresolvedSample(pad));
            };
            let Some(fingerprint) = self.sample_editor.commits[offset].fingerprint else {
                return Err(ProjectSnapshotError::UnresolvedSample(pad));
            };
            pads.push(ProjectSavePad {
                pad,
                source_path,
                source_generation: self.sample_editor.commits[offset].source_generation,
                fingerprint,
                settings: self.pads[offset].settings,
                mix: self.pad_mixes[offset],
                recipe: self.sample_editor.commits[offset].recipe,
            });
        }
        let patterns = self
            .patterns
            .export_project_patterns()
            .map_err(|error| ProjectSnapshotError::InvalidPatterns(error.to_string()))?;
        Ok(ProjectSaveSnapshot {
            project_id: self.project_session.project_id(),
            name: self.project_session.name().to_owned(),
            revision: self.project_session.current_revision(),
            master_mix: self.master_mix,
            midi: self.midi_settings,
            pads,
            patterns,
        })
    }

    pub fn discard_sample_draft(&mut self) {
        self.editor.confirm_discard();
    }

    #[cfg(test)]
    pub(crate) fn editor_mut_for_test(&mut self) -> &mut SampleEditor {
        &mut self.editor
    }

    #[cfg(test)]
    pub(crate) fn patterns_mut_for_test(&mut self) -> &mut PatternWorkspace {
        &mut self.patterns
    }

    pub fn tick(&mut self) {
        const METER_DECAY: f32 = 0.85;

        let next = self
            .audio
            .as_mut()
            .and_then(|audio| audio.latest_telemetry());
        self.meter_left = sanitize_peak(self.meter_left * METER_DECAY);
        self.meter_right = sanitize_peak(self.meter_right * METER_DECAY);
        if let Some(telemetry) = next {
            self.apply_telemetry(telemetry);
        }
    }

    pub fn maintain_audio(&mut self) -> bool {
        if self.audio.is_none() {
            return false;
        }
        self.edit_result_advanced = false;
        let mut changed = self.advance_one_deferred_edit_result();
        let runtime_error = {
            let audio = self.audio.as_mut().expect("audio was checked above");
            audio.reclaim_retired();
            audio.poll_runtime_error()
        };

        if let Some(error) = runtime_error {
            self.fail_audio(error);
            true
        } else if self.project_open.is_some() {
            changed
        } else {
            changed |= self.pump_recovery_requests();
            changed |= self.pump_pending_sample_edit();
            let telemetry = self
                .audio
                .as_mut()
                .and_then(|audio| audio.latest_telemetry());
            if let Some(telemetry) = telemetry {
                changed |= self.apply_telemetry(telemetry);
            }
            let recording_mutation_budget = usize::try_from(
                crate::MAX_PROJECT_REVISION.saturating_sub(self.project_revision()),
            )
            .unwrap_or(usize::MAX);
            let maintenance = {
                let audio = self
                    .audio
                    .as_mut()
                    .expect("audio remains present after a successful poll");
                self.patterns.maintain_with_recording_budget(
                    audio.as_mut(),
                    self.telemetry,
                    recording_mutation_budget,
                )
            };
            for _ in 0..maintenance.committed_mutations {
                self.commit_project_mutation();
            }
            changed |= maintenance.reclaimed_snapshots > 0
                || maintenance.drained_acks > 0
                || maintenance.compiled_slot.is_some()
                || maintenance.submitted_slot.is_some();
            self.recorded_ack_count = self
                .recorded_ack_count
                .saturating_add(maintenance.drained_acks);
            if maintenance.submitted_slot.is_some() {
                self.pattern_submission_count = self.pattern_submission_count.saturating_add(1);
            }
            if let Some(status) = maintenance.status {
                let unsupported_bootstrap = matches!(
                    &status,
                    PatternStatus::AudioCommandFailed { error, .. }
                        if error == "pattern audio is unsupported"
                ) && self.patterns.view() == WorkspaceView::Perform;
                if !unsupported_bootstrap {
                    self.status = pattern_status_text(&status);
                    changed = true;
                }
            }
            changed
        }
    }

    fn advance_one_deferred_edit_result(&mut self) -> bool {
        for offset in 0..PAD_VIEW_COUNT {
            let Some(result) = self.sample_editor.deferred_results[offset].take() else {
                continue;
            };
            let WorkerResult::Edited {
                pad,
                generation,
                recipe,
                result,
            } = *result
            else {
                continue;
            };
            if !self.pending_edit_matches(pad, generation, recipe) {
                continue;
            }
            self.edit_result_advanced = true;
            return self.apply_edited_worker_result(pad, generation, recipe, result);
        }
        false
    }

    fn pending_edit_matches(&self, pad: PadId, generation: u64, recipe: SampleEditRecipe) -> bool {
        self.sample_editor.pending[pad_offset(pad)]
            .as_ref()
            .is_some_and(|pending| {
                pending.generation == generation
                    && pending.recipe == recipe
                    && matches!(pending.phase, PendingEditPhase::WorkerQueued)
            })
    }

    fn pump_pending_sample_edit(&mut self) -> bool {
        for offset in 0..PAD_VIEW_COUNT {
            let phase =
                self.sample_editor.pending[offset]
                    .as_ref()
                    .map(|pending| match pending.phase {
                        PendingEditPhase::AwaitingWorker => 0,
                        PendingEditPhase::Ready(_) => 1,
                        PendingEditPhase::WorkerQueued | PendingEditPhase::Failed => 2,
                    });
            match phase {
                Some(0) => {
                    let pending = self.sample_editor.pending[offset]
                        .as_mut()
                        .expect("pending edit exists for its phase");
                    pending.phase = PendingEditPhase::WorkerQueued;
                    let request = WorkerRequest::EditSample {
                        pad: pad_from_offset(offset),
                        generation: pending.generation,
                        base: Arc::clone(&pending.base),
                        base_preview: Arc::clone(&pending.base_preview),
                        recipe: pending.recipe,
                    };
                    self.queue_worker_request(request);
                    return true;
                }
                Some(1) => return self.install_pending_sample_edit(offset),
                Some(2) | None => {}
                Some(_) => unreachable!("edit phase encoding is exhaustive"),
            }
        }
        false
    }

    fn install_pending_sample_edit(&mut self, offset: usize) -> bool {
        let Some(mut pending) = self.sample_editor.pending[offset].take() else {
            return false;
        };
        let PendingEditPhase::Ready(rendered) = pending.phase else {
            self.sample_editor.pending[offset] = Some(pending);
            return false;
        };
        if let Err(error) = self.ensure_project_mutation_available() {
            pending.phase = PendingEditPhase::Ready(rendered);
            self.sample_editor.pending[offset] = Some(pending);
            self.status = error;
            return true;
        }
        let Some(audio) = self.audio.as_mut() else {
            pending.phase = PendingEditPhase::Ready(rendered);
            self.sample_editor.pending[offset] = Some(pending);
            return false;
        };
        if rendered.rendered.sample_rate() != audio.sample_rate() {
            pending.phase = PendingEditPhase::Ready(rendered);
            self.sample_editor.pending[offset] = Some(pending);
            self.pads[offset].state = PadLoadState::WaitingForDevice;
            return true;
        }
        let pad = pad_from_offset(offset);
        let settings = self.pads[offset].settings;
        if let Err(error) = audio.install(
            pad,
            Arc::clone(&rendered.rendered),
            settings,
            self.pad_mixes[offset],
        ) {
            let kind = pending.kind;
            pending.phase = PendingEditPhase::Ready(rendered);
            self.sample_editor.pending[offset] = Some(pending);
            self.pads[offset].state = PadLoadState::Error(error.clone());
            self.status = error;
            if self.patterns.view() == WorkspaceView::Sample && self.selected_pad_id() == Some(pad)
            {
                match kind {
                    PendingEditKind::Apply => self
                        .editor
                        .observe_apply_failed(SampleEditorError::InstallFailed),
                    PendingEditKind::Undo => self
                        .editor
                        .observe_undo_failed(SampleEditorError::InstallFailed),
                }
            }
            self.fail_project_sample_apply(pad);
            return true;
        }

        let checkpoint = match (
            self.sample_editor.commits[offset].base.as_ref(),
            self.pads[offset].sample.as_ref(),
            self.sample_editor.commits[offset].base_preview.as_ref(),
            self.sample_editor.commits[offset].rendered_preview.as_ref(),
        ) {
            (Some(base), Some(sample), Some(base_preview), Some(rendered_preview)) => {
                Some(Box::new(SampleEditCheckpoint {
                    base: Arc::clone(base),
                    rendered: Arc::clone(sample),
                    recipe: self.sample_editor.commits[offset].recipe,
                    base_preview: Arc::clone(base_preview),
                    rendered_preview: Arc::clone(rendered_preview),
                }))
            }
            _ => None,
        };
        let view = &mut self.pads[offset];
        self.sample_editor.commits[offset].base = Some(pending.base);
        self.sample_editor.commits[offset].base_preview = Some(rendered.base_preview);
        self.sample_editor.commits[offset].recipe = pending.recipe;
        view.sample = Some(rendered.rendered);
        self.sample_editor.commits[offset].rendered_preview =
            Some(Arc::clone(&rendered.rendered_preview));
        view.preview = crate::loader::downsample_preview(&rendered.rendered_preview);
        view.state = PadLoadState::Ready;
        self.current_session_bound[offset] = true;
        match pending.kind {
            PendingEditKind::Apply => self.sample_editor.undo[offset] = checkpoint,
            PendingEditKind::Undo => self.sample_editor.undo[offset] = None,
        }
        self.status = if pending.kind == PendingEditKind::Undo {
            "Undid sample edit".to_owned()
        } else {
            "Applied sample edit".to_owned()
        };
        if self.patterns.view() == WorkspaceView::Sample && self.selected_pad_id() == Some(pad) {
            if pending.kind == PendingEditKind::Undo {
                self.editor.observe_undo_succeeded();
            } else {
                self.editor.observe_apply_succeeded();
            }
            self.sync_editor_to_selected_pad();
        }
        self.commit_project_mutation();
        if self.project_lifecycle_wait == Some(ProjectLifecycleWait::SampleApply) {
            self.project_lifecycle_wait = None;
            self.advance_project_action();
        }
        true
    }

    pub fn pad(&self, pad: PadId) -> &PadView {
        &self.pads[pad_offset(pad)]
    }

    pub fn pad_mix(&self, pad: PadId) -> PadMixSettings {
        self.pad_mixes[pad_offset(pad)]
    }

    pub const fn master_mix(&self) -> MasterMixSettings {
        self.master_mix
    }

    pub const fn mixer_cursor(&self) -> &MixerCursor {
        &self.mixer_cursor
    }

    pub fn update_pad_mix(&mut self, pad: PadId, settings: PadMixSettings) -> Result<(), String> {
        settings.validate().map_err(|error| error.to_string())?;
        let offset = pad_offset(pad);
        if self.pad_mixes[offset] == settings {
            return Ok(());
        }
        if self.pads[offset].sample.is_none() {
            self.pad_mixes[offset] = settings;
            return Ok(());
        }
        if !self.current_session_bound[offset] || self.audio.is_none() {
            return Err("loaded sample is not admitted to the current audio session".to_owned());
        }
        self.ensure_project_mutation_available()?;
        self.audio
            .as_mut()
            .expect("current session binding requires an audio controller")
            .update_pad_mix(pad, settings)?;
        self.pad_mixes[offset] = settings;
        self.commit_project_mutation();
        Ok(())
    }

    pub fn update_master_mix(&mut self, settings: MasterMixSettings) -> Result<(), String> {
        settings.validate().map_err(|error| error.to_string())?;
        if self.master_mix == settings {
            return Ok(());
        }
        if self.audio.is_none() {
            return Err("audio is unavailable".to_owned());
        }
        self.ensure_project_mutation_available()?;
        self.audio
            .as_mut()
            .expect("audio availability was checked")
            .update_master_mix(settings)?;
        self.master_mix = settings;
        self.commit_project_mutation();
        Ok(())
    }

    /// Atomically updates a pad's validated settings. Unloaded pads remain a local edit; loaded
    /// pads commit only after audio accepts the corresponding update.
    pub fn update_pad_settings(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String> {
        settings.validate().map_err(|error| error.to_string())?;
        let offset = pad_offset(pad);
        if self.pads[offset].settings == settings {
            return Ok(());
        }
        if self.pads[offset].sample.is_none() {
            self.pads[offset].settings = settings;
            return Ok(());
        }
        if !self.current_session_bound[offset] || self.audio.is_none() {
            return Err("loaded sample is not admitted to the current audio session".to_owned());
        }
        self.ensure_project_mutation_available()?;
        self.audio
            .as_mut()
            .expect("current session binding requires an audio controller")
            .update_pad(pad, settings)?;
        self.pads[offset].settings = settings;
        self.commit_project_mutation();
        Ok(())
    }

    fn ensure_project_mutation_available(&self) -> Result<(), String> {
        self.project_session
            .ensure_mutation_available()
            .map_err(|_| "project revision is exhausted".to_owned())
    }

    fn commit_project_mutation(&mut self) {
        self.project_session
            .commit_project_mutation(Instant::now(), || Ok::<(), ()>(()))
            .expect("project mutation was preflighted before its domain commit");
    }

    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    pub fn open_help(&mut self) {
        self.overlay = Some(Overlay::Help);
    }

    pub fn open_palette(&mut self) {
        self.palette.clear();
        self.palette_error = None;
        self.overlay = Some(Overlay::Palette);
    }

    pub fn open_picker(&mut self) {
        let source_parent = self
            .selected_pad_id()
            .and_then(|pad| self.pad(pad).source.as_deref())
            .and_then(|path| path.parent())
            .filter(|path| !path.as_os_str().is_empty())
            .map(ToOwned::to_owned);
        let directory = source_parent.unwrap_or_else(|| self.current_dir.clone());
        self.open_picker_at(directory);
    }

    pub fn open_picker_at(&mut self, directory: impl Into<PathBuf>) {
        let directory = resolve_picker_directory(&self.current_dir, directory.into());
        let request_id = self.file_picker.begin_scan(directory.clone());
        self.queue_worker_request(WorkerRequest::ScanDirectory {
            request_id,
            path: directory,
            show_hidden: self.file_picker.show_hidden(),
        });
        self.overlay = Some(Overlay::FilePicker);
    }

    pub fn close_overlay(&mut self) {
        let device_error = matches!(self.overlay, Some(Overlay::DeviceError(_)));
        if matches!(
            self.overlay,
            Some(Overlay::ApplySample { .. } | Overlay::DiscardSample { .. })
        ) {
            self.editor.cancel_confirmation();
            self.apply_sample_context = None;
        }
        if self.overlay == Some(Overlay::Palette) {
            self.palette_error = None;
        }
        if let Some(Overlay::DeviceError(error)) = &self.overlay {
            self.status = format!("{error} · Ctrl+R retries audio");
        }
        self.overlay = None;
        if device_error
            && self.pending_project_action.is_some()
            && self.project_lifecycle_wait.is_none()
        {
            self.advance_project_action();
        }
    }

    fn cancel_overlay(&mut self) {
        match self.overlay {
            Some(Overlay::ProjectOpenProgress) => {
                let _ = self.cancel_project_open();
                return;
            }
            Some(Overlay::ResolveSampleDraft { .. } | Overlay::UnsavedProject { .. }) => {
                self.cancel_project_action();
                return;
            }
            Some(Overlay::ProjectLifecycleProgress { .. } | Overlay::ProjectSaveProgress) => return,
            _ => {}
        }
        self.close_overlay();
    }

    pub fn palette_text(&self) -> &str {
        self.palette.text()
    }

    pub fn palette_cursor(&self) -> usize {
        self.palette.cursor()
    }

    pub fn palette_error(&self) -> Option<&str> {
        self.palette_error.as_deref()
    }

    pub fn file_picker(&self) -> &FilePicker {
        &self.file_picker
    }

    pub(crate) fn pad_display_source(&self, offset: usize) -> Option<&Path> {
        self.pads
            .get(offset)
            .and_then(|pad| pad.source.as_deref())
            .or_else(|| {
                self.pending_loads
                    .get(offset)
                    .and_then(Option::as_deref)
                    .map(|pending| pending.path.as_path())
            })
    }

    pub fn take_worker_requests(&mut self) -> Vec<WorkerRequest> {
        mem::take(&mut self.pending_worker_requests)
    }

    pub fn apply_worker_send_error(
        &mut self,
        request: WorkerRequest,
        error: WorkerSendError,
    ) -> bool {
        let affected_offset = match &request {
            WorkerRequest::LoadSample { pad, .. } | WorkerRequest::EditSample { pad, .. } => {
                Some(pad_offset(*pad))
            }
            WorkerRequest::ScanDirectory { .. }
            | WorkerRequest::SaveProject(_)
            | WorkerRequest::ProbeProject { .. }
            | WorkerRequest::DiscardRecovery { .. }
            | WorkerRequest::StageProjectSample(_)
            | WorkerRequest::FinalizeCapture(_)
            | WorkerRequest::ReleaseManagedCapture { .. }
            | WorkerRequest::Shutdown => None,
        };
        let message = error.to_string();
        let applied = match request {
            WorkerRequest::LoadSample {
                pad,
                generation,
                purpose,
                path,
                ..
            } => {
                let offset = pad_offset(pad);
                if let Some(kind) = self.matching_pending_load(offset, generation, purpose, &path) {
                    if error == WorkerSendError::WorkerBusy {
                        if let Some(pending) = self.pending_load_slot_mut(offset, kind).as_mut() {
                            pending.phase = PendingLoadPhase::AwaitingWorker;
                        }
                        self.recovery_cursor.get_or_insert(offset);
                    } else {
                        if let Some(pending) = self.pending_load_slot_mut(offset, kind).as_mut() {
                            pending.phase = PendingLoadPhase::Failed;
                        }
                    }
                    self.pads[offset].state = PadLoadState::Error(message.clone());
                    true
                } else {
                    false
                }
            }
            WorkerRequest::ScanDirectory {
                request_id, path, ..
            } if self.file_picker.pending_directory() == Some(path.as_path()) => self
                .file_picker
                .apply_scan(request_id, Err(message.clone())),
            WorkerRequest::EditSample {
                pad,
                generation,
                recipe,
                ..
            } => {
                let offset = pad_offset(pad);
                let Some(pending) = self.sample_editor.pending[offset].as_mut() else {
                    return false;
                };
                if pending.generation != generation
                    || pending.recipe != recipe
                    || !matches!(pending.phase, PendingEditPhase::WorkerQueued)
                {
                    return false;
                }
                if error == WorkerSendError::WorkerBusy {
                    pending.phase = PendingEditPhase::AwaitingWorker;
                } else {
                    pending.phase = PendingEditPhase::Failed;
                    self.pads[offset].state = PadLoadState::Error(message.clone());
                    self.fail_project_sample_apply(pad);
                }
                true
            }
            WorkerRequest::SaveProject(request) => self.restore_busy_project_save(*request, error),
            WorkerRequest::DiscardRecovery {
                token,
                directory,
                project_id,
                revision,
            } => {
                if let Some(applied) = self.restore_busy_project_recovery_discard(
                    token, &directory, project_id, revision, error,
                ) {
                    applied
                } else {
                    self.restore_busy_recovery_cleanup(
                        RecoveryCleanup {
                            token,
                            directory,
                            project_id,
                            revision,
                        },
                        error,
                    )
                }
            }
            WorkerRequest::ProbeProject { token, directory } => {
                let Some(ProjectOpenOperation::Probing {
                    progress,
                    worker_queued,
                }) = self.project_open.as_mut()
                else {
                    return false;
                };
                if progress.token != token || progress.directory != directory {
                    return false;
                }
                *worker_queued = false;
                true
            }
            WorkerRequest::StageProjectSample(request) => {
                let Some(ProjectOpenOperation::Staging(candidate)) = self.project_open.as_mut()
                else {
                    return false;
                };
                if candidate.progress.token != request.token
                    || candidate.decode_in_flight != Some((request.pad, request.generation))
                    || candidate.stage_generation != request.generation
                    || candidate.document.revision != request.revision
                {
                    return false;
                }
                candidate.decode_in_flight = None;
                true
            }
            WorkerRequest::FinalizeCapture(request) => {
                let exact = self.capture_worker_request.as_ref().is_some_and(|owned| {
                    owned.token == request.token
                        && owned.generation == request.generation
                        && owned.target == request.target
                        && owned.source == request.source
                        && owned.source_rate == request.source_rate
                        && owned.engine_rate == request.engine_rate
                        && owned.hard_limit == request.hard_limit
                        && Arc::ptr_eq(&owned.stereo, &request.stereo)
                });
                if exact {
                    self.capture_worker_request = None;
                }
                exact
            }
            WorkerRequest::ReleaseManagedCapture { id }
                if self.managed_release_in_flight == Some(id) =>
            {
                self.managed_release_in_flight = None;
                self.pending_managed_releases.push_front(id);
                true
            }
            WorkerRequest::ReleaseManagedCapture { .. }
            | WorkerRequest::ScanDirectory { .. }
            | WorkerRequest::Shutdown => false,
        };
        if applied {
            self.status = message;
            if let Some(offset) = affected_offset {
                self.refresh_editor_for_offset(offset);
            }
        }
        applied
    }

    fn restore_busy_project_save(
        &mut self,
        request: ProjectSaveWorkerRequest,
        error: WorkerSendError,
    ) -> bool {
        let Some(InFlightProjectOperation::Save(save)) = self.in_flight_project.as_ref() else {
            return false;
        };
        let expected = &save.descriptor;
        if request.token != expected.token
            || request.request.kind != expected.kind
            || request.request.snapshot.project_id != expected.project_id
            || request.request.directory != expected.directory
            || request.request.snapshot.revision != expected.revision
        {
            return false;
        }
        let InFlightProjectOperation::Save(save) = self
            .in_flight_project
            .take()
            .expect("matching save operation is present")
        else {
            unreachable!()
        };
        self.project_session.set_in_flight(None);
        let save = *save;
        if error == WorkerSendError::WorkerBusy {
            match save.descriptor.kind {
                SaveKind::Explicit => self.pending_explicit_save = Some(save),
                SaveKind::Recovery => {
                    self.project_session
                        .set_pending_autosave(Some(crate::AutosaveDescriptor {
                            revision: save.descriptor.revision,
                        }));
                    self.pending_autosave_save = Some(save);
                }
            }
        } else {
            self.project_save_error = Some(ProjectSaveFailure {
                kind: save.descriptor.kind,
                error: ProjectStoreError::Filesystem {
                    operation: "send project save",
                    path: save.descriptor.directory,
                    kind: std::io::ErrorKind::BrokenPipe,
                },
            });
        }
        true
    }

    fn restore_busy_recovery_cleanup(
        &mut self,
        request: RecoveryCleanup,
        error: WorkerSendError,
    ) -> bool {
        let Some(InFlightProjectOperation::Cleanup(expected)) = self.in_flight_project.as_ref()
        else {
            return false;
        };
        if expected != &request {
            return false;
        }
        self.in_flight_project = None;
        if error == WorkerSendError::WorkerBusy {
            self.pending_recovery_cleanup.push_front(request);
        } else {
            self.recovery_cleanup_warning = Some(ProjectStoreError::Filesystem {
                operation: "send recovery cleanup",
                path: request.directory,
                kind: std::io::ErrorKind::BrokenPipe,
            });
        }
        true
    }

    pub fn device_retry_requests(&self) -> usize {
        self.device_retry_requests
    }

    pub fn take_device_retry_requests(&mut self) -> usize {
        mem::take(&mut self.device_retry_requests)
    }

    pub fn retry_default_device(&mut self) -> bool {
        self.retry_default_device_with(open_default_audio)
    }

    pub fn retry_with(&mut self, audio: Box<dyn AudioPort>) -> bool {
        self.recover_audio(audio);
        true
    }

    pub fn shutdown_audio(&mut self) -> Result<(), String> {
        self.audio_format = None;
        self.recovery_cursor = None;
        self.pending_loads.fill_with(|| None);
        self.committed_recovery_loads.fill_with(|| None);
        self.reinstall_pending.fill(false);
        self.current_session_bound.fill(false);
        self.held_pad_by_key.fill(None);
        self.midi_owned_pads.fill(None);
        for pad in &mut self.pads {
            pad.active = false;
        }
        let Some(mut audio) = self.audio.take() else {
            return Ok(());
        };
        let result = audio.stop_all();
        drop(audio);
        result
    }

    fn retry_default_device_with(
        &mut self,
        open_audio: impl FnOnce() -> Result<Box<dyn AudioPort>, String>,
    ) -> bool {
        match open_audio() {
            Ok(audio) => self.recover_audio(audio),
            Err(error) => self.fail_audio(error),
        }
        true
    }

    pub fn set_keyboard_capabilities(&mut self, capabilities: KeyboardCapabilities) {
        self.keyboard_capabilities = capabilities;
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn begin_load(&mut self, pad: PadId, path: impl Into<PathBuf>) -> Option<WorkerRequest> {
        if let Err(error) = self.ensure_project_mutation_available() {
            self.status = error;
            return None;
        }
        let path = path.into();
        let engine_rate = self.audio.as_ref().map(|audio| audio.sample_rate());
        let offset = pad_offset(pad);
        self.invalidate_pending_edit(offset);
        let view = &mut self.pads[offset];
        view.generation = view.generation.wrapping_add(1);
        view.state = if engine_rate.is_some() {
            PadLoadState::Loading
        } else {
            PadLoadState::WaitingForDevice
        };
        let generation = view.generation;

        let request = if let Some(engine_rate) = engine_rate {
            self.pending_loads[offset] = Some(Box::new(PendingLoad {
                path: path.clone(),
                phase: PendingLoadPhase::WorkerQueued,
                kind: PendingLoadKind::User,
            }));
            Some(WorkerRequest::LoadSample {
                pad,
                generation,
                purpose: LoadPurpose::User,
                path,
                engine_rate,
                recipe: SampleEditRecipe::identity(),
            })
        } else {
            self.pending_loads[offset] = Some(Box::new(PendingLoad {
                path,
                phase: PendingLoadPhase::AwaitingWorker,
                kind: PendingLoadKind::User,
            }));
            self.recovery_cursor = Some(offset);
            None
        };
        self.refresh_editor_for_offset(offset);
        request
    }

    /// Starts a bounded worker render of a new recipe. The pad tuple is not changed until the
    /// audio command accepts the rendered buffer.
    pub fn request_sample_edit(
        &mut self,
        pad: PadId,
        recipe: SampleEditRecipe,
    ) -> Result<(), SampleEditRequestError> {
        recipe
            .validate()
            .map_err(|error| SampleEditRequestError::InvalidRecipe(error.to_string()))?;
        let offset = pad_offset(pad);
        if self.audio.is_none() {
            return Err(SampleEditRequestError::AudioUnavailable(
                self.audio_unavailable_message
                    .clone()
                    .unwrap_or_else(|| "audio device is unavailable".to_owned()),
            ));
        }
        if self.pending_loads[offset].is_some() {
            return Err(SampleEditRequestError::LoadPending);
        }
        let Some(base) = self.sample_editor.commits[offset].base.as_ref().cloned() else {
            return Err(SampleEditRequestError::EmptyPad);
        };
        let Some(base_preview) = self.sample_editor.commits[offset]
            .base_preview
            .as_ref()
            .cloned()
        else {
            return Err(SampleEditRequestError::EmptyPad);
        };
        self.project_session
            .ensure_mutation_available()
            .map_err(|_| SampleEditRequestError::ProjectRevisionExhausted)?;
        self.start_sample_edit(offset, base, base_preview, recipe, PendingEditKind::Apply)
    }

    /// Re-renders and installs the previous recipe through the same worker/audio path as Apply.
    /// The checkpoint remains available until that replacement is admitted.
    pub fn undo_sample_edit(&mut self, pad: PadId) -> Result<(), SampleEditRequestError> {
        let offset = pad_offset(pad);
        if self.audio.is_none() {
            return Err(SampleEditRequestError::AudioUnavailable(
                self.audio_unavailable_message
                    .clone()
                    .unwrap_or_else(|| "audio device is unavailable".to_owned()),
            ));
        }
        let Some(checkpoint) = self.sample_editor.undo[offset].as_ref() else {
            return Err(SampleEditRequestError::NoUndo);
        };
        // At the same device rate the checkpoint's base is the exact prior tuple. After a
        // recovery it intentionally re-renders the old recipe from the newly decoded base.
        let _retained_prior_rendered = Arc::clone(&checkpoint.rendered);
        let _retained_prior_base_preview = Arc::clone(&checkpoint.base_preview);
        let _retained_prior_rendered_preview = Arc::clone(&checkpoint.rendered_preview);
        let (base, base_preview) = if self
            .audio
            .as_ref()
            .is_some_and(|audio| checkpoint.base.sample_rate() == audio.sample_rate())
        {
            (
                Arc::clone(&checkpoint.base),
                Arc::clone(&checkpoint.base_preview),
            )
        } else if let (Some(base), Some(base_preview)) = (
            self.sample_editor.commits[offset].base.as_ref(),
            self.sample_editor.commits[offset].base_preview.as_ref(),
        ) {
            (Arc::clone(base), Arc::clone(base_preview))
        } else {
            return Err(SampleEditRequestError::EmptyPad);
        };
        self.project_session
            .ensure_mutation_available()
            .map_err(|_| SampleEditRequestError::ProjectRevisionExhausted)?;
        self.start_sample_edit(
            offset,
            base,
            base_preview,
            checkpoint.recipe,
            PendingEditKind::Undo,
        )
    }

    pub fn committed_sample_recipe(&self, pad: PadId) -> Option<SampleEditRecipe> {
        let view = self.pad(pad);
        view.sample
            .as_ref()
            .map(|_| self.sample_editor.commits[pad_offset(pad)].recipe)
    }

    pub fn base_sample(&self, pad: PadId) -> Option<&Arc<SampleBuffer>> {
        self.sample_editor.commits[pad_offset(pad)].base.as_ref()
    }

    pub fn edit_preview(&self, pad: PadId) -> Option<&EditPreview> {
        self.sample_editor.commits[pad_offset(pad)]
            .base_preview
            .as_ref()
    }

    pub fn sample_edit_status(&self, pad: PadId) -> SampleEditStatus {
        let offset = pad_offset(pad);
        match self.sample_editor.pending[offset]
            .as_ref()
            .map(|pending| &pending.phase)
        {
            _ if self.sample_editor.generation_exhausted[offset] => {
                SampleEditStatus::GenerationExhausted
            }
            Some(PendingEditPhase::AwaitingWorker) => SampleEditStatus::AwaitingWorker,
            Some(PendingEditPhase::WorkerQueued) => SampleEditStatus::Rendering,
            Some(PendingEditPhase::Ready(_)) => SampleEditStatus::ReadyToInstall,
            Some(PendingEditPhase::Failed) => SampleEditStatus::Failed,
            None if self.sample_editor.undo[offset].is_some() => SampleEditStatus::UndoAvailable,
            None => SampleEditStatus::Idle,
        }
    }

    /// Read-only state for the Sample workspace. The editor intentionally cannot observe or
    /// manipulate generations, buffers, or worker queues through this projection.
    pub fn sample_editor_context(&self, pad: PadId) -> SampleEditorContext {
        let base = self.base_sample(pad);
        SampleEditorContext {
            pad,
            source_generation: self.sample_editor.commits[pad_offset(pad)].source_generation,
            committed: self.committed_sample_recipe(pad),
            base_frames: base.map(|sample| sample.frames()),
            base_rate: base.map(|sample| sample.sample_rate()),
            settings: self.pad(pad).settings,
            edit_status: self.sample_edit_status(pad),
            device_available: self.audio.is_some(),
        }
    }

    fn start_sample_edit(
        &mut self,
        offset: usize,
        base: Arc<SampleBuffer>,
        base_preview: EditPreview,
        recipe: SampleEditRecipe,
        kind: PendingEditKind,
    ) -> Result<(), SampleEditRequestError> {
        let sample_rate = self
            .audio
            .as_ref()
            .map(|audio| audio.sample_rate())
            .ok_or_else(|| {
                SampleEditRequestError::AudioUnavailable("audio device is unavailable".to_owned())
            })?;
        if base.sample_rate() != sample_rate {
            return Err(SampleEditRequestError::RecoveryPending);
        }
        let Some(generation) = self.sample_editor.generations[offset].checked_add(1) else {
            self.sample_editor.generation_exhausted[offset] = true;
            let error = SampleEditRequestError::GenerationExhausted;
            self.status = error.to_string();
            return Err(error);
        };
        self.sample_editor.generations[offset] = generation;
        self.sample_editor.generation_exhausted[offset] = false;
        self.sample_editor.deferred_results[offset] = None;
        self.sample_editor.pending[offset] = Some(Box::new(PendingEdit {
            generation,
            base: Arc::clone(&base),
            base_preview: Arc::clone(&base_preview),
            recipe,
            kind,
            phase: PendingEditPhase::WorkerQueued,
        }));
        self.pads[offset].state = PadLoadState::Loading;
        self.queue_worker_request(WorkerRequest::EditSample {
            pad: pad_from_offset(offset),
            generation,
            base,
            base_preview,
            recipe,
        });
        Ok(())
    }

    fn invalidate_pending_edit(&mut self, offset: usize) {
        self.sample_editor.generations[offset] =
            self.sample_editor.generations[offset].saturating_add(1);
        self.sample_editor.pending[offset] = None;
        self.sample_editor.deferred_results[offset] = None;
    }

    fn suspend_pending_sample_edits(&mut self) {
        for offset in 0..PAD_VIEW_COUNT {
            self.sample_editor.generations[offset] =
                self.sample_editor.generations[offset].saturating_add(1);
            self.sample_editor.deferred_results[offset] = None;
            if let Some(pending) = self.sample_editor.pending[offset].as_mut() {
                pending.phase = PendingEditPhase::Failed;
            }
        }
    }

    pub fn apply_worker_result(&mut self, result: WorkerResult) -> bool {
        let result = match result {
            result @ (WorkerResult::CaptureFinalized { .. }
            | WorkerResult::ManagedCaptureReleased { .. }) => {
                self.capture_worker_results.push_back(result);
                return true;
            }
            WorkerResult::ProjectProbed {
                token,
                directory,
                result,
            } => return self.apply_project_probe(token, directory, result),
            result @ WorkerResult::ProjectSampleStaged { .. } => {
                return self.apply_project_sample_staged(result);
            }
            WorkerResult::ProjectSaved {
                token,
                kind,
                project_id,
                directory,
                revision,
                result,
            } => {
                return self
                    .apply_project_saved(token, kind, project_id, directory, revision, result);
            }
            WorkerResult::RecoveryDiscarded {
                token,
                directory,
                project_id,
                revision,
                result,
            } => {
                if let Some(applied) = self.apply_project_recovery_discarded(
                    token,
                    directory.clone(),
                    project_id,
                    revision,
                    result.clone(),
                ) {
                    return applied;
                }
                return self.apply_recovery_cleanup(token, directory, project_id, revision, result);
            }
            result => result,
        };
        if let WorkerResult::Edited {
            pad,
            generation,
            recipe,
            result,
        } = result
        {
            if !self.pending_edit_matches(pad, generation, recipe) {
                return false;
            }
            if self.edit_result_advanced {
                let offset = pad_offset(pad);
                self.sample_editor.deferred_results[offset] =
                    Some(Box::new(WorkerResult::Edited {
                        pad,
                        generation,
                        recipe,
                        result,
                    }));
                return true;
            }
            self.edit_result_advanced = true;
            return self.apply_edited_worker_result(pad, generation, recipe, result);
        }
        let WorkerResult::Loaded {
            pad,
            generation,
            purpose,
            path,
            result,
        } = result
        else {
            let WorkerResult::Scanned {
                request_id,
                path,
                result,
            } = result
            else {
                return false;
            };
            if self.file_picker.pending_directory() != Some(path.as_path()) {
                return false;
            }
            let error = result.as_ref().err().cloned();
            let truncated = result.as_ref().is_ok_and(|scan| scan.truncated());
            let applied = self.file_picker.apply_scan(request_id, result);
            if applied {
                if let Some(error) = error {
                    self.status = error;
                } else if truncated {
                    self.status = format!(
                        "directory results limited to the first {MAX_DIRECTORY_ENTRIES} entries"
                    );
                }
            }
            return applied;
        };
        let offset = pad_offset(pad);
        let Some(kind) = self.matching_pending_load(offset, generation, purpose, &path) else {
            return false;
        };

        let loaded = match result {
            Ok(loaded) => loaded,
            Err(error) => {
                let error = error.to_string();
                if let Some(pending) = self.pending_load_slot_mut(offset, kind).as_mut() {
                    pending.phase = PendingLoadPhase::Failed;
                }
                self.pads[offset].state =
                    if self.current_session_bound[offset] && self.pads[offset].sample.is_some() {
                        PadLoadState::Ready
                    } else {
                        PadLoadState::Error(error.clone())
                    };
                self.status = error;
                self.refresh_editor_for_offset(offset);
                return true;
            }
        };

        *self.pending_load_slot_mut(offset, kind) = Some(Box::new(PendingLoad {
            path,
            phase: PendingLoadPhase::Ready(loaded),
            kind,
        }));
        let Some(sample_rate) = self.audio.as_ref().map(|audio| audio.sample_rate()) else {
            self.pads[offset].state = PadLoadState::WaitingForDevice;
            self.recovery_cursor = Some(offset);
            self.refresh_editor_for_offset(offset);
            return true;
        };
        if self
            .pending_load_slot(offset, kind)
            .as_ref()
            .and_then(|pending| match &pending.phase {
                PendingLoadPhase::Ready(loaded) => Some(loaded.rendered.sample_rate()),
                _ => None,
            })
            != Some(sample_rate)
        {
            if let Some(pending) = self.pending_load_slot_mut(offset, kind).as_mut() {
                pending.phase = PendingLoadPhase::AwaitingWorker;
            }
            self.pads[offset].state = PadLoadState::Loading;
            self.recovery_cursor = Some(offset);
            self.refresh_editor_for_offset(offset);
            return true;
        }
        self.install_pending_load(offset, kind);
        true
    }

    fn apply_project_saved(
        &mut self,
        token: ProjectToken,
        kind: SaveKind,
        project_id: ProjectId,
        directory: PathBuf,
        revision: u64,
        result: Result<SaveReceipt, ProjectStoreError>,
    ) -> bool {
        let Some(InFlightProjectOperation::Save(save)) = self.in_flight_project.as_ref() else {
            return false;
        };
        let expected = &save.descriptor;
        if expected.token != token
            || expected.kind != kind
            || expected.project_id != project_id
            || expected.directory != directory
            || expected.revision != revision
        {
            return false;
        }
        if let Ok(receipt) = &result
            && (receipt.kind != kind
                || receipt.project_id != project_id
                || receipt.revision != revision)
        {
            return false;
        }
        let save_succeeded = result.is_ok();

        let InFlightProjectOperation::Save(save) = self
            .in_flight_project
            .take()
            .expect("matching save operation is present")
        else {
            unreachable!()
        };
        self.project_session.set_in_flight(None);
        match result {
            Err(error) => {
                self.status = error.to_string();
                self.project_save_error = Some(ProjectSaveFailure { kind, error });
                if kind == SaveKind::Recovery {
                    self.autosave_retry_clock_pending = true;
                    self.autosave_retry_since = None;
                }
            }
            Ok(receipt) => {
                self.apply_project_asset_mappings(&save.snapshot, &receipt);
                self.project_save_error = None;
                self.autosave_retry_clock_pending = false;
                self.autosave_retry_since = None;
                match kind {
                    SaveKind::Explicit => {
                        if self
                            .pending_autosave_save
                            .as_ref()
                            .is_some_and(|pending| pending.descriptor.revision <= revision)
                        {
                            self.pending_autosave_save = None;
                            self.project_session.set_pending_autosave(None);
                        }
                        if save.save_as {
                            self.project_session.adopt_saved_project(
                                project_id,
                                receipt.directory.clone(),
                                save.snapshot.name,
                                revision,
                            );
                            self.save_as_identity = None;
                        } else {
                            self.project_session.mark_explicit_saved(revision);
                        }
                        if let Ok(cleanup_token) = self.allocate_project_token() {
                            debug_assert!(
                                self.pending_recovery_cleanup.len() < WORKER_CHANNEL_CAPACITY
                            );
                            self.pending_recovery_cleanup.push_back(RecoveryCleanup {
                                token: cleanup_token,
                                directory: receipt.directory,
                                project_id,
                                revision: self.project_session.autosaved_revision(),
                            });
                        }
                    }
                    SaveKind::Recovery => self.project_session.mark_autosaved(revision),
                }
            }
        }
        let lifecycle_save_revision = match self.project_lifecycle_wait {
            Some(ProjectLifecycleWait::Saving {
                token: expected,
                action_revision,
            }) if expected == token => Some(action_revision),
            _ => None,
        };
        if kind == SaveKind::Explicit
            && let Some(action_revision) = lifecycle_save_revision
        {
            if save_succeeded {
                if self.project_session.current_revision() == action_revision {
                    self.complete_project_action();
                } else {
                    self.reconfirm_project_action(
                        "Project changed while saving; review the newer changes",
                    );
                }
            } else {
                let error = self.status.clone();
                self.reconfirm_project_action(&error);
            }
        } else if kind == SaveKind::Explicit
            && matches!(self.overlay, Some(Overlay::ProjectSaveProgress))
        {
            self.overlay = if save_succeeded {
                None
            } else {
                Some(Overlay::ProjectError {
                    title: "SAVE PROJECT ERROR".to_owned(),
                    message: self.status.clone(),
                })
            };
        }
        true
    }

    fn apply_project_asset_mappings(
        &mut self,
        snapshot: &ProjectSaveSnapshot,
        receipt: &SaveReceipt,
    ) {
        for mapping in &receipt.mappings {
            let Some(saved_pad) = snapshot.pads.iter().find(|pad| {
                pad.pad == mapping.pad
                    && pad.source_generation == mapping.source_generation
                    && pad.fingerprint == mapping.fingerprint
            }) else {
                continue;
            };
            let offset = pad_offset(saved_pad.pad);
            if self.sample_editor.commits[offset].source_generation == mapping.source_generation
                && self.sample_editor.commits[offset].fingerprint == Some(mapping.fingerprint)
            {
                self.pads[offset].source = Some(mapping.project_path.clone());
                if receipt.kind == SaveKind::Explicit {
                    self.retire_managed_capture_at(offset);
                }
            }
        }
    }

    fn apply_recovery_cleanup(
        &mut self,
        token: ProjectToken,
        directory: PathBuf,
        project_id: ProjectId,
        revision: u64,
        result: Result<(), ProjectStoreError>,
    ) -> bool {
        let Some(InFlightProjectOperation::Cleanup(cleanup)) = self.in_flight_project.as_ref()
        else {
            return false;
        };
        if cleanup.token != token
            || cleanup.directory != directory
            || cleanup.project_id != project_id
            || cleanup.revision != revision
        {
            return false;
        }
        let lifecycle_action_revision = match self.project_lifecycle_wait.as_ref() {
            Some(ProjectLifecycleWait::DiscardingRecovery {
                cleanup: expected,
                action_revision,
            }) if expected == cleanup => Some(*action_revision),
            _ => None,
        };
        let cleanup_succeeded = result.is_ok();
        self.in_flight_project = None;
        self.recovery_cleanup_warning = result.err();
        if cleanup_succeeded
            && self.project_session.project_id() == project_id
            && self.project_session.directory() == Some(directory.as_path())
            && self.project_session.autosaved_revision() == revision
        {
            self.project_session
                .mark_autosaved(self.project_session.saved_revision());
        }
        if let Some(action_revision) = lifecycle_action_revision {
            if cleanup_succeeded {
                if self.project_session.current_revision() == action_revision {
                    self.complete_project_action();
                } else {
                    self.reconfirm_project_action(
                        "Project changed while discarding recovery; choose how to continue",
                    );
                }
            } else {
                self.reconfirm_project_action("Recovery discard failed; choose how to continue");
                if let Some(error) = &self.recovery_cleanup_warning {
                    self.status = error.to_string();
                }
            }
        }
        true
    }

    fn apply_edited_worker_result(
        &mut self,
        pad: PadId,
        generation: u64,
        recipe: SampleEditRecipe,
        result: Result<RenderedSample, String>,
    ) -> bool {
        let offset = pad_offset(pad);
        let Some(pending) = self.sample_editor.pending[offset].as_mut() else {
            return false;
        };
        if pending.generation != generation
            || pending.recipe != recipe
            || !matches!(pending.phase, PendingEditPhase::WorkerQueued)
        {
            return false;
        }
        match result {
            Ok(rendered) => pending.phase = PendingEditPhase::Ready(rendered),
            Err(error) => {
                pending.phase = PendingEditPhase::Failed;
                self.pads[offset].state = PadLoadState::Error(error.clone());
                self.status = error;
                self.fail_project_sample_apply(pad);
            }
        }
        self.refresh_editor_for_offset(offset);
        true
    }

    fn press_pad(&mut self, index: usize) {
        self.trigger_pad(index, true);
    }

    fn trigger_pad(&mut self, index: usize, track_physical_hold: bool) {
        if self.held_pad_by_key.get(index).is_some_and(Option::is_some) {
            return;
        }
        let Some(pad) = self.pad_in_active_bank(index) else {
            return;
        };
        let _ = self.select_pad(index);
        if self.patterns.view() == WorkspaceView::Pattern {
            let step = self.patterns.cursor().step();
            self.patterns.move_cursor_to(pad, step);
        }
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        let recording = self.patterns.is_recording();
        let records_duration = self.pads[pad_offset(pad)].settings.mode != PlaybackMode::OneShot;
        let result = if recording {
            audio.trigger_live_tracked(pad, 1.0).map(Some)
        } else {
            audio.trigger_live(pad, 1.0).map(|()| None)
        };
        match result {
            Ok(command)
                if track_physical_hold
                    && (self.keyboard_capabilities.release_events
                        || self.pads[pad_offset(pad)].settings.mode != PlaybackMode::OneShot) =>
            {
                self.held_pad_by_key[index] = Some(pad);
                if let Some(command) = command {
                    self.patterns.note_live_trigger_with_duration(
                        index,
                        command,
                        pad,
                        1.0,
                        records_duration,
                    );
                }
            }
            Ok(command) => {
                if let Some(command) = command {
                    self.patterns.note_live_trigger_with_duration(
                        index,
                        command,
                        pad,
                        1.0,
                        records_duration,
                    );
                }
            }
            Err(error) => self.status = error,
        }
    }

    fn apply_device_error_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press
            && key.modifiers == KeyModifiers::NONE
            && matches!(key.code, KeyCode::Char('r' | 'R'))
        {
            self.device_retry_requests = self.device_retry_requests.saturating_add(1);
        }
    }

    fn apply_project_open_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self
            .project_open_stage()
            .is_none_or(|stage| stage.phase != ProjectOpenPhase::AwaitingRecoveryChoice)
        {
            return;
        }
        let choice = match key.code {
            KeyCode::Char('r' | 'R') => RecoveryChoice::Restore,
            KeyCode::Char('d' | 'D') => RecoveryChoice::Discard,
            KeyCode::Char('c' | 'C') => RecoveryChoice::Cancel,
            _ => return,
        };
        if let Err(error) = self.choose_project_recovery(choice) {
            self.status = error.to_string();
        }
    }

    fn apply_project_error_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press && matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
            self.overlay = None;
        }
    }

    fn apply_palette_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let text_changed = match key.code {
            KeyCode::Enter => {
                self.execute_palette();
                false
            }
            KeyCode::Left => {
                self.palette.move_left();
                false
            }
            KeyCode::Right => {
                self.palette.move_right();
                false
            }
            KeyCode::Home => {
                self.palette.move_home();
                false
            }
            KeyCode::End => {
                self.palette.move_end();
                false
            }
            KeyCode::Backspace => {
                let prior_len = self.palette.text().len();
                self.palette.backspace();
                self.palette.text().len() != prior_len
            }
            KeyCode::Delete => {
                let prior_len = self.palette.text().len();
                self.palette.delete();
                self.palette.text().len() != prior_len
            }
            KeyCode::Char(character)
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.palette.insert(character);
                true
            }
            _ => return,
        };
        if text_changed {
            self.palette_error = None;
        }
    }

    fn execute_palette(&mut self) {
        let command = match parse_palette(self.palette.text()) {
            Ok(command) => command,
            Err(error) => {
                self.palette_error = Some(error);
                return;
            }
        };
        self.palette_error = None;
        match command {
            PaletteCommand::OpenPicker => self.open_picker(),
            PaletteCommand::LoadPath(path) => {
                self.begin_selected_load(path);
                self.overlay = None;
            }
            PaletteCommand::Save => match self.request_save() {
                Ok(()) => {
                    self.overlay = Some(Overlay::ProjectSaveProgress);
                    self.status = "Saving project…".to_owned();
                }
                Err(ProjectSaveError::Untitled) => {
                    self.overlay = None;
                    self.status = "Untitled project: use save-as <directory>".to_owned();
                }
                Err(error) => {
                    self.overlay = None;
                    self.status = error.to_string();
                }
            },
            PaletteCommand::SaveAs(directory) => match self.request_save_as(directory) {
                Ok(()) => {
                    self.overlay = Some(Overlay::ProjectSaveProgress);
                    self.status = "Saving project as a new directory…".to_owned();
                }
                Err(error) => {
                    self.overlay = None;
                    self.status = error.to_string();
                }
            },
            PaletteCommand::OpenProject(directory) => {
                self.request_open_project_interactive(directory);
            }
            PaletteCommand::Bank(bank) => {
                if self.editor.is_dirty() {
                    self.status = "discard sample draft before changing bank".to_owned();
                    return;
                }
                self.active_bank = bank;
                self.sync_editor_to_selected_pad();
                self.overlay = None;
            }
            PaletteCommand::Select(index) => {
                if self.select_pad(index) {
                    self.overlay = None;
                }
            }
            PaletteCommand::StopAll => {
                self.stop_all();
                self.overlay = None;
            }
            PaletteCommand::MidiChannel(channel) => {
                let result = self.update_midi_channel(channel);
                self.finish_midi_palette_command(result);
            }
            PaletteCommand::MidiLearn => {
                self.arm_midi_learn();
                self.overlay = None;
            }
            PaletteCommand::MidiUnmap => {
                let result = self.unmap_selected_midi();
                self.finish_midi_palette_command(result);
            }
            PaletteCommand::MidiResetBank => {
                let result = self.reset_active_midi_bank();
                self.finish_midi_palette_command(result);
            }
            PaletteCommand::MidiPorts => {
                let result = self.list_midi_ports();
                self.finish_midi_palette_command(result);
            }
            PaletteCommand::MidiConnect(index) => {
                let result = self.connect_midi_port(index);
                self.finish_midi_palette_command(result);
            }
            PaletteCommand::MidiDisconnect => {
                let result = self.disconnect_midi_port();
                self.finish_midi_palette_command(result);
            }
            PaletteCommand::Help => self.open_help(),
            PaletteCommand::Quit => {
                self.begin_project_action(PendingProjectAction::Quit);
            }
            PaletteCommand::Pattern(slot) => self.select_pattern_slot(usize::from(slot)),
            PaletteCommand::Tempo(tempo) => self.apply_pattern_edit(|patterns| {
                patterns
                    .set_tempo(sampler_core::Tempo::new(tempo).expect("palette validated tempo"))
            }),
            PaletteCommand::Bars(bars) => {
                self.apply_pattern_edit(|patterns| patterns.set_bars(bars))
            }
            PaletteCommand::Resolution(resolution) => {
                self.apply_pattern_edit(|patterns| patterns.set_resolution(resolution))
            }
            PaletteCommand::Swing(swing) => {
                self.apply_pattern_edit(|patterns| patterns.set_swing(swing))
            }
            PaletteCommand::Quantize(strength) => {
                self.apply_pattern_edit(|patterns| patterns.set_quantize(strength))
            }
            PaletteCommand::Record => self.toggle_pattern_recording(),
            PaletteCommand::Play => self.start_pattern_playback(),
            PaletteCommand::Stop => self.stop_pattern_playback(),
            PaletteCommand::ClearPattern => self.open_clear_pattern(),
            PaletteCommand::TrimStart(frame) => self.apply_palette_sample(|editor| {
                editor.set_marker_to_frame(SampleMarker::Start, frame)
            }),
            PaletteCommand::TrimEnd(frame) => self.apply_palette_sample(|editor| {
                editor.set_marker_to_frame(SampleMarker::End, frame)
            }),
            PaletteCommand::Normalize(enabled) => self.apply_palette_sample(|editor| {
                if editor.draft().normalize != enabled {
                    editor.toggle_normalize();
                }
                Ok(())
            }),
            PaletteCommand::Reverse(enabled) => self.apply_palette_sample(|editor| {
                if editor.draft().reversed != enabled {
                    editor.toggle_reverse();
                }
                Ok(())
            }),
            PaletteCommand::Pitch(pitch) => self.apply_palette_editor_settings(|settings| {
                settings.pitch_semitones = pitch;
            }),
            PaletteCommand::Mode(mode) => self.apply_palette_editor_settings(|settings| {
                settings.mode = mode;
            }),
            PaletteCommand::ApplySample => {
                if self.require_sample_workspace() {
                    self.request_editor_apply();
                }
            }
            PaletteCommand::UndoSample => {
                if self.require_sample_workspace() {
                    self.request_editor_undo();
                }
            }
            PaletteCommand::Resample => {
                let result = self.request_resample();
                self.finish_capture_palette_command(result);
            }
            PaletteCommand::RecordInput => {
                let result = self.request_input_recording();
                self.finish_capture_palette_command(result);
            }
            PaletteCommand::CaptureStop => {
                let result = self.stop_capture();
                self.finish_capture_palette_command(result);
            }
            PaletteCommand::CaptureCancel => {
                let result = self.cancel_capture();
                self.finish_capture_palette_command(result);
            }
            PaletteCommand::PadLevel(value) => {
                self.apply_palette_pad_settings(|settings| settings.gain_db = value)
            }
            PaletteCommand::PadPan(value) => {
                self.apply_palette_pad_settings(|settings| settings.pan = value)
            }
            PaletteCommand::PadMute(value) => {
                self.apply_palette_pad_mix(|settings| settings.muted = value)
            }
            PaletteCommand::PadChoke(value) => {
                self.apply_palette_pad_settings(|settings| settings.choke_group = value)
            }
            PaletteCommand::DelaySend(value) => {
                self.apply_palette_pad_mix(|settings| settings.delay_send = value)
            }
            PaletteCommand::ReverbSend(value) => {
                self.apply_palette_pad_mix(|settings| settings.reverb_send = value)
            }
            PaletteCommand::MasterLevel(value) => {
                self.apply_palette_master_mix(|settings| settings.gain_db = value)
            }
            PaletteCommand::DelayEnable(value) => {
                self.apply_palette_master_mix(|settings| settings.delay.enabled = value)
            }
            PaletteCommand::DelayTime(value) => {
                self.apply_palette_master_mix(|settings| settings.delay.time_ms = value)
            }
            PaletteCommand::DelayFeedback(value) => {
                self.apply_palette_master_mix(|settings| settings.delay.feedback = value)
            }
            PaletteCommand::DelayReturn(value) => {
                self.apply_palette_master_mix(|settings| settings.delay.return_db = value)
            }
            PaletteCommand::ReverbEnable(value) => {
                self.apply_palette_master_mix(|settings| settings.reverb.enabled = value)
            }
            PaletteCommand::ReverbRoom(value) => {
                self.apply_palette_master_mix(|settings| settings.reverb.room_size = value)
            }
            PaletteCommand::ReverbDamping(value) => {
                self.apply_palette_master_mix(|settings| settings.reverb.damping = value)
            }
            PaletteCommand::ReverbReturn(value) => {
                self.apply_palette_master_mix(|settings| settings.reverb.return_db = value)
            }
        }
    }

    fn apply_palette_pad_settings(&mut self, reduce: impl FnOnce(&mut PadSettings)) {
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        let mut settings = self.pads[pad_offset(pad)].settings;
        reduce(&mut settings);
        let result = self.update_pad_settings(pad, settings);
        if result.is_ok() {
            self.sync_editor_to_selected_pad();
        }
        self.finish_mixer_palette_command(result);
    }

    fn finish_midi_palette_command(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.overlay = None,
            Err(error) => self.palette_error = Some(error),
        }
    }

    fn apply_palette_pad_mix(&mut self, reduce: impl FnOnce(&mut PadMixSettings)) {
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        let mut settings = self.pad_mixes[pad_offset(pad)];
        reduce(&mut settings);
        let result = self.update_pad_mix(pad, settings);
        self.finish_mixer_palette_command(result);
    }

    fn apply_palette_master_mix(&mut self, reduce: impl FnOnce(&mut MasterMixSettings)) {
        let mut settings = self.master_mix;
        reduce(&mut settings);
        let result = self.update_master_mix(settings);
        self.finish_mixer_palette_command(result);
    }

    fn finish_mixer_palette_command(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.overlay = None,
            Err(error) => self.palette_error = Some(error),
        }
    }

    fn finish_capture_palette_command(&mut self, result: Result<(), CaptureError>) {
        match result {
            Ok(()) => {
                if self.overlay == Some(Overlay::Palette) {
                    self.overlay = None;
                }
            }
            Err(error) => self.palette_error = Some(error.to_string()),
        }
    }

    fn apply_palette_sample(
        &mut self,
        reduce: impl FnOnce(&mut SampleEditor) -> Result<(), String>,
    ) {
        if !self.require_sample_workspace() {
            return;
        }
        if let Err(error) = reduce(&mut self.editor) {
            self.palette_error = Some(error);
        }
    }

    fn apply_palette_editor_settings(&mut self, reduce: impl FnOnce(&mut PadSettings)) {
        if !self.require_sample_workspace() {
            return;
        }
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        let mut settings = self.editor.settings();
        reduce(&mut settings);
        match self.update_pad_settings(pad, settings) {
            Ok(()) => self.sync_editor_to_selected_pad(),
            Err(error) => self.palette_error = Some(error),
        }
    }

    fn require_sample_workspace(&mut self) -> bool {
        if self.patterns.view() != WorkspaceView::Sample {
            self.palette_error = Some("sample command requires Sample workspace".to_owned());
            return false;
        }
        if self.editor.committed().is_none() {
            self.palette_error = Some("selected pad is empty".to_owned());
            return false;
        }
        if self.editor_operation_pending() {
            self.palette_error = Some("sample edit is pending".to_owned());
            return false;
        }
        if !self.editor.can_edit() {
            self.palette_error =
                Some("sample editor context must be discarded before editing".to_owned());
            return false;
        }
        true
    }

    fn require_sample_editor_key(&mut self) -> bool {
        if self.editor.committed().is_none() {
            self.status = "selected pad is empty".to_owned();
            return false;
        }
        if self.editor_operation_pending() {
            self.status = "sample edit is pending".to_owned();
            return false;
        }
        if !self.editor.can_edit() {
            return false;
        }
        true
    }

    fn apply_picker_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Up => self.file_picker.move_cursor(-1),
            KeyCode::Down => self.file_picker.move_cursor(1),
            KeyCode::Home => self.file_picker.select_first(),
            KeyCode::End => self.file_picker.select_last(),
            KeyCode::Backspace => self.open_picker_parent(),
            KeyCode::Char('.') if key.modifiers == KeyModifiers::NONE => {
                let request_id = self.file_picker.toggle_hidden();
                self.queue_current_picker_scan(request_id);
            }
            KeyCode::Enter => self.open_picker_selection(),
            _ => {}
        }
    }

    fn apply_help_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press
            && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
            && key.code == KeyCode::Char('?')
        {
            self.overlay = None;
        }
    }

    fn apply_workspace_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press && self.apply_global_pattern_key(key) {
            return;
        }
        match self.patterns.view() {
            WorkspaceView::Perform => self.apply_perform_key(key),
            WorkspaceView::Pattern => self.apply_pattern_key(key),
            WorkspaceView::Sample => self.apply_sample_key(key),
            WorkspaceView::Mixer => self.apply_mixer_key(key),
        }
    }

    fn apply_global_pattern_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Tab | KeyCode::BackTab
                if matches!(
                    (key.code, key.modifiers),
                    (KeyCode::Tab, KeyModifiers::NONE | KeyModifiers::SHIFT)
                        | (KeyCode::BackTab, KeyModifiers::SHIFT)
                ) =>
            {
                if self.patterns.view() == WorkspaceView::Sample && self.editor_operation_pending()
                {
                    return true;
                }
                let view = if key.code == KeyCode::BackTab || key.modifiers == KeyModifiers::SHIFT {
                    self.patterns.view().previous()
                } else {
                    self.patterns.view().next()
                };
                if self.patterns.view() == WorkspaceView::Sample
                    && view != WorkspaceView::Sample
                    && self.editor.is_dirty()
                {
                    self.overlay = Some(Overlay::DiscardSample {
                        pad: self.editor.pad(),
                    });
                    return true;
                }
                self.patterns.set_view(view);
                if view == WorkspaceView::Sample {
                    self.sync_editor_to_selected_pad();
                }
                true
            }
            KeyCode::Char(' ') if key.modifiers == KeyModifiers::NONE => {
                if self.pattern_transport_is_playing() {
                    self.stop_pattern_playback();
                } else {
                    self.start_pattern_playback();
                }
                true
            }
            KeyCode::Char('r' | 'R') if is_explicit_device_retry(key) => {
                self.toggle_pattern_recording();
                true
            }
            KeyCode::Char(',') if key.modifiers == KeyModifiers::NONE => {
                self.change_pattern_slot(-1);
                true
            }
            KeyCode::Char('.') if key.modifiers == KeyModifiers::NONE => {
                self.change_pattern_slot(1);
                true
            }
            _ => false,
        }
    }

    fn apply_perform_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press {
            match key.code {
                KeyCode::Char('?')
                    if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
                {
                    self.open_help();
                }
                KeyCode::Char(':')
                    if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
                {
                    self.open_palette();
                }
                KeyCode::Char('l') if key.modifiers == KeyModifiers::NONE => {
                    self.open_picker();
                }
                KeyCode::Left => self.move_selection(-1, 0),
                KeyCode::Right => self.move_selection(1, 0),
                KeyCode::Up => self.move_selection(0, -1),
                KeyCode::Down => self.move_selection(0, 1),
                KeyCode::Enter => self.trigger_pad(self.selected_pad, false),
                _ => {
                    if let Some(action) = map_key(key, self.keyboard_capabilities) {
                        self.apply(action);
                    }
                }
            }
        } else if let Some(action) = map_key(key, self.keyboard_capabilities) {
            self.apply(action);
        }
    }

    fn apply_mixer_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let action = match (key.code, key.modifiers) {
            (KeyCode::Char('?'), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.open_help();
                return;
            }
            (KeyCode::Char(':'), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.open_palette();
                return;
            }
            (KeyCode::Left, KeyModifiers::CONTROL) => MixerAction::PreviousSection,
            (KeyCode::Right, KeyModifiers::CONTROL) => MixerAction::NextSection,
            (KeyCode::Up, KeyModifiers::NONE) => MixerAction::PreviousField,
            (KeyCode::Down, KeyModifiers::NONE) => MixerAction::NextField,
            (KeyCode::Left, KeyModifiers::NONE) => MixerAction::Decrement,
            (KeyCode::Right, KeyModifiers::NONE) => MixerAction::Increment,
            (KeyCode::Enter, KeyModifiers::NONE) => MixerAction::Activate,
            (KeyCode::Backspace, KeyModifiers::NONE) => MixerAction::Reset,
            (KeyCode::Esc, KeyModifiers::NONE) => MixerAction::ReturnToPerform,
            _ => return,
        };
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        let context = MixerContext {
            pad,
            pad_settings: self.pads[pad_offset(pad)].settings,
            pad_mix: self.pad_mixes[pad_offset(pad)],
            master_mix: self.master_mix,
        };
        let intent = self.mixer_cursor.reduce(action, context);
        if let Some(intent) = intent {
            self.apply_mixer_intent(intent);
        }
    }

    fn apply_mixer_intent(&mut self, intent: MixerIntent) {
        let result = match intent {
            MixerIntent::UpdatePadSettings { pad, settings } => {
                let result = self.update_pad_settings(pad, settings);
                if result.is_ok() {
                    self.sync_editor_to_selected_pad();
                }
                result
            }
            MixerIntent::UpdatePadMix { pad, settings } => self.update_pad_mix(pad, settings),
            MixerIntent::UpdateMasterMix(settings) => self.update_master_mix(settings),
            MixerIntent::ReturnToPerform => {
                self.patterns.set_view(WorkspaceView::Perform);
                Ok(())
            }
        };
        if let Err(error) = result {
            self.status = error;
        }
    }

    fn apply_pattern_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            if let Some(action) = map_key(key, self.keyboard_capabilities) {
                self.apply(action);
            }
            return;
        }
        match key.code {
            KeyCode::Char('?')
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.open_help();
            }
            KeyCode::Char(':')
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.open_palette();
            }
            KeyCode::Left => self.patterns.move_cursor_steps(-1),
            KeyCode::Right => self.patterns.move_cursor_steps(1),
            KeyCode::Up => self.move_pattern_cursor_pad(-1),
            KeyCode::Down => self.move_pattern_cursor_pad(1),
            KeyCode::PageUp => self.patterns.move_cursor_bar(-1),
            KeyCode::PageDown => self.patterns.move_cursor_bar(1),
            KeyCode::Enter => self.apply_pattern_edit(|patterns| patterns.toggle_step()),
            KeyCode::Delete if key.modifiers == KeyModifiers::CONTROL => self.open_clear_pattern(),
            KeyCode::Delete => self.apply_pattern_edit(|patterns| patterns.delete_step()),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.apply_pattern_edit(|patterns| patterns.adjust_velocity(0.05))
            }
            KeyCode::Char('-') => {
                self.apply_pattern_edit(|patterns| patterns.adjust_velocity(-0.05))
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::NONE => {
                self.apply_pattern_edit(|patterns| patterns.undo_clear())
            }
            _ => {
                if let Some(action) = map_key(key, self.keyboard_capabilities) {
                    self.apply(action);
                }
            }
        }
    }

    fn apply_sample_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.editor_operation_pending() && key.code == KeyCode::Esc {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key
                .modifiers
                .difference(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                .is_empty()
            && matches!(key.code, KeyCode::Char('z' | 'Z'))
        {
            if !self.require_sample_editor_key() {
                return;
            }
            self.request_editor_undo();
            return;
        }
        if matches!(
            key.code,
            KeyCode::Left
                | KeyCode::Right
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Enter
                | KeyCode::Char('m' | 'n' | 'u' | 'o' | 'g' | 'l')
        ) && !self.require_sample_editor_key()
        {
            return;
        }
        match key.code {
            KeyCode::Char('?')
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.open_help()
            }
            KeyCode::Char(':')
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.open_palette()
            }
            KeyCode::Left => self
                .editor
                .move_marker(-1, key.modifiers == KeyModifiers::SHIFT),
            KeyCode::Right => self
                .editor
                .move_marker(1, key.modifiers == KeyModifiers::SHIFT),
            KeyCode::PageUp if key.modifiers == KeyModifiers::NONE => self.editor.zoom_in(),
            KeyCode::PageDown if key.modifiers == KeyModifiers::NONE => self.editor.zoom_out(),
            KeyCode::Char('m') if key.modifiers == KeyModifiers::NONE => {
                self.editor.set_marker(match self.editor.marker() {
                    SampleMarker::Start => SampleMarker::End,
                    SampleMarker::End => SampleMarker::Start,
                });
            }
            KeyCode::Char('n') if key.modifiers == KeyModifiers::NONE => {
                self.editor.toggle_normalize()
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::NONE => {
                self.editor.toggle_reverse()
            }
            KeyCode::Up if key.modifiers == KeyModifiers::NONE => self.adjust_editor_pitch(1),
            KeyCode::Down if key.modifiers == KeyModifiers::NONE => self.adjust_editor_pitch(-1),
            KeyCode::Char('o') if key.modifiers == KeyModifiers::NONE => {
                self.set_editor_mode(PlaybackMode::OneShot)
            }
            KeyCode::Char('g') if key.modifiers == KeyModifiers::NONE => {
                self.set_editor_mode(PlaybackMode::Gate)
            }
            KeyCode::Char('l') if key.modifiers == KeyModifiers::NONE => {
                self.set_editor_mode(PlaybackMode::Loop)
            }
            KeyCode::Enter if key.modifiers == KeyModifiers::NONE => self.request_editor_apply(),
            KeyCode::Esc if key.modifiers == KeyModifiers::NONE => self.request_editor_escape(),
            _ => {}
        }
    }

    fn sync_editor_to_selected_pad(&mut self) {
        if let Some(pad) = self.selected_pad_id() {
            let context = self.sample_editor_context(pad);
            self.editor.sync_context(context);
        }
    }

    fn refresh_editor_for_offset(&mut self, offset: usize) {
        if self.selected_pad_id() == Some(pad_from_offset(offset)) {
            self.sync_editor_to_selected_pad();
        }
    }

    fn request_editor_apply(&mut self) {
        if self.editor_operation_pending() {
            return;
        }
        let retry_error = matches!(
            self.editor.status(),
            crate::sample_editor::SampleEditorStatus::Error(_)
        );
        if retry_error && self.pending_loads[pad_offset(self.editor.pad())].is_some() {
            return;
        }
        let Some(SampleEditorIntent::Apply { pad, recipe }) = self.editor.request_apply() else {
            return;
        };
        let Some(frames) = self.base_sample(pad).map(|sample| sample.frames()) else {
            self.editor
                .observe_apply_failed(SampleEditorError::SelectedPadReplaced);
            return;
        };
        let before_frames = self
            .committed_sample_recipe(pad)
            .and_then(|before| before.frame_range(frames).ok())
            .map_or(0, |range| range.len());
        let after_frames = recipe.frame_range(frames).map_or(0, |range| range.len());
        let Some(base_rate) = self.base_sample(pad).map(|sample| sample.sample_rate()) else {
            self.editor
                .observe_apply_failed(SampleEditorError::SelectedPadReplaced);
            return;
        };
        self.apply_sample_context = Some(ApplySampleContext {
            pad,
            pad_generation: self.pads[pad_offset(pad)].generation,
            source: self.pads[pad_offset(pad)].source.clone(),
            base_frames: frames,
            base_rate,
        });
        self.overlay = Some(Overlay::ApplySample {
            pad,
            before_frames,
            after_frames,
        });
    }

    fn request_editor_undo(&mut self) {
        if self.editor_operation_pending() {
            return;
        }
        if let Some(SampleEditorIntent::Undo { pad }) = self.editor.request_undo()
            && let Err(error) = self.undo_sample_edit(pad)
        {
            self.status = error.to_string();
            self.editor
                .observe_undo_failed(SampleEditorError::InstallFailed);
        }
    }

    fn request_editor_escape(&mut self) {
        match self.editor.escape() {
            SampleEditorIntent::ReturnToPerform => self.patterns.set_view(WorkspaceView::Perform),
            SampleEditorIntent::ConfirmDiscard { pad } => {
                self.overlay = Some(Overlay::DiscardSample { pad })
            }
            _ => {}
        }
    }

    fn apply_sample_apply_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press || key.code != KeyCode::Enter {
            return;
        }
        let Some(Overlay::ApplySample { pad, .. }) = self.overlay.clone() else {
            return;
        };
        let selected = self.selected_pad_id();
        let context_matches = self.apply_sample_context.as_ref().is_some_and(|context| {
            context.pad == pad
                && context.pad_generation == self.pads[pad_offset(pad)].generation
                && context.source == self.pads[pad_offset(pad)].source
                && self.base_sample(pad).is_some_and(|base| {
                    base.frames() == context.base_frames && base.sample_rate() == context.base_rate
                })
        });
        if selected != Some(pad)
            || self.editor.pad() != pad
            || !context_matches
            || self.editor_operation_pending()
        {
            if matches!(
                self.editor.status(),
                crate::sample_editor::SampleEditorStatus::ApplyConfirmation
            ) {
                self.editor
                    .observe_apply_failed(SampleEditorError::SelectedPadReplaced);
            }
            self.status = "sample changed while apply confirmation was open".to_owned();
            self.reject_apply_confirmation();
            return;
        }
        let Some(SampleEditorIntent::Apply { recipe, .. }) = self.editor.confirm_apply() else {
            self.reject_apply_confirmation();
            return;
        };
        match self.request_sample_edit(pad, recipe) {
            Ok(()) => {
                self.editor.observe_pending();
                self.overlay = None;
                self.apply_sample_context = None;
            }
            Err(error) => {
                self.status = error.to_string();
                self.editor
                    .observe_apply_failed(SampleEditorError::InstallFailed);
                self.reject_apply_confirmation();
            }
        }
    }

    /// Applies the terminal half of every rejected Apply confirmation. This deliberately does
    /// not call `cancel_confirmation`: the caller's typed error must remain visible with its
    /// draft intact, while modal and token ownership always disappear together.
    fn reject_apply_confirmation(&mut self) {
        self.overlay = None;
        self.apply_sample_context = None;
    }

    fn apply_sample_discard_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter {
            self.editor.confirm_discard();
            self.sync_editor_to_selected_pad();
            self.overlay = None;
        }
    }

    fn adjust_editor_pitch(&mut self, delta: i8) {
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        let mut settings = self.editor.settings();
        settings.pitch_semitones = (settings.pitch_semitones + f32::from(delta)).clamp(-24.0, 24.0);
        self.admit_editor_settings(pad, settings);
    }

    fn set_editor_mode(&mut self, mode: PlaybackMode) {
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        let mut settings = self.editor.settings();
        settings.mode = mode;
        self.admit_editor_settings(pad, settings);
    }

    fn admit_editor_settings(&mut self, pad: PadId, settings: PadSettings) {
        match self.update_pad_settings(pad, settings) {
            Ok(()) => self.sync_editor_to_selected_pad(),
            Err(error) => self.status = error,
        }
    }

    fn apply_clear_pattern_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter {
            let Some(Overlay::ClearPattern { slot, .. }) = self.overlay.clone() else {
                return;
            };
            self.patterns.select_slot(slot);
            self.apply_pattern_edit(|patterns| patterns.clear_selected());
            if let Some(audio) = self.audio.as_mut()
                && let Err(error) = audio.set_record_capture(None)
            {
                self.status = error;
            }
            self.overlay = None;
        }
    }

    fn move_selection(&mut self, horizontal: isize, vertical: isize) {
        let row = self.selected_pad / 4;
        let column = self.selected_pad % 4;
        let row = row.saturating_add_signed(vertical).min(3);
        let column = column.saturating_add_signed(horizontal).min(3);
        self.select_pad(row * 4 + column);
    }

    fn select_pad(&mut self, index: usize) -> bool {
        let Some(pad) = self.pad_in_active_bank(index) else {
            return false;
        };
        if self.editor.pad() != pad && self.editor.is_dirty() {
            self.status = "discard sample draft before selecting another pad".to_owned();
            return false;
        }
        self.selected_pad = index;
        if self.patterns.view() == WorkspaceView::Sample {
            self.sync_editor_to_selected_pad();
        }
        true
    }

    fn editor_operation_pending(&self) -> bool {
        matches!(
            self.sample_edit_status(self.editor.pad()),
            SampleEditStatus::AwaitingWorker
                | SampleEditStatus::Rendering
                | SampleEditStatus::ReadyToInstall
        ) || matches!(
            self.editor.status(),
            crate::sample_editor::SampleEditorStatus::Pending
        )
    }

    fn move_pattern_cursor_pad(&mut self, delta: i8) {
        let cursor = self.patterns.cursor();
        let index = i16::from(cursor.pad().index())
            .saturating_add(i16::from(delta))
            .clamp(0, i16::from(PADS_PER_BANK.saturating_sub(1)));
        let index = u8::try_from(index).expect("clamped pattern pad index fits in u8");
        let pad = PadId::new(self.active_bank, index).expect("bounded pattern pad is valid");
        self.selected_pad = usize::from(index);
        self.patterns.move_cursor_to(pad, cursor.step());
    }

    fn change_pattern_slot(&mut self, delta: i8) {
        let current = i16::from(self.patterns.selected_slot().get());
        let requested = current.saturating_add(i16::from(delta)).clamp(0, 15);
        let slot = PatternSlotId::new(u8::try_from(requested).expect("bounded slot fits in u8"))
            .expect("bounded slot is valid");
        if slot == self.patterns.selected_slot() {
            self.status = if delta < 0 {
                "already at pattern 1".to_owned()
            } else {
                "already at pattern 16".to_owned()
            };
            return;
        }
        self.select_pattern(slot);
    }

    fn select_pattern_slot(&mut self, index: usize) {
        let Some(slot) = u8::try_from(index)
            .ok()
            .and_then(|index| PatternSlotId::new(index).ok())
        else {
            self.palette_error = Some("pattern must be 1..16".to_owned());
            return;
        };
        self.select_pattern(slot);
    }

    fn select_pattern(&mut self, slot: PatternSlotId) {
        if !self.patterns.is_slot_ready(slot) {
            self.patterns.select_slot(slot);
            self.report_pattern_not_ready(slot);
            return;
        }
        let disarm_capture = self
            .patterns
            .record_capture()
            .is_some_and(|(captured_slot, _)| captured_slot != slot);
        let switch = if self.pattern_transport_is_playing() {
            PatternSwitch::NextBoundary
        } else {
            PatternSwitch::Immediate
        };
        if let Some(audio) = self.audio.as_mut() {
            if disarm_capture && let Err(error) = audio.set_record_capture(None) {
                self.status = error;
                return;
            }
            if disarm_capture {
                self.patterns.stop_recording();
            }
            if let Err(error) = audio.select_pattern(slot, switch) {
                self.status = error;
                return;
            }
        } else if disarm_capture {
            self.report_audio_unavailable();
            return;
        }
        self.patterns.select_slot(slot);
        self.overlay = None;
    }

    fn start_pattern_playback(&mut self) {
        let slot = self.patterns.selected_slot();
        if !self.patterns.is_slot_ready(slot) {
            self.report_pattern_not_ready(slot);
            return;
        }
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        if let Err(error) = audio.select_pattern(slot, PatternSwitch::Immediate) {
            self.status = error;
            return;
        }
        if let Err(error) = audio.play_pattern() {
            self.status = error;
            return;
        }
        self.note_pattern_transport_intent(true);
        self.overlay = None;
    }

    fn stop_pattern_playback(&mut self) {
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        if let Err(error) = audio.set_record_capture(None) {
            self.status = error;
            return;
        }
        self.patterns.stop_recording();
        if let Err(error) = audio.stop_pattern() {
            self.status = error;
            return;
        }
        self.note_pattern_transport_intent(false);
        self.overlay = None;
    }

    fn toggle_pattern_recording(&mut self) {
        if self.patterns.capture_state().is_some() {
            self.stop_pattern_recording();
        } else {
            self.start_pattern_recording();
        }
    }

    fn start_pattern_recording(&mut self) {
        let slot = self.patterns.selected_slot();
        if !self.patterns.is_slot_ready(slot) {
            self.report_pattern_not_ready(slot);
            return;
        }
        let generation = self.patterns.selected_pattern().generation();
        let stamp = TransportStamp {
            slot,
            generation,
            origin: (self.telemetry.pattern_slot == Some(slot)
                && self.telemetry.pattern_generation == Some(generation))
            .then_some(self.telemetry.pattern_origin)
            .flatten()
            .unwrap_or(0),
            loop_frames: self.patterns.selected_pattern().transport().loop_frames(),
        };
        let start_transport = !self.pattern_transport_is_playing();
        {
            let Some(audio) = self.audio.as_mut() else {
                self.report_audio_unavailable();
                return;
            };
            if start_transport {
                if let Err(error) = audio.select_pattern(slot, PatternSwitch::Immediate) {
                    self.status = error;
                    return;
                }
                if let Err(error) = audio.play_pattern() {
                    self.status = error;
                    return;
                }
            }
        }
        if start_transport {
            self.note_pattern_transport_intent(true);
        }
        if let Err(error) = self.patterns.start_recording(stamp) {
            self.status = error.to_string();
            return;
        }
        let capture = self.patterns.record_capture();
        let result = self
            .audio
            .as_mut()
            .expect("audio remains present after transport admission")
            .set_record_capture(capture);
        if let Err(error) = result {
            self.patterns.stop_recording();
            self.status = error;
        }
    }

    fn stop_pattern_recording(&mut self) {
        self.patterns.stop_recording();
        if let Some(audio) = self.audio.as_mut()
            && let Err(error) = audio.set_record_capture(None)
        {
            self.status = error;
        }
    }

    fn open_clear_pattern(&mut self) {
        let slot = self.patterns.selected_slot();
        self.overlay = Some(Overlay::ClearPattern {
            slot,
            event_count: self.patterns.selected_pattern().events().len(),
        });
    }

    fn report_pattern_not_ready(&mut self, slot: PatternSlotId) {
        let status = self.patterns.last_status().filter(|status| match status {
            PatternStatus::UpdatePending { slot: status_slot }
            | PatternStatus::SnapshotBackpressured { slot: status_slot }
            | PatternStatus::SnapshotCompileFailed {
                slot: status_slot, ..
            }
            | PatternStatus::AudioCommandFailed {
                slot: status_slot, ..
            } => *status_slot == slot,
        });
        self.status = status.map_or_else(
            || pattern_status_text(&PatternStatus::UpdatePending { slot }),
            pattern_status_text,
        );
    }

    fn pattern_transport_is_playing(&self) -> bool {
        self.pending_pattern_transport
            .map(|intent| intent.playing)
            .unwrap_or(self.telemetry.pattern_playing)
    }

    fn note_pattern_transport_intent(&mut self, playing: bool) {
        self.pending_pattern_transport = Some(PendingPatternTransport { playing });
    }

    fn apply_pattern_edit(
        &mut self,
        edit: impl FnOnce(&mut PatternWorkspace) -> Result<(), sampler_core::PatternEditError>,
    ) {
        if let Err(error) = self.ensure_project_mutation_available() {
            self.status = error;
            return;
        }
        let generation = self.patterns.selected_pattern().generation();
        if let Err(error) = edit(&mut self.patterns) {
            self.status = error.to_string();
        } else {
            if self.patterns.selected_pattern().generation() != generation {
                self.commit_project_mutation();
            }
            self.overlay = None;
        }
    }

    pub fn selected_pad_id(&self) -> Option<PadId> {
        let index = u8::try_from(self.selected_pad).ok()?;
        PadId::new(self.active_bank, index).ok()
    }

    fn begin_selected_load(&mut self, path: PathBuf) {
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        if let Some(request) = self.begin_load(pad, path) {
            self.queue_worker_request(request);
        }
    }

    fn open_picker_parent(&mut self) {
        let directory = self
            .file_picker
            .pending_directory()
            .unwrap_or_else(|| self.file_picker.directory());
        let Some(parent) = directory.parent().map(ToOwned::to_owned) else {
            self.status = "already at filesystem root".to_owned();
            return;
        };
        self.open_picker_at(parent);
    }

    fn open_picker_selection(&mut self) {
        let Some(entry) = self.file_picker.selected().cloned() else {
            return;
        };
        if entry.is_directory() {
            self.open_picker_at(entry.path);
        } else if entry.is_selectable_file() {
            self.begin_selected_load(entry.path);
            self.overlay = None;
        } else {
            self.status = "entry is not a supported audio file".to_owned();
        }
    }

    fn queue_current_picker_scan(&mut self, request_id: u64) {
        let path = self
            .file_picker
            .pending_directory()
            .unwrap_or_else(|| self.file_picker.directory())
            .to_owned();
        self.queue_worker_request(WorkerRequest::ScanDirectory {
            request_id,
            path,
            show_hidden: self.file_picker.show_hidden(),
        });
    }

    fn release_pad(&mut self, index: usize) {
        if !self.validate_pad_index(index) {
            return;
        }
        let Some(pad) = self.held_pad_by_key[index] else {
            return;
        };
        if self.pads[pad_offset(pad)].settings.mode == PlaybackMode::OneShot
            && self.patterns.is_recording()
        {
            self.held_pad_by_key[index] = None;
            return;
        }
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        let recording = self.patterns.is_recording();
        let result = if recording {
            audio.release_live_tracked(pad).map(Some)
        } else {
            audio.release_live(pad).map(|()| None)
        };
        match result {
            Ok(command) => {
                self.held_pad_by_key[index] = None;
                if let Some(command) = command {
                    self.patterns.note_live_release(index, command);
                }
            }
            Err(error) => self.status = error,
        }
    }

    fn release_midi_owner(&mut self, owner: usize) -> bool {
        let Some(owned) = self.midi_owned_pads[owner] else {
            return true;
        };
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return false;
        };
        let result = audio.release_owned_live_tracked(owned.pad, owned.trigger_id);
        match result {
            Ok(command) => {
                self.midi_owned_pads[owner] = None;
                if self.patterns.is_recording() {
                    self.patterns
                        .note_live_release(MIDI_RECORDING_KEY_OFFSET + owner, command);
                }
                true
            }
            Err(error) => {
                self.status = error;
                false
            }
        }
    }

    fn stop_pad(&mut self, index: usize) {
        let Some(pad) = self.pad_in_active_bank(index) else {
            return;
        };
        let _ = self.select_pad(index);
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        match audio.stop_pad(pad) {
            Ok(()) if self.held_pad_by_key[index] == Some(pad) => {
                self.held_pad_by_key[index] = None;
            }
            Ok(()) => {}
            Err(error) => self.status = error,
        }
    }

    fn stop_all(&mut self) {
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        match audio.stop_all() {
            Ok(()) => {
                self.held_pad_by_key.fill(None);
                self.midi_owned_pads.fill(None);
                self.patterns.stop_recording();
                self.note_pattern_transport_intent(false);
            }
            Err(error) => self.status = error,
        }
    }

    fn change_bank(&mut self, delta: i8) {
        let current = i16::from(u8::from(self.active_bank));
        let requested = current + i16::from(delta);
        if requested < 0 {
            self.status = "already at first bank (A)".to_owned();
            return;
        }
        if requested >= i16::from(BANK_COUNT) {
            self.status = "already at last bank (J)".to_owned();
            return;
        }
        let next_bank = BankId::new(u8::try_from(requested).expect("bounded bank fits in u8"))
            .expect("bounded bank is valid");
        if self.editor.is_dirty() {
            self.status = "discard sample draft before changing bank".to_owned();
            return;
        }
        self.active_bank = next_bank;
        if self.patterns.view() == WorkspaceView::Pattern {
            let cursor = self.patterns.cursor();
            let pad = PadId::new(self.active_bank, cursor.pad().index())
                .expect("existing cursor index is valid");
            self.patterns.move_cursor_to(pad, cursor.step());
        }
        if self.patterns.view() == WorkspaceView::Sample {
            self.sync_editor_to_selected_pad();
        }
    }

    fn pad_in_active_bank(&mut self, index: usize) -> Option<PadId> {
        if !self.validate_pad_index(index) {
            return None;
        }
        let index = u8::try_from(index).expect("validated pad index fits in u8");
        Some(PadId::new(self.active_bank, index).expect("validated pad index is valid"))
    }

    fn validate_pad_index(&mut self, index: usize) -> bool {
        if index < usize::from(PADS_PER_BANK) {
            true
        } else {
            self.status = format!("pad {index} is outside 0..16");
            false
        }
    }

    fn report_audio_unavailable(&mut self) {
        self.status = self
            .audio_unavailable_message
            .clone()
            .unwrap_or_else(|| "audio device is unavailable".to_owned());
    }

    fn apply_telemetry(&mut self, telemetry: Telemetry) -> bool {
        let changed = self.telemetry != telemetry;
        if self
            .pending_pattern_transport
            .is_some_and(|intent| telemetry.pattern_playing == intent.playing)
        {
            self.pending_pattern_transport = None;
        }
        self.meter_left = self.meter_left.max(sanitize_peak(telemetry.peak_left));
        self.meter_right = self.meter_right.max(sanitize_peak(telemetry.peak_right));
        self.telemetry = telemetry;
        for bank in 0..BANK_COUNT {
            let bank = BankId::new(bank).expect("bounded bank is valid");
            for index in 0..PADS_PER_BANK {
                let pad = PadId::new(bank, index).expect("bounded pad is valid");
                self.pads[pad_offset(pad)].active = telemetry.is_pad_active(pad);
            }
        }
        changed
    }

    fn queue_worker_request(&mut self, request: WorkerRequest) -> bool {
        if self.pending_worker_requests.len() < WORKER_CHANNEL_CAPACITY {
            self.pending_worker_requests.push(request);
            true
        } else {
            self.apply_worker_send_error(request, WorkerSendError::WorkerBusy);
            false
        }
    }

    fn fail_audio(&mut self, error: String) {
        let capture_active = self.capture_session.phase().is_some();
        if capture_active {
            let _ = self
                .capture_session
                .mark_failed_with_cause(CaptureFailureCause::DeviceRuntime, error.clone());
        }
        self.audio = None;
        self.audio_format = None;
        self.recovery_cursor = None;
        self.reinstall_pending.fill(false);
        self.current_session_bound.fill(false);
        self.suspend_pending_sample_edits();
        self.fail_project_sample_apply(self.editor.pad());
        self.audio_unavailable_message = Some(error.clone());
        self.held_pad_by_key.fill(None);
        self.cancel_midi_learn();
        self.midi_owned_pads.fill(None);
        self.patterns.stop_recording();
        self.pending_pattern_transport = None;
        for pad in &mut self.pads {
            pad.active = false;
        }
        self.status = error.clone();
        self.overlay = if capture_active {
            Some(Overlay::CaptureFailed {
                action: self
                    .pending_project_action
                    .as_ref()
                    .map(PendingProjectAction::label),
            })
        } else {
            Some(Overlay::DeviceError(error))
        };
        self.sync_editor_to_selected_pad();
    }

    fn recover_audio(&mut self, mut audio: Box<dyn AudioPort>) {
        let sample_rate = audio.sample_rate();
        let channels = audio.channels();
        let mut local_error = None;
        let mut prebound = [false; PAD_VIEW_COUNT];

        if let Err(error) = audio.update_master_mix(self.master_mix) {
            self.status = error.clone();
            self.audio_unavailable_message = Some(error);
            return;
        }
        if self.project_open.is_none() {
            for (offset, bound) in prebound.iter_mut().enumerate() {
                let Some(sample) = self.pads[offset].sample.as_ref() else {
                    continue;
                };
                if sample.sample_rate() != sample_rate {
                    continue;
                }
                let pad = pad_from_offset(offset);
                if let Err(error) = audio.install_recovery(
                    pad,
                    Arc::clone(sample),
                    self.pads[offset].settings,
                    self.pad_mixes[offset],
                ) {
                    self.status = error.clone();
                    self.audio_unavailable_message = Some(error);
                    return;
                }
                *bound = true;
            }
        }

        self.midi_owned_pads.fill(None);
        self.audio = Some(audio);
        self.audio_format = Some((sample_rate, channels));
        self.audio_unavailable_message = None;
        self.held_pad_by_key.fill(None);
        self.overlay = None;
        self.committed_recovery_loads.fill_with(|| None);
        self.reinstall_pending.fill(false);
        self.current_session_bound.fill(false);
        self.pending_pattern_transport = None;
        if let Some(ProjectOpenOperation::Staging(candidate)) = self.project_open.as_mut() {
            let rate_changed = candidate.engine_rate != sample_rate;
            let staged_rate_matches = candidate
                .staged_pads
                .iter()
                .flatten()
                .all(|staged| staged.loaded.rendered.sample_rate() == sample_rate);
            let restart_staging = rate_changed || !staged_rate_matches;
            if restart_staging {
                candidate.engine_rate = sample_rate;
                candidate.stage_generation = candidate.stage_generation.wrapping_add(1);
                candidate.staged_pads.fill_with(|| None);
                candidate.next_decode = 0;
                candidate.decode_in_flight = None;
                candidate.progress.staged_pads = 0;
            }
            let mut patterns = PatternWorkspace::new(sample_rate);
            let rebuild = patterns
                .replace_project_patterns(candidate.document.patterns.clone())
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    patterns
                        .rebuild_sample_rate(sample_rate)
                        .map_err(|error| error.to_string())
                });
            if let Err(error) = rebuild {
                self.status = error;
            } else {
                candidate.patterns = patterns;
            }
            candidate.admission = ProjectAdmission::StopAll;
            candidate.progress.admitted_actions = 1;
            self.overlay = Some(Overlay::ProjectOpenProgress);
            self.status = if restart_staging {
                "Audio rate changed; restaging project audio".to_owned()
            } else {
                "Audio reconnected; restarting project admission".to_owned()
            };
            self.sync_editor_to_selected_pad();
            return;
        }
        if let Err(error) = self.patterns.rebuild_sample_rate(sample_rate) {
            self.status = error.to_string();
        }

        for bank in 0..BANK_COUNT {
            let bank = BankId::new(bank).expect("bounded bank is valid");
            for index in 0..PADS_PER_BANK {
                let pad = PadId::new(bank, index).expect("bounded pad is valid");
                let offset = pad_offset(pad);
                let view = &mut self.pads[offset];

                if view
                    .sample
                    .as_ref()
                    .is_some_and(|sample| sample.sample_rate() == sample_rate)
                {
                    self.current_session_bound[offset] = prebound[offset];
                    self.reinstall_pending[offset] = !prebound[offset];
                    if prebound[offset] {
                        view.state = PadLoadState::Ready;
                    }
                } else if let Some(path) = view.source.clone() {
                    self.recovery_generations[offset] = self.recovery_generations[offset]
                        .max(view.generation)
                        .wrapping_add(1);
                    self.committed_recovery_loads[offset] = Some(Box::new(PendingLoad {
                        path,
                        phase: PendingLoadPhase::AwaitingWorker,
                        kind: PendingLoadKind::Recovery,
                    }));
                } else if self.pending_loads[offset].is_none()
                    && (view.sample.is_some() || view.state != PadLoadState::Empty)
                {
                    let error = format!(
                        "cannot reload pad for {sample_rate} Hz because its source path is unavailable"
                    );
                    view.state = PadLoadState::Error(error.clone());
                    local_error = Some(error);
                }

                if let Some(pending) = self.pending_loads[offset].as_mut()
                    && matches!(
                        &pending.phase,
                        PendingLoadPhase::Ready(loaded)
                            if loaded.rendered.sample_rate() != sample_rate
                    )
                {
                    pending.phase = PendingLoadPhase::AwaitingWorker;
                }

                let user_load_active = self.pending_loads[offset]
                    .as_ref()
                    .is_some_and(|pending| !matches!(pending.phase, PendingLoadPhase::Failed));
                if self.reinstall_pending[offset]
                    || self.committed_recovery_loads[offset].is_some()
                    || user_load_active
                {
                    view.state = PadLoadState::Loading;
                }
            }
        }

        self.recovery_cursor = Some(0);
        self.status = local_error.unwrap_or_else(|| "audio device connected".to_owned());
        self.pump_recovery_requests();
        self.sync_editor_to_selected_pad();
        if self.capture_session.phase() == Some(CapturePhase::Failed) {
            self.restore_capture_presentation();
        }
        if self.pending_project_action.is_some() && self.project_lifecycle_wait.is_none() {
            self.advance_project_action();
        }
    }

    fn pump_recovery_requests(&mut self) -> bool {
        let Some(mut cursor) = self.recovery_cursor else {
            return false;
        };
        let Some(sample_rate) = self.audio.as_ref().map(|audio| audio.sample_rate()) else {
            self.recovery_cursor = None;
            return false;
        };
        let mut visited = 0;

        while visited < PAD_VIEW_COUNT {
            let offset = cursor;
            cursor = (cursor + 1) % PAD_VIEW_COUNT;
            visited += 1;

            if self.reinstall_pending[offset] {
                self.recovery_cursor = Some(cursor);
                self.reinstall_committed_sample(offset);
                return true;
            }

            if let Some(pending) = self.committed_recovery_loads[offset].as_mut() {
                if matches!(
                    &pending.phase,
                    PendingLoadPhase::Ready(loaded)
                        if loaded.rendered.sample_rate() != sample_rate
                ) {
                    pending.phase = PendingLoadPhase::AwaitingWorker;
                }

                match pending.phase {
                    PendingLoadPhase::AwaitingWorker => {
                        let request = WorkerRequest::LoadSample {
                            pad: pad_from_offset(offset),
                            generation: self.recovery_generations[offset],
                            purpose: LoadPurpose::Recovery,
                            path: pending.path.clone(),
                            engine_rate: sample_rate,
                            recipe: self.sample_editor.commits[offset].recipe,
                        };
                        pending.phase = PendingLoadPhase::WorkerQueued;
                        self.pads[offset].state = PadLoadState::Loading;
                        self.recovery_cursor = Some(cursor);
                        self.queue_worker_request(request);
                        return true;
                    }
                    PendingLoadPhase::Ready(_) => {
                        self.recovery_cursor = Some(cursor);
                        self.install_pending_load(offset, PendingLoadKind::Recovery);
                        return true;
                    }
                    PendingLoadPhase::WorkerQueued => continue,
                    PendingLoadPhase::Failed => {}
                }
            }

            if let Some(pending) = self.pending_loads[offset].as_mut() {
                if matches!(
                    &pending.phase,
                    PendingLoadPhase::Ready(loaded)
                        if loaded.rendered.sample_rate() != sample_rate
                ) {
                    pending.phase = PendingLoadPhase::AwaitingWorker;
                }

                match pending.phase {
                    PendingLoadPhase::AwaitingWorker => {
                        let request = WorkerRequest::LoadSample {
                            pad: pad_from_offset(offset),
                            generation: self.pads[offset].generation,
                            purpose: LoadPurpose::User,
                            path: pending.path.clone(),
                            engine_rate: sample_rate,
                            recipe: SampleEditRecipe::identity(),
                        };
                        pending.phase = PendingLoadPhase::WorkerQueued;
                        self.pads[offset].state = PadLoadState::Loading;
                        self.recovery_cursor = Some(cursor);
                        self.queue_worker_request(request);
                        return true;
                    }
                    PendingLoadPhase::Ready(_) => {
                        self.recovery_cursor = Some(cursor);
                        self.install_pending_load(offset, PendingLoadKind::User);
                        return true;
                    }
                    PendingLoadPhase::WorkerQueued | PendingLoadPhase::Failed => continue,
                }
            }
        }

        let still_recovering = self.recovery_action_pending();
        self.recovery_cursor = still_recovering.then_some(cursor);
        false
    }

    fn pending_load_slot(&self, offset: usize, kind: PendingLoadKind) -> &Option<Box<PendingLoad>> {
        match kind {
            PendingLoadKind::User => &self.pending_loads[offset],
            PendingLoadKind::Recovery => &self.committed_recovery_loads[offset],
        }
    }

    fn pending_load_slot_mut(
        &mut self,
        offset: usize,
        kind: PendingLoadKind,
    ) -> &mut Option<Box<PendingLoad>> {
        match kind {
            PendingLoadKind::User => &mut self.pending_loads[offset],
            PendingLoadKind::Recovery => &mut self.committed_recovery_loads[offset],
        }
    }

    fn matching_pending_load(
        &self,
        offset: usize,
        generation: u64,
        purpose: LoadPurpose,
        path: &Path,
    ) -> Option<PendingLoadKind> {
        let kind = match purpose {
            LoadPurpose::User => PendingLoadKind::User,
            LoadPurpose::Recovery => PendingLoadKind::Recovery,
        };
        let expected_generation = match kind {
            PendingLoadKind::User => self.pads[offset].generation,
            PendingLoadKind::Recovery => self.recovery_generations[offset],
        };
        (expected_generation == generation
            && self
                .pending_load_slot(offset, kind)
                .as_ref()
                .is_some_and(|pending| {
                    pending.kind.purpose() == purpose
                        && pending.path == path
                        && matches!(pending.phase, PendingLoadPhase::WorkerQueued)
                }))
        .then_some(kind)
    }

    fn install_pending_load(&mut self, offset: usize, kind: PendingLoadKind) {
        let Some(mut pending) = self.pending_load_slot_mut(offset, kind).take() else {
            return;
        };
        let PendingLoadPhase::Ready(loaded) = pending.phase else {
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            return;
        };
        let Some(audio_sample_rate) = self.audio.as_ref().map(|audio| audio.sample_rate()) else {
            pending.phase = PendingLoadPhase::Ready(loaded);
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            self.pads[offset].state = PadLoadState::WaitingForDevice;
            return;
        };
        if loaded.rendered.sample_rate() != audio_sample_rate {
            pending.phase = PendingLoadPhase::AwaitingWorker;
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            self.pads[offset].state = PadLoadState::Loading;
            self.recovery_cursor = Some(offset);
            return;
        }

        if kind == PendingLoadKind::User
            && let Err(error) = self.ensure_project_mutation_available()
        {
            pending.phase = PendingLoadPhase::Ready(loaded);
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            self.pads[offset].state = PadLoadState::Error(error.clone());
            self.status = error;
            self.refresh_editor_for_offset(offset);
            return;
        }

        let pad = pad_from_offset(offset);
        let settings = self.pads[offset].settings;
        let audio = self.audio.as_mut().expect("audio availability was checked");
        let install_result = match pending.kind {
            PendingLoadKind::User => audio.install(
                pad,
                Arc::clone(&loaded.rendered),
                settings,
                self.pad_mixes[offset],
            ),
            PendingLoadKind::Recovery => audio.install_recovery(
                pad,
                Arc::clone(&loaded.rendered),
                settings,
                self.pad_mixes[offset],
            ),
        };
        if let Err(error) = install_result {
            pending.phase = PendingLoadPhase::Ready(loaded);
            *self.pending_load_slot_mut(offset, kind) = Some(pending);
            self.pads[offset].state = PadLoadState::Error(error.clone());
            self.status = error;
            self.recovery_cursor.get_or_insert(offset);
            self.refresh_editor_for_offset(offset);
            return;
        }

        if kind == PendingLoadKind::User {
            self.retire_managed_capture_at(offset);
        }

        let label = pending
            .path
            .file_name()
            .unwrap_or(pending.path.as_os_str())
            .to_string_lossy()
            .into_owned();
        let view = &mut self.pads[offset];
        view.label = label.clone();
        view.source = Some(pending.path);
        if kind == PendingLoadKind::User {
            self.sample_editor.commits[offset].source_generation = view.generation;
            self.sample_editor.commits[offset].fingerprint = Some(loaded.fingerprint);
        }
        self.sample_editor.commits[offset].base = Some(loaded.base);
        self.sample_editor.commits[offset].recipe = loaded.recipe;
        view.sample = Some(loaded.rendered);
        self.sample_editor.commits[offset].base_preview = Some(loaded.base_preview);
        self.sample_editor.commits[offset].rendered_preview =
            Some(Arc::clone(&loaded.rendered_preview));
        view.preview = crate::loader::downsample_preview(&loaded.rendered_preview);
        view.state = PadLoadState::Ready;
        self.reinstall_pending[offset] = false;
        self.current_session_bound[offset] = true;
        if kind == PendingLoadKind::User {
            self.committed_recovery_loads[offset] = None;
            self.sample_editor.undo[offset] = None;
        }
        let action = if kind == PendingLoadKind::Recovery {
            "Recovered"
        } else {
            "Loaded"
        };
        self.status = format!("{action} {}", label.to_uppercase());
        self.refresh_editor_for_offset(offset);
        if kind == PendingLoadKind::User {
            self.commit_project_mutation();
        }
    }

    fn reinstall_committed_sample(&mut self, offset: usize) {
        let pad = pad_from_offset(offset);
        let Some(sample) = self.pads[offset].sample.as_ref().cloned() else {
            self.reinstall_pending[offset] = false;
            return;
        };
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        if sample.sample_rate() != audio.sample_rate() {
            self.reinstall_pending[offset] = false;
            if let Some(path) = self.pads[offset].source.clone() {
                self.recovery_generations[offset] = self.recovery_generations[offset]
                    .max(self.pads[offset].generation)
                    .wrapping_add(1);
                self.committed_recovery_loads[offset] = Some(Box::new(PendingLoad {
                    path,
                    phase: PendingLoadPhase::AwaitingWorker,
                    kind: PendingLoadKind::Recovery,
                }));
                self.pads[offset].state = PadLoadState::Loading;
                self.recovery_cursor = Some(offset);
            }
            return;
        }

        match audio.install_recovery(
            pad,
            sample,
            self.pads[offset].settings,
            self.pad_mixes[offset],
        ) {
            Ok(_) => {
                self.reinstall_pending[offset] = false;
                self.current_session_bound[offset] = true;
                self.pads[offset].state = PadLoadState::Ready;
            }
            Err(error) => {
                self.pads[offset].state = PadLoadState::Error(error.clone());
                self.status = error;
                self.recovery_cursor.get_or_insert(offset);
                self.refresh_editor_for_offset(offset);
            }
        }
    }

    fn recovery_action_pending(&self) -> bool {
        self.reinstall_pending
            .iter()
            .copied()
            .any(|pending| pending)
            || self
                .committed_recovery_loads
                .iter()
                .flatten()
                .any(|pending| {
                    matches!(
                        pending.phase,
                        PendingLoadPhase::AwaitingWorker | PendingLoadPhase::Ready(_)
                    )
                })
            || self.pending_loads.iter().flatten().any(|pending| {
                matches!(
                    pending.phase,
                    PendingLoadPhase::AwaitingWorker | PendingLoadPhase::Ready(_)
                )
            })
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // MIDI callbacks can feed commands into the App/audio ownership model, so quiesce them
        // before the audio callback and its application-owned sample buffers are torn down.
        drop(self.midi_service.take());
        drop(self.audio.take());
        for pad in &mut self.pads {
            pad.sample = None;
        }
        for commit in &mut self.sample_editor.commits {
            commit.base = None;
            commit.base_preview = None;
            commit.rendered_preview = None;
        }
        self.sample_editor.pending.fill_with(|| None);
        self.sample_editor.deferred_results.fill_with(|| None);
        self.sample_editor.undo.fill_with(|| None);
    }
}

fn resolve_picker_directory(current_dir: &Path, directory: PathBuf) -> PathBuf {
    let absolute = if directory.as_os_str().is_empty() {
        current_dir.to_owned()
    } else if directory.is_absolute() {
        directory
    } else {
        current_dir.join(directory)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn sanitize_peak(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn pattern_status_text(status: &PatternStatus) -> String {
    match status {
        PatternStatus::UpdatePending { slot } => {
            format!("pattern {} update pending", slot.get() + 1)
        }
        PatternStatus::SnapshotBackpressured { slot } => {
            format!("pattern {} update waiting for audio queue", slot.get() + 1)
        }
        PatternStatus::SnapshotCompileFailed { slot, error } => {
            format!("pattern {} compile failed: {error}", slot.get() + 1)
        }
        PatternStatus::AudioCommandFailed { slot, error } => {
            format!("pattern {} audio command failed: {error}", slot.get() + 1)
        }
    }
}

fn is_explicit_device_retry(key: KeyEvent) -> bool {
    let allowed = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    key.kind == KeyEventKind::Press
        && matches!(key.code, KeyCode::Char('r' | 'R'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && key.modifiers.difference(allowed).is_empty()
}

fn pad_offset(pad: PadId) -> usize {
    usize::from(u8::from(pad.bank())) * usize::from(PADS_PER_BANK) + usize::from(pad.index())
}

fn midi_owner_index(channel: u8, note: u8) -> usize {
    usize::from(channel - 1) * MIDI_NOTE_COUNT + usize::from(note)
}

fn pad_from_offset(offset: usize) -> PadId {
    let bank =
        u8::try_from(offset / usize::from(PADS_PER_BANK)).expect("bounded pad bank fits in u8");
    let index =
        u8::try_from(offset % usize::from(PADS_PER_BANK)).expect("bounded pad index fits in u8");
    PadId::new(BankId::new(bank).expect("bounded bank is valid"), index)
        .expect("bounded pad is valid")
}

fn restore_committed_audio_pads(
    audio: &mut dyn AudioPort,
    pads: &[PadView; PAD_VIEW_COUNT],
    pad_mixes: &[PadMixSettings; PAD_VIEW_COUNT],
    current_session_bound: &[bool; PAD_VIEW_COUNT],
    start_pad: usize,
    end_pad: usize,
) -> Result<(), (usize, String)> {
    for offset in start_pad..end_pad {
        let pad = pad_from_offset(offset);
        let result = if current_session_bound[offset] {
            let sample = pads[offset].sample.as_ref().ok_or_else(|| {
                (
                    offset,
                    format!("restore {pad:?}: committed sample is missing"),
                )
            })?;
            audio
                .install_recovery(
                    pad,
                    Arc::clone(sample),
                    pads[offset].settings,
                    pad_mixes[offset],
                )
                .map(|_| ())
                .map_err(|error| format!("restore {pad:?}: {error}"))
        } else {
            audio
                .remove_sample(pad)
                .map_err(|error| format!("remove candidate-only {pad:?}: {error}"))
        };
        if let Err(error) = result {
            return Err((offset, error));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use sampler_audio::{
        AudioController, AudioEngine, EnginePorts, Frame, LiveAck, LiveAckKind, LiveCommandId,
        PatternSnapshotSlot, PatternSwitch, SampleBuffer, SampleSlot, Telemetry, TransportStamp,
        audio_channels, audio_channels_with_test_capacities,
    };
    use sampler_core::{
        BankId, MidiChannel, MidiChannelFilter, MidiNote, MidiSettings, PadId, PadSettings,
        PatternSlotId, PatternSnapshot, PlaybackMode, SampleEditRecipe,
    };

    use crate::audio::{AudioPort, CaptureSupport};
    use crate::capture_store::{CaptureStoreError, ManagedCaptureId};
    use crate::input::InputAction;
    use crate::midi::{
        MidiBackend, MidiBackendPort, MidiConnection, MidiEvent, MidiIngressProducer, MidiService,
        MidiServiceError,
    };

    use crate::DirectoryScan;
    use crate::loader::{
        LoadPurpose, LoadSampleError, LoadedSample, ProjectSaveWorkerRequest, RenderedSample,
        WORKER_CHANNEL_CAPACITY, WorkerHandle, WorkerRequest, WorkerResult, WorkerSendError,
    };

    use super::{
        App, EDIT_PREVIEW_COLUMNS, MIDI_NOTE_COUNT, MIDI_OWNERSHIP_COUNT, MidiOwnedVoice,
        PADS_PER_BANK, PREVIEW_COLUMNS, PadLoadState, PreviewColumn, RecoveryCleanup,
        SampleEditStatus,
    };
    use crate::pattern::{PatternWorkspace, WorkspaceView};
    use crate::project_session::ProjectSnapshotError;
    use crate::project_store::{ProjectAssetMapping, ProjectStoreError, SaveKind, SaveReceipt};

    #[derive(Debug, Clone, PartialEq)]
    enum AudioCall {
        Install(PadId),
        RemoveSample(PadId),
        Trigger(PadId, Frame, f32),
        Release(PadId, Frame),
        StopPad(PadId),
        StopAll,
        TrackedTrigger(PadId, f32),
        TrackedRelease(PadId),
        TrackedOwnedRelease(PadId, LiveCommandId),
        InstallPattern,
        SelectPattern(PatternSlotId, PatternSwitch),
        PlayPattern,
        StopPattern,
        SetRecordCapture(Option<(PatternSlotId, u64)>),
    }

    #[derive(Clone)]
    struct CallLog(Rc<RefCell<Vec<AudioCall>>>);

    impl CallLog {
        fn snapshot(&self) -> Vec<AudioCall> {
            self.0.borrow().clone()
        }

        fn clear(&self) {
            self.0.borrow_mut().clear();
        }
    }

    struct FakeAudio {
        sample_rate: u32,
        channels: u16,
        horizon: Frame,
        horizon_reads: Rc<Cell<usize>>,
        trigger_error: Option<String>,
        release_error: Option<String>,
        owned_release_failure: Option<(usize, String)>,
        stop_pad_error: Option<String>,
        stop_all_error: Option<String>,
        stop_pattern_error: Option<String>,
        capture_error: Option<String>,
        install_error: Option<String>,
        update_error: Option<String>,
        calls: CallLog,
        maintenance: Rc<RefCell<Vec<&'static str>>>,
        runtime_error: Option<String>,
        shutdown: Option<Rc<RefCell<Vec<&'static str>>>>,
        pattern_controller: AudioController,
        _pattern_ports: EnginePorts,
        drain_pattern_queue_after_backpressure: bool,
        live_acks: VecDeque<LiveAck>,
        next_live_id: u64,
    }

    #[derive(Default)]
    struct FakeMidiState {
        ports: Vec<MidiBackendPort>,
        connected: Vec<String>,
        connect_error_for: Option<String>,
        closed: usize,
        lifecycle: Option<Rc<RefCell<Vec<&'static str>>>>,
    }

    struct FakeMidiBackend(Rc<RefCell<FakeMidiState>>);

    struct FakeMidiConnection(Rc<RefCell<FakeMidiState>>);

    impl MidiConnection for FakeMidiConnection {
        fn close(self: Box<Self>) {
            let mut state = self.0.borrow_mut();
            state.closed += 1;
            if let Some(lifecycle) = &state.lifecycle {
                lifecycle.borrow_mut().push("close-midi");
            }
        }
    }

    impl MidiBackend for FakeMidiBackend {
        fn list_ports(&mut self) -> Result<Vec<MidiBackendPort>, MidiServiceError> {
            Ok(self.0.borrow().ports.clone())
        }

        fn connect(
            &mut self,
            port: &MidiBackendPort,
            _producer: MidiIngressProducer,
        ) -> Result<Box<dyn MidiConnection>, MidiServiceError> {
            self.0.borrow_mut().connected.push(port.backend_id.clone());
            if self.0.borrow().connect_error_for.as_deref() == Some(port.backend_id.as_str()) {
                return Err(MidiServiceError::Connect("candidate refused".to_owned()));
            }
            Ok(Box::new(FakeMidiConnection(Rc::clone(&self.0))))
        }
    }

    fn fake_midi_service(
        ports: impl IntoIterator<Item = (&'static str, &'static str)>,
    ) -> (MidiService, Rc<RefCell<FakeMidiState>>) {
        let state = Rc::new(RefCell::new(FakeMidiState {
            ports: ports
                .into_iter()
                .map(|(backend_id, name)| MidiBackendPort {
                    backend_id: backend_id.to_owned(),
                    name: name.to_owned(),
                })
                .collect(),
            ..FakeMidiState::default()
        }));
        (
            MidiService::new(Box::new(FakeMidiBackend(Rc::clone(&state)))),
            state,
        )
    }

    impl FakeAudio {
        fn ready(sample_rate: u32, channels: u16) -> Self {
            let (pattern_controller, pattern_ports) = audio_channels();
            Self {
                sample_rate,
                channels,
                horizon: 0,
                horizon_reads: Rc::new(Cell::new(0)),
                trigger_error: None,
                release_error: None,
                owned_release_failure: None,
                stop_pad_error: None,
                stop_all_error: None,
                stop_pattern_error: None,
                capture_error: None,
                install_error: None,
                update_error: None,
                calls: CallLog(Rc::new(RefCell::new(Vec::new()))),
                maintenance: Rc::new(RefCell::new(Vec::new())),
                runtime_error: None,
                shutdown: None,
                pattern_controller,
                _pattern_ports: pattern_ports,
                drain_pattern_queue_after_backpressure: false,
                live_acks: VecDeque::new(),
                next_live_id: 1,
            }
        }

        fn pattern_queue_full_once(sample_rate: u32, channels: u16) -> Self {
            let (mut pattern_controller, pattern_ports) =
                audio_channels_with_test_capacities(1, 256, 64);
            pattern_controller.play_pattern().unwrap();
            Self {
                sample_rate,
                channels,
                horizon: 0,
                horizon_reads: Rc::new(Cell::new(0)),
                trigger_error: None,
                release_error: None,
                owned_release_failure: None,
                stop_pad_error: None,
                stop_all_error: None,
                stop_pattern_error: None,
                capture_error: None,
                install_error: None,
                update_error: None,
                calls: CallLog(Rc::new(RefCell::new(Vec::new()))),
                maintenance: Rc::new(RefCell::new(Vec::new())),
                runtime_error: None,
                shutdown: None,
                pattern_controller,
                _pattern_ports: pattern_ports,
                drain_pattern_queue_after_backpressure: false,
                live_acks: VecDeque::new(),
                next_live_id: 1,
            }
        }

        fn with_horizon(mut self, horizon: Frame) -> Self {
            self.horizon = horizon;
            self
        }

        fn failing_trigger(mut self, error: &str) -> Self {
            self.trigger_error = Some(error.to_owned());
            self
        }

        fn failing_release_once(mut self, error: &str) -> Self {
            self.release_error = Some(error.to_owned());
            self
        }

        fn failing_owned_release_at(mut self, attempt: usize, error: &str) -> Self {
            self.owned_release_failure = Some((attempt, error.to_owned()));
            self
        }

        fn failing_stop_pad_once(mut self, error: &str) -> Self {
            self.stop_pad_error = Some(error.to_owned());
            self
        }

        fn failing_stop_all_once(mut self, error: &str) -> Self {
            self.stop_all_error = Some(error.to_owned());
            self
        }

        fn failing_stop_pattern_once(mut self, error: &str) -> Self {
            self.stop_pattern_error = Some(error.to_owned());
            self
        }

        fn failing_capture_once(mut self, error: &str) -> Self {
            self.capture_error = Some(error.to_owned());
            self
        }

        fn call_log(&self) -> CallLog {
            self.calls.clone()
        }

        fn failing_install(mut self, error: &str) -> Self {
            self.install_error = Some(error.to_owned());
            self
        }

        fn failing_update_once(mut self, error: &str) -> Self {
            self.update_error = Some(error.to_owned());
            self
        }

        fn failing_runtime(mut self, error: &str) -> Self {
            self.runtime_error = Some(error.to_owned());
            self
        }

        fn with_shutdown_log(mut self, shutdown: Rc<RefCell<Vec<&'static str>>>) -> Self {
            self.shutdown = Some(shutdown);
            self
        }

        fn with_live_acks(mut self, acks: impl IntoIterator<Item = LiveAck>) -> Self {
            self.live_acks.extend(acks);
            self
        }

        fn admit_live_id(&mut self) -> LiveCommandId {
            let id = LiveCommandId::new(self.next_live_id).expect("test live id is nonzero");
            self.next_live_id = self.next_live_id.saturating_add(1);
            id
        }
    }

    impl Drop for FakeAudio {
        fn drop(&mut self) {
            if let Some(shutdown) = &self.shutdown {
                shutdown.borrow_mut().push("drop-audio");
            }
        }
    }

    impl AudioPort for FakeAudio {
        fn capture_support(&self) -> CaptureSupport {
            CaptureSupport::Unsupported
        }

        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn channels(&self) -> u16 {
            self.channels
        }

        fn render_horizon(&self) -> Frame {
            self.horizon_reads
                .set(self.horizon_reads.get().saturating_add(1));
            self.horizon
        }

        fn install(
            &mut self,
            pad: PadId,
            _sample: Arc<SampleBuffer>,
            _settings: PadSettings,
            _mix: sampler_core::PadMixSettings,
        ) -> Result<SampleSlot, String> {
            if let Some(error) = self.install_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::Install(pad));
            SampleSlot::new(0).map_err(|error| error.to_string())
        }

        fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String> {
            if let Some(error) = &self.trigger_error {
                return Err(error.clone());
            }
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::Trigger(pad, at, velocity));
            Ok(())
        }

        fn trigger_live(&mut self, pad: PadId, velocity: f32) -> Result<(), String> {
            if let Some(error) = &self.trigger_error {
                return Err(error.clone());
            }
            self.calls.0.borrow_mut().push(AudioCall::Trigger(
                pad,
                self.horizon.saturating_add(64),
                velocity,
            ));
            Ok(())
        }

        fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String> {
            if let Some(error) = self.release_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::Release(pad, at));
            Ok(())
        }

        fn release_live(&mut self, pad: PadId) -> Result<(), String> {
            if let Some(error) = self.release_error.take() {
                return Err(error);
            }
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::Release(pad, self.horizon.saturating_add(64)));
            Ok(())
        }

        fn trigger_live_tracked(
            &mut self,
            pad: PadId,
            velocity: f32,
        ) -> Result<LiveCommandId, String> {
            if let Some(error) = &self.trigger_error {
                return Err(error.clone());
            }
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::TrackedTrigger(pad, velocity));
            Ok(self.admit_live_id())
        }

        fn release_live_tracked(&mut self, pad: PadId) -> Result<LiveCommandId, String> {
            if let Some(error) = self.release_error.take() {
                return Err(error);
            }
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::TrackedRelease(pad));
            Ok(self.admit_live_id())
        }

        fn release_owned_live_tracked(
            &mut self,
            pad: PadId,
            target_trigger_id: LiveCommandId,
        ) -> Result<LiveCommandId, String> {
            if let Some((remaining, error)) = self.owned_release_failure.as_mut() {
                if *remaining == 1 {
                    let error = error.clone();
                    self.owned_release_failure = None;
                    return Err(error);
                }
                *remaining -= 1;
            }
            if let Some(error) = self.release_error.take() {
                return Err(error);
            }
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::TrackedOwnedRelease(pad, target_trigger_id));
            Ok(self.admit_live_id())
        }

        fn release_owned_live_batch(
            &mut self,
            releases: &[(PadId, LiveCommandId)],
        ) -> Result<Vec<LiveCommandId>, String> {
            if let Some((attempt, error)) = self.owned_release_failure.as_mut() {
                if *attempt <= releases.len() {
                    let error = error.clone();
                    self.owned_release_failure = None;
                    return Err(error);
                }
                *attempt -= releases.len();
            }
            if let Some(error) = self.release_error.take() {
                return Err(error);
            }
            let mut commands = Vec::with_capacity(releases.len());
            for &(pad, target_trigger_id) in releases {
                self.calls
                    .0
                    .borrow_mut()
                    .push(AudioCall::TrackedOwnedRelease(pad, target_trigger_id));
                commands.push(self.admit_live_id());
            }
            Ok(commands)
        }

        fn install_pattern(
            &mut self,
            snapshot: Arc<PatternSnapshot>,
        ) -> Result<PatternSnapshotSlot, String> {
            self.calls.0.borrow_mut().push(AudioCall::InstallPattern);
            let result = self
                .pattern_controller
                .install_pattern(snapshot)
                .map_err(|error| error.to_string());
            if result.is_err() {
                self.drain_pattern_queue_after_backpressure = true;
            }
            result
        }

        fn select_pattern(
            &mut self,
            slot: PatternSlotId,
            switch: PatternSwitch,
        ) -> Result<(), String> {
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::SelectPattern(slot, switch));
            Ok(())
        }

        fn play_pattern(&mut self) -> Result<(), String> {
            self.calls.0.borrow_mut().push(AudioCall::PlayPattern);
            Ok(())
        }

        fn stop_pattern(&mut self) -> Result<(), String> {
            if let Some(error) = self.stop_pattern_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::StopPattern);
            Ok(())
        }

        fn set_record_capture(
            &mut self,
            capture: Option<(PatternSlotId, u64)>,
        ) -> Result<(), String> {
            if let Some(error) = self.capture_error.take() {
                return Err(error);
            }
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::SetRecordCapture(capture));
            Ok(())
        }

        fn drain_live_acks(&mut self, output: &mut [LiveAck]) -> usize {
            let count = output.len().min(self.live_acks.len());
            for slot in output.iter_mut().take(count) {
                *slot = self
                    .live_acks
                    .pop_front()
                    .expect("bounded ack count was checked");
            }
            count
        }

        fn reclaim_retired_patterns(&mut self) -> usize {
            0
        }

        fn stop_pad(&mut self, pad: PadId) -> Result<(), String> {
            if let Some(error) = self.stop_pad_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::StopPad(pad));
            Ok(())
        }

        fn remove_sample(&mut self, pad: PadId) -> Result<(), String> {
            self.calls.0.borrow_mut().push(AudioCall::RemoveSample(pad));
            Ok(())
        }

        fn stop_all(&mut self) -> Result<(), String> {
            if let Some(shutdown) = &self.shutdown {
                shutdown.borrow_mut().push("stop-all");
            }
            if let Some(error) = self.stop_all_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::StopAll);
            Ok(())
        }

        fn update_pad(&mut self, _pad: PadId, _settings: PadSettings) -> Result<(), String> {
            if let Some(error) = self.update_error.take() {
                return Err(error);
            }
            Ok(())
        }

        fn update_pad_mix(
            &mut self,
            _pad: PadId,
            _settings: sampler_core::PadMixSettings,
        ) -> Result<(), String> {
            Ok(())
        }

        fn update_master_mix(
            &mut self,
            _settings: sampler_core::MasterMixSettings,
        ) -> Result<(), String> {
            Ok(())
        }

        fn reclaim_retired(&mut self) -> usize {
            self.maintenance.borrow_mut().push("reclaim");
            if self.drain_pattern_queue_after_backpressure {
                while self._pattern_ports.immediate_commands.pop().is_ok() {}
                while self._pattern_ports.commands.pop().is_ok() {}
                self.drain_pattern_queue_after_backpressure = false;
            }
            0
        }

        fn latest_telemetry(&mut self) -> Option<Telemetry> {
            None
        }

        fn poll_runtime_error(&mut self) -> Option<String> {
            self.maintenance.borrow_mut().push("poll");
            self.runtime_error.take()
        }
    }

    struct EngineAudio {
        controller: Rc<RefCell<AudioController>>,
        sample_rate: u32,
    }

    impl EngineAudio {
        fn harness(sample_rate: u32) -> (Self, Rc<RefCell<AudioController>>, AudioEngine) {
            let (controller, ports) = audio_channels();
            let controller = Rc::new(RefCell::new(controller));
            let engine = AudioEngine::new(sample_rate, ports).unwrap();
            (
                Self {
                    controller: Rc::clone(&controller),
                    sample_rate,
                },
                controller,
                engine,
            )
        }
    }

    impl AudioPort for EngineAudio {
        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn channels(&self) -> u16 {
            2
        }

        fn render_horizon(&self) -> Frame {
            self.controller.borrow().render_horizon()
        }

        fn install(
            &mut self,
            pad: PadId,
            sample: Arc<SampleBuffer>,
            settings: PadSettings,
            mix: sampler_core::PadMixSettings,
        ) -> Result<SampleSlot, String> {
            self.controller
                .borrow_mut()
                .install(pad, sample, settings, mix)
                .map_err(|error| error.to_string())
        }

        fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .trigger(pad, at, velocity)
                .map_err(|error| error.to_string())
        }

        fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .release(pad, at)
                .map_err(|error| error.to_string())
        }

        fn trigger_live_tracked(
            &mut self,
            pad: PadId,
            velocity: f32,
        ) -> Result<LiveCommandId, String> {
            self.controller
                .borrow_mut()
                .trigger_live_tracked(pad, velocity)
                .map_err(|error| error.to_string())
        }

        fn release_live_tracked(&mut self, pad: PadId) -> Result<LiveCommandId, String> {
            self.controller
                .borrow_mut()
                .release_live_tracked(pad)
                .map_err(|error| error.to_string())
        }

        fn release_owned_live_tracked(
            &mut self,
            pad: PadId,
            target_trigger_id: LiveCommandId,
        ) -> Result<LiveCommandId, String> {
            self.controller
                .borrow_mut()
                .release_owned_live_tracked(pad, target_trigger_id)
                .map_err(|error| error.to_string())
        }

        fn release_owned_live_batch(
            &mut self,
            releases: &[(PadId, LiveCommandId)],
        ) -> Result<Vec<LiveCommandId>, String> {
            self.controller
                .borrow_mut()
                .release_owned_live_batch(releases)
                .map_err(|error| error.to_string())
        }

        fn install_pattern(
            &mut self,
            snapshot: Arc<PatternSnapshot>,
        ) -> Result<PatternSnapshotSlot, String> {
            self.controller
                .borrow_mut()
                .install_pattern(snapshot)
                .map_err(|error| error.to_string())
        }

        fn select_pattern(
            &mut self,
            slot: PatternSlotId,
            switch: PatternSwitch,
        ) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .select_pattern(slot, switch)
                .map_err(|error| error.to_string())
        }

        fn play_pattern(&mut self) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .play_pattern()
                .map_err(|error| error.to_string())
        }

        fn stop_pattern(&mut self) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .stop_pattern()
                .map_err(|error| error.to_string())
        }

        fn set_record_capture(
            &mut self,
            capture: Option<(PatternSlotId, u64)>,
        ) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .set_record_capture(capture)
                .map_err(|error| error.to_string())
        }

        fn drain_live_acks(&mut self, output: &mut [LiveAck]) -> usize {
            self.controller.borrow_mut().drain_live_acks(output)
        }

        fn reclaim_retired_patterns(&mut self) -> usize {
            let mut reclaimed = 0;
            while self
                .controller
                .borrow_mut()
                .reclaim_retired_pattern()
                .is_some()
            {
                reclaimed += 1;
            }
            reclaimed
        }

        fn remove_sample(&mut self, pad: PadId) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .remove_sample(pad)
                .map_err(|error| error.to_string())
        }

        fn stop_pad(&mut self, pad: PadId) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .stop_pad(pad)
                .map_err(|error| error.to_string())
        }

        fn stop_all(&mut self) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .stop_all()
                .map_err(|error| error.to_string())
        }

        fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .update_pad(pad, settings)
                .map_err(|error| error.to_string())
        }

        fn update_pad_mix(
            &mut self,
            pad: PadId,
            settings: sampler_core::PadMixSettings,
        ) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .update_pad_mix(pad, settings)
                .map_err(|error| error.to_string())
        }

        fn update_master_mix(
            &mut self,
            settings: sampler_core::MasterMixSettings,
        ) -> Result<(), String> {
            self.controller
                .borrow_mut()
                .update_master_mix(settings)
                .map_err(|error| error.to_string())
        }

        fn reclaim_retired(&mut self) -> usize {
            self.controller.borrow_mut().reclaim_retired()
        }

        fn latest_telemetry(&mut self) -> Option<Telemetry> {
            self.controller.borrow_mut().latest_telemetry()
        }

        fn poll_runtime_error(&mut self) -> Option<String> {
            None
        }

        fn capture_support(&self) -> CaptureSupport {
            CaptureSupport::Unsupported
        }
    }

    #[test]
    fn audio_maintenance_reclaims_before_polling_runtime_errors() {
        let audio = FakeAudio::ready(48_000, 2);
        let maintenance = Rc::clone(&audio.maintenance);
        let mut app = App::with_audio(Box::new(audio));

        assert!(app.maintain_audio());
        assert_eq!(*maintenance.borrow(), ["reclaim", "poll"]);
    }

    #[test]
    fn an_audio_runtime_error_moves_the_app_to_device_failed_state() {
        let audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(audio));

        assert!(app.maintain_audio());

        assert_eq!(app.audio_format(), None);
        assert_eq!(
            app.overlay(),
            Some(&super::Overlay::DeviceError(
                "device disconnected".to_owned()
            ))
        );
        assert_eq!(app.status(), "device disconnected");
    }

    #[test]
    fn shutdown_stops_and_drops_audio_even_when_stop_all_fails() {
        let shutdown = Rc::new(RefCell::new(Vec::new()));
        let audio = FakeAudio::ready(48_000, 2)
            .failing_stop_all_once("stop-all queue is full")
            .with_shutdown_log(Rc::clone(&shutdown));
        let mut app = App::with_audio(Box::new(audio));

        assert_eq!(
            app.shutdown_audio(),
            Err("stop-all queue is full".to_owned())
        );

        assert_eq!(*shutdown.borrow(), ["stop-all", "drop-audio"]);
        assert_eq!(app.audio_format(), None);
    }

    #[test]
    fn runtime_failure_preserves_pads_and_retry_reinstalls_matching_rate() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let request = app.begin_load(pad(0, 0), path("kick.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "kick.wav"));

        app.maintain_audio();

        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::DeviceError(_))
        ));
        assert!(app.pad(pad(0, 0)).sample.is_some());

        let replacement = FakeAudio::ready(48_000, 2);
        let calls = replacement.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement)));

        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Ready);
        assert_eq!(
            calls.snapshot().last(),
            Some(&AudioCall::Install(pad(0, 0)))
        );
    }

    #[test]
    fn retry_at_a_new_rate_reloads_from_source_instead_of_reusing_pcm() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let request = app.begin_load(pad(0, 0), path("kick.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "kick.wav"));
        app.maintain_audio();

        let replacement = FakeAudio::ready(44_100, 2);
        let calls = replacement.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement)));

        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Loading);
        assert_eq!(calls.snapshot(), []);
        assert_eq!(
            app.take_worker_requests().last(),
            Some(&WorkerRequest::LoadSample {
                pad: pad(0, 0),
                generation: generation.wrapping_add(1),
                purpose: LoadPurpose::Recovery,
                path: "kick.wav".into(),
                engine_rate: 44_100,
                recipe: SampleEditRecipe::identity(),
            })
        );
    }

    #[test]
    fn later_matching_rate_retry_rejects_an_older_wrong_rate_result() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let request = app.begin_load(pad(0, 0), path("kick.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "kick.wav"));
        let retained = Arc::clone(app.pad(pad(0, 0)).sample.as_ref().unwrap());
        app.maintain_audio();

        let changed_rate = FakeAudio::ready(44_100, 2).failing_runtime("replacement disconnected");
        app.retry_default_device_with(|| Ok(Box::new(changed_rate)));
        let stale_request = app.take_worker_requests().pop().unwrap();
        let WorkerRequest::LoadSample {
            generation: stale_generation,
            purpose: stale_purpose,
            ..
        } = stale_request
        else {
            panic!("wrong request")
        };
        app.maintain_audio();

        let original_rate = FakeAudio::ready(48_000, 2);
        let calls = original_rate.call_log();
        app.retry_default_device_with(|| Ok(Box::new(original_rate)));

        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);
        assert!(
            !app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad(0, 0),
                stale_generation,
                stale_purpose,
                "kick.wav",
                44_100,
                1,
                SampleEditRecipe::identity(),
            ))
        );
        assert!(Arc::ptr_eq(
            app.pad(pad(0, 0)).sample.as_ref().unwrap(),
            &retained
        ));
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Ready);
        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);
    }

    #[test]
    fn recovery_progresses_fairly_after_busy_and_unreadable_pads() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        for index in 0..12 {
            let pad = pad(0, index);
            let source = format!("pad-{index}.wav");
            let request = app.begin_load(pad, &source).unwrap();
            let WorkerRequest::LoadSample { generation, .. } = request else {
                panic!("wrong request")
            };
            app.apply_worker_result(loaded(pad, generation, &source));
        }
        app.maintain_audio();

        app.retry_default_device_with(|| Ok(Box::new(FakeAudio::ready(44_100, 2))));

        let [first] = app.take_worker_requests().try_into().unwrap();
        assert!(matches!(
            first,
            WorkerRequest::LoadSample { pad: pad_id, .. } if pad_id == pad(0, 0)
        ));
        app.apply_worker_send_error(first, WorkerSendError::WorkerBusy);
        assert_eq!(app.status(), "loader busy");

        app.maintain_audio();
        let [second] = app.take_worker_requests().try_into().unwrap();
        assert!(matches!(
            second,
            WorkerRequest::LoadSample { pad: pad_id, .. } if pad_id == pad(0, 1)
        ));

        let mut requests = vec![second];
        let mut completed = Vec::new();
        while let Some(request) = requests.pop() {
            let WorkerRequest::LoadSample {
                pad: pad_id,
                generation,
                purpose,
                path,
                ..
            } = request
            else {
                panic!("wrong request")
            };

            if pad_id == pad(0, 1) {
                app.apply_worker_result(WorkerResult::Loaded {
                    pad: pad_id,
                    generation,
                    purpose,
                    path,
                    result: Err(LoadSampleError::Decode("unreadable early pad".to_owned())),
                });
            } else {
                completed.push(pad_id);
                app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                    pad_id,
                    generation,
                    purpose,
                    path.to_str().unwrap(),
                    44_100,
                    1,
                    SampleEditRecipe::identity(),
                ));
            }

            app.maintain_audio();
            requests.extend(app.take_worker_requests());
        }

        assert_eq!(completed.len(), 11);
        assert!(completed.contains(&pad(0, 0)));
        assert_eq!(app.pad(pad(0, 11)).state, PadLoadState::Ready);
        assert_eq!(
            app.pad(pad(0, 1)).state,
            PadLoadState::Error("unreadable early pad".to_owned())
        );
    }

    #[test]
    fn permanent_worker_error_named_loader_busy_is_not_retried_by_recovery() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let request = app.begin_load(pad(0, 0), "pad.wav").unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "pad.wav"));
        app.maintain_audio();
        app.retry_default_device_with(|| Ok(Box::new(FakeAudio::ready(44_100, 2))));

        let request = app.take_worker_requests().pop().unwrap();
        let WorkerRequest::LoadSample {
            pad: pad_id,
            generation,
            purpose,
            path,
            ..
        } = request
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad_id,
            generation,
            purpose,
            path,
            result: Err(LoadSampleError::Decode("loader busy".to_owned())),
        });

        app.maintain_audio();

        assert!(app.take_worker_requests().is_empty());
        assert_eq!(
            app.pad(pad(0, 0)).state,
            PadLoadState::Error("loader busy".to_owned())
        );
    }

    fn pad(bank: u8, index: u8) -> PadId {
        PadId::new(BankId::new(bank).unwrap(), index).unwrap()
    }

    fn path(value: &str) -> &std::path::Path {
        std::path::Path::new(value)
    }

    fn loaded(pad: PadId, generation: u64, source: &str) -> WorkerResult {
        loaded_with_frames(pad, generation, source, 48_000, 1)
    }

    fn loaded_with_frames(
        pad: PadId,
        generation: u64,
        source: &str,
        sample_rate: u32,
        frames: usize,
    ) -> WorkerResult {
        loaded_with_recipe_and_frames(
            pad,
            generation,
            source,
            sample_rate,
            frames,
            SampleEditRecipe::identity(),
        )
    }

    fn loaded_with_recipe_and_frames(
        pad: PadId,
        generation: u64,
        source: &str,
        sample_rate: u32,
        frames: usize,
        recipe: SampleEditRecipe,
    ) -> WorkerResult {
        loaded_with_purpose_recipe_and_frames(
            pad,
            generation,
            LoadPurpose::User,
            source,
            sample_rate,
            frames,
            recipe,
        )
    }

    fn loaded_with_purpose_recipe_and_frames(
        pad: PadId,
        generation: u64,
        purpose: LoadPurpose,
        source: &str,
        sample_rate: u32,
        frames: usize,
        recipe: SampleEditRecipe,
    ) -> WorkerResult {
        let rendered =
            Arc::new(SampleBuffer::new(sample_rate, [0.25, -0.25].repeat(frames)).unwrap());
        WorkerResult::Loaded {
            pad,
            generation,
            purpose,
            path: source.into(),
            result: Ok(LoadedSample {
                fingerprint: crate::SourceFingerprint::from_encoded_bytes(
                    std::path::Path::new("fixture.wav"),
                    &[],
                )
                .unwrap(),
                base: Arc::clone(&rendered),
                base_preview: Arc::new([PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS]),
                rendered,
                rendered_preview: Arc::new(
                    [PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS],
                ),
                recipe,
                source_rate: sample_rate,
                source_frames: frames,
                duration: std::time::Duration::from_secs_f64(
                    frames as f64 / f64::from(sample_rate),
                ),
            }),
        }
    }

    fn changed_rate_recovery_colliding_with_same_path_user_load() -> (
        App,
        PadId,
        SampleEditRecipe,
        u64,
        WorkerRequest,
        WorkerRequest,
    ) {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "same.wav").unwrap()
        else {
            panic!("expected initial load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "same.wav")));

        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::EditSample {
                generation: edit_generation,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected edit request");
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            *edit_generation,
            recipe,
            48_000,
            vec![-0.4, 0.4],
        )));
        assert!(app.maintain_audio());
        assert_eq!(app.sample_editor.commits[0].recipe, recipe);
        let source_generation = app.sample_editor_context(pad).source_generation;

        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        assert!(app.maintain_audio());
        app.retry_with(Box::new(FakeAudio::ready(44_100, 2)));
        let [recovery] = app.take_worker_requests().try_into().unwrap();
        let user = app.begin_load(pad, "same.wav").unwrap();
        (app, pad, recipe, source_generation, recovery, user)
    }

    fn edited(
        app: &App,
        pad: PadId,
        generation: u64,
        recipe: SampleEditRecipe,
        sample_rate: u32,
        frames: Vec<f32>,
    ) -> WorkerResult {
        let offset = super::pad_offset(pad);
        let base_preview = app.sample_editor.pending[offset]
            .as_ref()
            .map(|pending| Arc::clone(&pending.base_preview))
            .or_else(|| {
                app.sample_editor.commits[offset]
                    .base_preview
                    .as_ref()
                    .map(Arc::clone)
            })
            .expect("an edit result must carry its request's base preview");
        WorkerResult::Edited {
            pad,
            generation,
            recipe,
            result: Ok(RenderedSample {
                base_preview,
                rendered: Arc::new(SampleBuffer::new(sample_rate, frames).unwrap()),
                rendered_preview: Arc::new(
                    [PreviewColumn { min: -5, max: 5 }; EDIT_PREVIEW_COLUMNS],
                ),
            }),
        }
    }

    #[test]
    fn edit_commits_base_rendered_recipe_and_preview_only_after_audio_admission() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let old_rendered = Arc::clone(app.pad(pad).sample.as_ref().unwrap());
        let old_base = Arc::clone(app.base_sample(pad).unwrap());
        let old_source = app.pad(pad).source.clone();
        let old_label = app.pad(pad).label.clone();
        calls.clear();

        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::EditSample {
                generation,
                base,
                recipe: sent_recipe,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected one edit request")
        };
        assert!(Arc::ptr_eq(base, &old_base));
        assert_eq!(*sent_recipe, recipe);
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            *generation,
            recipe,
            48_000,
            vec![-0.4, 0.4],
        )));

        assert!(Arc::ptr_eq(
            app.pad(pad).sample.as_ref().unwrap(),
            &old_rendered
        ));
        assert_eq!(
            app.committed_sample_recipe(pad),
            Some(SampleEditRecipe::identity())
        );
        assert_eq!(calls.snapshot(), []);
        assert!(app.maintain_audio());

        assert_eq!(app.committed_sample_recipe(pad), Some(recipe));
        assert_eq!(app.pad(pad).sample.as_ref().unwrap().data(), &[-0.4, 0.4]);
        assert!(Arc::ptr_eq(app.base_sample(pad).unwrap(), &old_base));
        assert_eq!(app.pad(pad).source, old_source);
        assert_eq!(app.pad(pad).label, old_label);
        assert_eq!(
            app.edit_preview(pad).unwrap().as_ref(),
            &[PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS]
        );
        assert_eq!(
            app.pad(pad).preview,
            [PreviewColumn { min: -5, max: 5 }; PREVIEW_COLUMNS]
        );
        assert_eq!(
            calls
                .snapshot()
                .into_iter()
                .filter(|call| matches!(call, AudioCall::Install(_)))
                .collect::<Vec<_>>(),
            [AudioCall::Install(pad)]
        );
    }

    #[test]
    fn identity_edit_reuses_the_base_buffer_and_preview_owners() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let base = Arc::clone(app.base_sample(pad).unwrap());
        let base_preview = Arc::clone(app.edit_preview(pad).unwrap());

        app.request_sample_edit(pad, SampleEditRecipe::identity())
            .unwrap();
        let request = app.take_worker_requests().pop().unwrap();
        let mut worker = WorkerHandle::spawn();
        worker.try_send(request).unwrap();
        let result = worker.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(app.apply_worker_result(result));
        assert!(app.maintain_audio());

        assert!(Arc::ptr_eq(app.base_sample(pad).unwrap(), &base));
        assert!(Arc::ptr_eq(app.pad(pad).sample.as_ref().unwrap(), &base));
        assert!(Arc::ptr_eq(app.edit_preview(pad).unwrap(), &base_preview));
        assert!(Arc::ptr_eq(
            app.sample_editor.commits[0]
                .rendered_preview
                .as_ref()
                .unwrap(),
            &base_preview
        ));
        worker.shutdown().unwrap();
    }

    #[test]
    fn stale_worker_and_install_failure_keep_the_previous_tuple_exactly() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let old_rendered = Arc::clone(app.pad(pad).sample.as_ref().unwrap());
        let old_base = Arc::clone(app.base_sample(pad).unwrap());
        let old_recipe = app.committed_sample_recipe(pad).unwrap();
        let old_preview = app.pad(pad).preview;

        let first = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, first).unwrap();
        let first_generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        let second = SampleEditRecipe {
            normalize: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, second).unwrap();
        let second_generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        assert!(!app.apply_worker_result(edited(
            &app,
            pad,
            first_generation,
            first,
            48_000,
            vec![0.2, 0.2]
        )));
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            second_generation,
            second,
            48_000,
            vec![0.3, 0.3]
        )));

        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_install("install full"),
        ));
        assert!(app.maintain_audio());

        assert!(Arc::ptr_eq(
            app.pad(pad).sample.as_ref().unwrap(),
            &old_rendered
        ));
        assert!(Arc::ptr_eq(app.base_sample(pad).unwrap(), &old_base));
        assert_eq!(app.committed_sample_recipe(pad), Some(old_recipe));
        assert_eq!(app.pad(pad).preview, old_preview);
        assert_eq!(
            app.pad(pad).state,
            PadLoadState::Error("install full".to_owned())
        );
        assert!(app.current_session_bound[0]);
    }

    #[test]
    fn device_retry_redecodes_base_and_reapplies_committed_phase_recipe() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let recipe = SampleEditRecipe {
            start_phase: 1,
            end_phase: sampler_core::SAMPLE_PHASE_SCALE,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let edit_generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            edit_generation,
            recipe,
            48_000,
            vec![0.1, 0.1]
        )));
        assert!(app.maintain_audio());
        assert_eq!(app.committed_sample_recipe(pad), Some(recipe));
        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        assert!(app.maintain_audio());

        app.retry_default_device_with(|| Ok(Box::new(FakeAudio::ready(44_100, 2))));
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::LoadSample {
                engine_rate,
                recipe: recovered_recipe,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected recovery load")
        };
        assert_eq!(*engine_rate, 44_100);
        assert_eq!(*recovered_recipe, recipe);
    }

    #[test]
    fn undo_reinstalls_the_checkpoint_through_the_worker_and_audio_paths() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let base_preview = Arc::clone(app.edit_preview(pad).unwrap());
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            generation,
            recipe,
            48_000,
            vec![-0.4, 0.4],
        )));
        assert!(app.maintain_audio());
        calls.clear();

        app.undo_sample_edit(pad).unwrap();
        let WorkerRequest::EditSample {
            generation,
            recipe: undo_recipe,
            ..
        } = app.take_worker_requests().pop().unwrap()
        else {
            panic!("wrong request")
        };
        assert_eq!(undo_recipe, SampleEditRecipe::identity());
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            generation,
            undo_recipe,
            48_000,
            vec![0.25, -0.25],
        )));
        assert!(app.maintain_audio());
        assert!(Arc::ptr_eq(app.edit_preview(pad).unwrap(), &base_preview));

        assert_eq!(
            app.committed_sample_recipe(pad),
            Some(SampleEditRecipe::identity())
        );
        assert_eq!(app.sample_edit_status(pad), SampleEditStatus::Idle);
        assert_eq!(
            calls
                .snapshot()
                .into_iter()
                .filter(|call| matches!(call, AudioCall::Install(_)))
                .collect::<Vec<_>>(),
            [AudioCall::Install(pad)]
        );
    }

    #[test]
    fn busy_edit_send_retains_the_candidate_for_one_later_retry() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let recipe = SampleEditRecipe {
            normalize: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let request = app.take_worker_requests().pop().unwrap();
        assert!(app.apply_worker_send_error(request, WorkerSendError::WorkerBusy));
        assert_eq!(
            app.sample_edit_status(pad),
            SampleEditStatus::AwaitingWorker
        );
        assert_eq!(app.status(), "loader busy");

        assert!(app.maintain_audio());
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::EditSample {
                recipe: retried_recipe,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected retry")
        };
        assert_eq!(*retried_recipe, recipe);
    }

    #[test]
    fn device_recovery_never_auto_applies_a_confirmed_edit_that_was_interrupted() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let old_request = app.take_worker_requests().pop().unwrap();
        let WorkerRequest::EditSample {
            generation: old_generation,
            ..
        } = old_request
        else {
            panic!("wrong request")
        };

        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        assert!(app.maintain_audio());
        app.retry_default_device_with(|| Ok(Box::new(FakeAudio::ready(48_000, 2))));

        assert_eq!(app.sample_edit_status(pad), SampleEditStatus::Failed);
        assert!(app.take_worker_requests().is_empty());
        assert!(!app.apply_worker_result(edited(
            &app,
            pad,
            old_generation,
            recipe,
            48_000,
            vec![-0.4, 0.4]
        )));
        assert_eq!(
            app.committed_sample_recipe(pad),
            Some(SampleEditRecipe::identity())
        );
    }

    #[test]
    fn edit_generation_exhaustion_never_reuses_zero_or_replaces_the_live_request() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.sample_editor.generations[0] = u64::MAX - 1;
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let WorkerRequest::EditSample {
            generation: max_generation,
            ..
        } = app.take_worker_requests().pop().unwrap()
        else {
            panic!("wrong request")
        };
        assert_eq!(max_generation, u64::MAX);

        assert_eq!(
            app.request_sample_edit(pad, SampleEditRecipe::identity()),
            Err(super::SampleEditRequestError::GenerationExhausted)
        );
        assert_eq!(app.sample_editor.generations[0], u64::MAX);
        assert_eq!(
            app.sample_editor.pending[0]
                .as_ref()
                .map(|pending| pending.generation),
            Some(u64::MAX)
        );
        assert_eq!(
            app.sample_edit_status(pad),
            SampleEditStatus::GenerationExhausted
        );
        assert!(app.take_worker_requests().is_empty());
    }

    #[test]
    fn deferred_edit_results_accept_only_the_exact_current_generation() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.sample_editor.generations[0] = 0;
        let recipe = SampleEditRecipe {
            normalize: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let current_generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        app.edit_result_advanced = true;

        assert!(!app.apply_worker_result(edited(
            &app,
            pad,
            u64::MAX,
            recipe,
            48_000,
            vec![0.9, 0.9],
        )));
        assert!(app.sample_editor.deferred_results[0].is_none());
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            current_generation,
            recipe,
            48_000,
            vec![0.4, 0.4]
        )));
        assert!(app.sample_editor.deferred_results[0].is_some());

        assert!(app.maintain_audio());
        assert_eq!(app.sample_edit_status(pad), SampleEditStatus::UndoAvailable);
        assert_eq!(app.pad(pad).sample.as_ref().unwrap().data(), &[0.4, 0.4]);
    }

    #[test]
    fn newer_edit_discards_deferred_prior_result_without_spending_next_maintenance_budget() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let recipe_a = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe_a).unwrap();
        let generation_a = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        app.edit_result_advanced = true;
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            generation_a,
            recipe_a,
            48_000,
            vec![-0.4, 0.4]
        )));
        assert!(app.sample_editor.deferred_results[0].is_some());

        let recipe_b = SampleEditRecipe {
            normalize: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe_b).unwrap();
        let generation_b = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        assert!(app.sample_editor.deferred_results[0].is_none());

        // Model an already-queued stale result arriving just before maintenance. It must be
        // discarded before the one-result budget is marked consumed.
        let stale_result = edited(&app, pad, generation_a, recipe_a, 48_000, vec![-0.4, 0.4]);
        app.sample_editor.deferred_results[0] = Some(Box::new(stale_result));
        assert!(app.maintain_audio());
        assert!(!app.edit_result_advanced);
        assert!(app.sample_editor.deferred_results[0].is_none());

        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            generation_b,
            recipe_b,
            48_000,
            vec![0.4, 0.4]
        )));
        assert!(matches!(
            app.sample_editor.pending[0]
                .as_ref()
                .map(|pending| &pending.phase),
            Some(super::PendingEditPhase::Ready(_))
        ));
        assert!(app.sample_editor.deferred_results[0].is_none());
    }

    #[test]
    fn exhausted_undo_generation_preserves_the_checkpoint_and_current_tuple() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let edit_generation = match app.take_worker_requests().pop().unwrap() {
            WorkerRequest::EditSample { generation, .. } => generation,
            _ => panic!("wrong request"),
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            edit_generation,
            recipe,
            48_000,
            vec![-0.4, 0.4]
        )));
        assert!(app.maintain_audio());
        let current = Arc::clone(app.pad(pad).sample.as_ref().unwrap());
        app.sample_editor.generations[0] = u64::MAX;

        assert_eq!(
            app.undo_sample_edit(pad),
            Err(super::SampleEditRequestError::GenerationExhausted)
        );
        assert!(Arc::ptr_eq(app.pad(pad).sample.as_ref().unwrap(), &current));
        assert_eq!(app.committed_sample_recipe(pad), Some(recipe));
        assert!(app.sample_editor.undo[0].is_some());
        assert!(app.sample_editor.pending[0].is_none());
        assert_eq!(
            app.sample_edit_status(pad),
            SampleEditStatus::GenerationExhausted
        );
    }

    #[test]
    fn app_discards_a_superseded_load_generation() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.begin_load(pad(0, 0), path("old.wav"));
        let old_generation = app.pad(pad(0, 0)).generation;
        app.begin_load(pad(0, 0), path("new.wav"));

        app.apply_worker_result(loaded(pad(0, 0), old_generation, "old.wav"));

        assert_eq!(app.pad(pad(0, 0)).source, None);
        assert_eq!(
            app.pending_loads[0]
                .as_ref()
                .map(|pending| pending.path.as_path()),
            Some(path("new.wav"))
        );
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Loading);
    }

    #[test]
    fn failed_replacement_keeps_committed_source_and_sample_paired() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let first = app.begin_load(pad(0, 0), path("old.wav")).unwrap();
        let WorkerRequest::LoadSample {
            generation: first_generation,
            ..
        } = first
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), first_generation, "old.wav"));
        let committed = Arc::clone(app.pad(pad(0, 0)).sample.as_ref().unwrap());

        let replacement = app.begin_load(pad(0, 0), path("new.wav")).unwrap();
        let WorkerRequest::LoadSample {
            generation: replacement_generation,
            path: result_path,
            ..
        } = replacement
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(0, 0),
            generation: replacement_generation,
            purpose: LoadPurpose::User,
            path: result_path,
            result: Err(LoadSampleError::Decode(
                "replacement decode failed".to_owned(),
            )),
        });

        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("old.wav")));
        assert!(Arc::ptr_eq(
            app.pad(pad(0, 0)).sample.as_ref().unwrap(),
            &committed
        ));
    }

    #[test]
    fn device_retry_after_a_failed_replacement_reinstalls_the_committed_sample() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let first = app.begin_load(pad(0, 0), "old.wav").unwrap();
        let WorkerRequest::LoadSample { generation, .. } = first else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "old.wav"));

        let replacement = app.begin_load(pad(0, 0), "new.wav").unwrap();
        let WorkerRequest::LoadSample {
            generation,
            path: result_path,
            ..
        } = replacement
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(0, 0),
            generation,
            purpose: LoadPurpose::User,
            path: result_path,
            result: Err(LoadSampleError::Decode("replacement failed".to_owned())),
        });
        app.maintain_audio();

        let replacement_audio = FakeAudio::ready(48_000, 2);
        let calls = replacement_audio.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement_audio)));

        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);
        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("old.wav")));
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Ready);
    }

    #[test]
    fn same_rate_retry_recovers_committed_sample_while_replacement_remains_pending() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let first = app.begin_load(pad(0, 0), "old.wav").unwrap();
        let WorkerRequest::LoadSample { generation, .. } = first else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "old.wav"));
        let replacement = app.begin_load(pad(0, 0), "new.wav").unwrap();
        app.maintain_audio();

        let replacement_audio = FakeAudio::ready(48_000, 2);
        let calls = replacement_audio.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement_audio)));

        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);
        let WorkerRequest::LoadSample {
            generation,
            path: result_path,
            ..
        } = replacement
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(0, 0),
            generation,
            purpose: LoadPurpose::User,
            path: result_path,
            result: Err(LoadSampleError::Decode("replacement failed".to_owned())),
        });

        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("old.wav")));
        assert_eq!(
            app.pad(pad(0, 0)).sample.as_ref().unwrap().sample_rate(),
            48_000
        );
    }

    #[test]
    fn same_rate_recovery_survives_replacement_started_before_maintenance() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        for (index, source) in [(0, "first.wav"), (1, "old.wav")] {
            let pad = pad(0, index);
            let request = app.begin_load(pad, source).unwrap();
            let WorkerRequest::LoadSample { generation, .. } = request else {
                panic!("wrong request")
            };
            assert!(app.apply_worker_result(loaded(pad, generation, source)));
        }
        assert!(app.maintain_audio());

        let replacement_audio = FakeAudio::ready(48_000, 2);
        let calls = replacement_audio.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement_audio)));
        assert_eq!(
            calls.snapshot(),
            [AudioCall::Install(pad(0, 0)), AudioCall::Install(pad(0, 1)),]
        );

        let replacement = app.begin_load(pad(0, 1), "new.wav").unwrap();
        let WorkerRequest::LoadSample {
            generation,
            path: replacement_path,
            ..
        } = replacement
        else {
            panic!("wrong request")
        };
        assert!(app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(0, 1),
            generation,
            purpose: LoadPurpose::User,
            path: replacement_path,
            result: Err(LoadSampleError::Decode("replacement failed".to_owned())),
        }));

        assert!(app.maintain_audio());

        assert_eq!(
            calls
                .snapshot()
                .into_iter()
                .filter(|call| matches!(call, AudioCall::Install(_)))
                .collect::<Vec<_>>(),
            [AudioCall::Install(pad(0, 0)), AudioCall::Install(pad(0, 1))]
        );
        assert_eq!(app.pad(pad(0, 1)).source.as_deref(), Some(path("old.wav")));
        assert_eq!(app.pad(pad(0, 1)).state, PadLoadState::Ready);
    }

    #[test]
    fn changed_rate_retry_recovers_committed_source_while_replacement_remains_pending() {
        let failed_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed_audio));
        let first = app.begin_load(pad(0, 0), "old.wav").unwrap();
        let WorkerRequest::LoadSample { generation, .. } = first else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "old.wav"));
        let replacement = app.begin_load(pad(0, 0), "new.wav").unwrap();
        app.maintain_audio();

        let replacement_audio = FakeAudio::ready(44_100, 2);
        let calls = replacement_audio.call_log();
        app.retry_default_device_with(|| Ok(Box::new(replacement_audio)));
        let [recovery] = app.take_worker_requests().try_into().unwrap();
        let WorkerRequest::LoadSample {
            generation,
            purpose,
            path: recovery_path,
            ..
        } = recovery
        else {
            panic!("wrong request")
        };
        assert_eq!(recovery_path, path("old.wav"));
        app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
            pad(0, 0),
            generation,
            purpose,
            "old.wav",
            44_100,
            1,
            SampleEditRecipe::identity(),
        ));
        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);

        let WorkerRequest::LoadSample {
            generation,
            path: result_path,
            ..
        } = replacement
        else {
            panic!("wrong request")
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(0, 0),
            generation,
            purpose: LoadPurpose::User,
            path: result_path,
            result: Err(LoadSampleError::Decode("replacement failed".to_owned())),
        });

        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("old.wav")));
        assert_eq!(
            app.pad(pad(0, 0)).sample.as_ref().unwrap().sample_rate(),
            44_100
        );
    }

    #[test]
    fn matching_load_is_installed_before_replacing_the_pad_sample() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        let request = app.begin_load(pad(0, 0), path("new.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };

        app.apply_worker_result(loaded(pad(0, 0), generation, "new.wav"));

        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Ready);
        assert!(app.pad(pad(0, 0)).sample.is_some());
    }

    #[test]
    fn install_failure_preserves_the_prior_ready_sample() {
        let fake = FakeAudio::ready(48_000, 2).failing_install("install queue is full");
        let mut app = App::with_audio(Box::new(fake));
        let first = Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap());
        app.pads[0].sample = Some(Arc::clone(&first));
        app.pads[0].state = PadLoadState::Ready;
        let request = app.begin_load(pad(0, 0), path("new.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };

        app.apply_worker_result(loaded(pad(0, 0), generation, "new.wav"));

        assert!(Arc::ptr_eq(
            app.pad(pad(0, 0)).sample.as_ref().unwrap(),
            &first
        ));
        assert!(matches!(app.pad(pad(0, 0)).state, PadLoadState::Error(_)));
    }

    #[test]
    fn no_device_retains_the_path_without_creating_a_load_request() {
        let mut app = App::without_audio("no output device");

        let request = app.begin_load(pad(0, 0), path("kick.wav"));

        assert!(request.is_none());
        assert_eq!(app.pad(pad(0, 0)).source, None);
        assert_eq!(
            app.pending_loads[0]
                .as_ref()
                .map(|pending| pending.path.as_path()),
            Some(path("kick.wav"))
        );
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::WaitingForDevice);
    }

    #[test]
    fn no_device_pending_load_is_scheduled_after_retry() {
        let mut app = App::without_audio("no output device");
        app.begin_load(pad(0, 0), "kick.wav");
        let generation = app.pad(pad(0, 0)).generation;

        app.retry_default_device_with(|| Ok(Box::new(FakeAudio::ready(44_100, 2))));

        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::LoadSample {
                pad: pad(0, 0),
                generation,
                purpose: LoadPurpose::User,
                path: "kick.wav".into(),
                engine_rate: 44_100,
                recipe: SampleEditRecipe::identity(),
            }]
        );
        assert_eq!(app.pad(pad(0, 0)).source, None);
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Loading);
    }

    #[test]
    fn pad_press_uses_render_horizon_plus_sixty_four_frames() {
        let fake = FakeAudio::ready(48_000, 2).with_horizon(10_000);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(5));

        assert_eq!(
            calls.snapshot(),
            [AudioCall::Trigger(pad(0, 5), 10_064, 1.0)]
        );
    }

    #[test]
    fn pad_press_uses_the_causal_live_port_without_a_separate_horizon_read() {
        let fake = FakeAudio::ready(48_000, 2).with_horizon(10_000);
        let horizon_reads = Rc::clone(&fake.horizon_reads);
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(5));

        assert_eq!(horizon_reads.get(), 0);
    }

    #[test]
    fn fallback_one_shot_press_rearms_without_a_release_event() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.pads[0].settings =
            PadSettings::new(PlaybackMode::OneShot, 0.0, 0.0, 0.0, None).unwrap();

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadPress(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
        assert!(!app.is_pad_held(0));
    }

    #[test]
    fn bank_navigation_is_bounded_and_release_targets_the_original_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::BankDelta(1));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(app.active_bank(), BankId::new(1).unwrap());
        assert_eq!(
            calls.snapshot().last(),
            Some(&AudioCall::Release(pad(0, 0), 64))
        );
    }

    #[test]
    fn duplicate_press_does_not_retrigger_or_replace_the_held_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::BankDelta(1));
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::Release(pad(0, 0), 64),
            ]
        );
    }

    #[test]
    fn failed_release_keeps_the_held_pad_for_retry() {
        let fake = FakeAudio::ready(48_000, 2).failing_release_once("release queue is full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadRelease(0));
        assert!(app.status().contains("release queue is full"));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::Release(pad(0, 0), 64),
            ]
        );
    }

    #[test]
    fn recording_pad_lifecycle_tracks_gate_and_loop_releases_but_not_one_shot() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });
        let stamp = sampler_audio::TransportStamp {
            slot: PatternSlotId::new(0).unwrap(),
            generation: 0,
            origin: 0,
            loop_frames: 96_000,
        };

        for (index, mode, expect_release) in [
            (0, PlaybackMode::OneShot, false),
            (1, PlaybackMode::Gate, true),
            (2, PlaybackMode::Loop, true),
        ] {
            app.patterns.start_recording(stamp).unwrap();
            app.pads[index].settings = PadSettings::new(mode, 0.0, 0.0, 0.0, None).unwrap();
            app.apply(InputAction::PadPress(index));
            app.apply(InputAction::PadRelease(index));
            assert!(!app.is_pad_held(index));
            assert_eq!(
                calls
                    .snapshot()
                    .iter()
                    .filter(|call| matches!(call, AudioCall::TrackedRelease(tracked) if *tracked == pad(0, index as u8)))
                    .count(),
                usize::from(expect_release),
                "{mode:?} release semantics",
            );
            calls.clear();
            app.patterns.stop_recording();
        }
    }

    #[test]
    fn failed_stop_pad_keeps_the_slot_held_until_stop_retry_succeeds() {
        let fake = FakeAudio::ready(48_000, 2).failing_stop_pad_once("stop queue is full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadStop(0));
        assert!(app.status().contains("stop queue is full"));
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadStop(0));
        app.apply(InputAction::PadPress(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::StopPad(pad(0, 0)),
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
    }

    #[test]
    fn bank_switched_stop_does_not_forget_the_original_held_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::BankDelta(1));
        app.apply(InputAction::PadStop(0));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::StopPad(pad(1, 0)),
                AudioCall::Release(pad(0, 0), 64),
            ]
        );
    }

    #[test]
    fn failed_stop_all_keeps_slots_held_until_stop_retry_succeeds() {
        let fake = FakeAudio::ready(48_000, 2).failing_stop_all_once("stop-all queue is full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::StopAll);
        assert!(app.status().contains("stop-all queue is full"));
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::StopAll);
        app.apply(InputAction::PadPress(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::StopAll,
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
    }

    #[test]
    fn controller_overflow_is_visible_and_nonfatal() {
        let fake = FakeAudio::ready(48_000, 2).failing_trigger("audio command queue is full");
        let mut app = App::with_audio(Box::new(fake));
        app.apply(InputAction::PadPress(0));
        assert!(app.status().contains("queue is full"));
        assert!(!app.should_quit());
    }

    #[test]
    fn scheduling_saturates_at_the_frame_limit() {
        let fake = FakeAudio::ready(48_000, 2).with_horizon(Frame::MAX);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(15));

        assert_eq!(
            calls.snapshot(),
            [AudioCall::Trigger(pad(0, 15), Frame::MAX, 1.0)]
        );
    }

    #[test]
    fn bank_navigation_does_not_wrap_and_reports_both_edges() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.apply(InputAction::BankDelta(-1));
        assert_eq!(app.active_bank(), BankId::new(0).unwrap());
        assert!(app.status().contains("first bank"));

        app.apply(InputAction::BankDelta(9));
        assert_eq!(app.active_bank(), BankId::new(9).unwrap());
        app.apply(InputAction::BankDelta(1));
        assert_eq!(app.active_bank(), BankId::new(9).unwrap());
        assert!(app.status().contains("last bank"));
        assert!(!app.should_quit());
    }

    #[test]
    fn invalid_pad_positions_are_visible_and_nonfatal() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(16));
        app.apply(InputAction::PadRelease(usize::MAX));

        assert!(calls.snapshot().is_empty(), "{:?}", calls.snapshot());
        assert!(app.status().contains("outside 0..16"));
        assert!(!app.should_quit());
    }

    #[test]
    fn missing_audio_keeps_a_complete_browsable_model() {
        let mut app = App::without_audio("no output device");

        assert_eq!(app.active_bank(), BankId::new(0).unwrap());
        assert_eq!(app.pads().len(), super::PAD_VIEW_COUNT);
        assert_eq!(
            app.overlay(),
            Some(&super::Overlay::DeviceError("no output device".to_owned()))
        );
        app.apply(InputAction::PadPress(0));
        assert!(app.status().contains("no output device"));
        assert!(!app.should_quit());
    }

    fn key(character: char, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Char(character), modifiers, kind)
    }

    fn transport_telemetry(rendered_frame: Frame, playing: bool) -> Telemetry {
        Telemetry {
            active_pads: [0; 3],
            rendered_frame,
            last_triggered_frame: None,
            peak_left: 0.0,
            peak_right: 0.0,
            active_voices: 0,
            late_commands: 0,
            invalid_commands: 0,
            command_overflows: 0,
            pattern_slot: Some(PatternSlotId::new(0).unwrap()),
            pattern_generation: Some(0),
            pattern_playing: playing,
            pattern_recording: false,
            pattern_origin: playing.then_some(100),
            pattern_playhead: 0,
            pattern_loop_count: 0,
            pattern_overflows: 0,
            live_ack_overflows: 0,
        }
    }

    #[test]
    fn stale_stopped_telemetry_does_not_cancel_an_accepted_play_intent() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        app.maintain_audio();
        calls.clear();
        app.apply_telemetry(transport_telemetry(100, false));

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_telemetry(transport_telemetry(101, false));
        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
                AudioCall::SelectPattern(
                    PatternSlotId::new(1).unwrap(),
                    PatternSwitch::NextBoundary,
                ),
            ]
        );
    }

    #[test]
    fn stale_playing_telemetry_does_not_cancel_an_accepted_stop_intent() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        app.maintain_audio();
        calls.clear();
        app.apply_telemetry(transport_telemetry(100, true));

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_telemetry(transport_telemetry(101, true));
        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SetRecordCapture(None),
                AudioCall::StopPattern,
                AudioCall::SelectPattern(PatternSlotId::new(1).unwrap(), PatternSwitch::Immediate),
            ]
        );
    }

    #[test]
    fn play_does_not_admit_transport_before_the_selected_snapshot_is_installed() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(calls.snapshot().is_empty());
        assert!(app.status().contains("pattern 1 update pending"));
    }

    #[test]
    fn play_waits_for_backpressured_install_then_admits_after_a_later_maintenance_success() {
        let fake = FakeAudio::pattern_queue_full_once(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.maintain_audio();
        calls.clear();
        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(calls.snapshot().is_empty());
        assert!(app.status().contains("waiting for audio queue"));

        app.maintain_audio();
        calls.clear();
        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
            ]
        );
    }

    #[test]
    fn editing_an_installed_pattern_invalidates_transport_readiness_until_reinstalled() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        calls.clear();

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(calls.snapshot().is_empty());
        assert!(app.status().contains("pattern 1 update pending"));
    }

    #[test]
    fn selecting_an_unready_slot_never_admits_a_pattern_switch() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        calls.clear();

        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            app.patterns().selected_slot(),
            PatternSlotId::new(1).unwrap()
        );
        assert!(calls.snapshot().is_empty());
        assert!(app.status().contains("pattern 2 update pending"));
    }

    #[test]
    fn changing_slot_disarms_a_different_capture_before_selecting_and_next_pad_is_untracked() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        for _ in 0..32 {
            app.maintain_audio();
        }
        let captured = PatternSlotId::new(1).unwrap();
        app.patterns.select_slot(captured);
        app.patterns
            .start_recording(sampler_audio::TransportStamp {
                slot: captured,
                generation: 0,
                origin: 0,
                loop_frames: 96_000,
            })
            .unwrap();
        calls.clear();

        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply(InputAction::PadPress(0));

        assert!(!app.patterns().is_recording());
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SetRecordCapture(None),
                AudioCall::SelectPattern(PatternSlotId::new(2).unwrap(), PatternSwitch::Immediate),
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
    }

    #[test]
    fn selecting_the_current_pattern_cancels_a_pending_other_slot_capture() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        for _ in 0..32 {
            app.maintain_audio();
        }
        let captured = PatternSlotId::new(1).unwrap();
        app.patterns.select_slot(captured);
        app.patterns
            .start_recording(sampler_audio::TransportStamp {
                slot: captured,
                generation: 0,
                origin: 0,
                loop_frames: 96_000,
            })
            .unwrap();
        calls.clear();

        app.apply_key(key(',', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            app.patterns().selected_slot(),
            PatternSlotId::new(0).unwrap()
        );
        assert!(!app.patterns().is_recording());
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SetRecordCapture(None),
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
            ]
        );
    }

    #[test]
    fn capture_disarm_failure_aborts_slot_supersede_without_losing_recording() {
        let fake = FakeAudio::ready(48_000, 2).failing_capture_once("capture queue full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        for _ in 0..32 {
            app.maintain_audio();
        }
        let captured = PatternSlotId::new(1).unwrap();
        app.patterns.select_slot(captured);
        app.patterns
            .start_recording(sampler_audio::TransportStamp {
                slot: captured,
                generation: 0,
                origin: 0,
                loop_frames: 96_000,
            })
            .unwrap();
        calls.clear();

        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(app.patterns().selected_slot(), captured);
        assert!(app.patterns().is_recording());
        assert!(app.status().contains("capture queue full"));
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn palette_pattern_selection_uses_the_same_capture_disarm_reducer() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        for _ in 0..32 {
            app.maintain_audio();
        }
        let captured = PatternSlotId::new(1).unwrap();
        app.patterns.select_slot(captured);
        app.patterns
            .start_recording(sampler_audio::TransportStamp {
                slot: captured,
                generation: 0,
                origin: 0,
                loop_frames: 96_000,
            })
            .unwrap();
        calls.clear();

        app.apply_key(key(':', KeyModifiers::SHIFT, KeyEventKind::Press));
        app.apply_terminal_event(Event::Paste("pattern 3".into()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!app.patterns().is_recording());
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SetRecordCapture(None),
                AudioCall::SelectPattern(PatternSlotId::new(2).unwrap(), PatternSwitch::Immediate),
            ]
        );
    }

    #[test]
    fn control_r_arms_pattern_recording_while_plain_r_remains_a_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        calls.clear();

        app.apply_key(key('r', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(key('r', KeyModifiers::CONTROL, KeyEventKind::Press));
        app.apply_key(key('1', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 7), 64, 1.0),
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate,),
                AudioCall::PlayPattern,
                AudioCall::SetRecordCapture(Some((PatternSlotId::new(0).unwrap(), 0))),
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
            ]
        );
    }

    #[test]
    fn accepted_play_intent_makes_same_batch_slot_change_wait_for_the_next_boundary() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        app.maintain_audio();
        calls.clear();

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
                AudioCall::SelectPattern(
                    PatternSlotId::new(1).unwrap(),
                    PatternSwitch::NextBoundary,
                ),
            ]
        );
    }

    #[test]
    fn accepted_play_intent_makes_a_second_space_stop_before_telemetry_arrives() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        app.maintain_audio();
        calls.clear();

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
                AudioCall::SetRecordCapture(None),
                AudioCall::StopPattern,
            ]
        );
    }

    #[test]
    fn stop_all_replaces_a_pending_play_intent_before_telemetry_arrives() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        app.maintain_audio();
        calls.clear();

        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply(InputAction::StopAll);
        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
                AudioCall::StopAll,
                AudioCall::SelectPattern(PatternSlotId::new(1).unwrap(), PatternSwitch::Immediate),
            ]
        );
    }

    #[test]
    fn capture_disarms_before_a_failed_transport_stop() {
        let fake = FakeAudio::ready(48_000, 2).failing_stop_pattern_once("stop failed");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.maintain_audio();
        calls.clear();

        app.apply_key(key('r', KeyModifiers::CONTROL, KeyEventKind::Press));
        app.apply_key(key(' ', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(key('1', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(!app.patterns().is_recording());
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::SelectPattern(PatternSlotId::new(0).unwrap(), PatternSwitch::Immediate),
                AudioCall::PlayPattern,
                AudioCall::SetRecordCapture(Some((PatternSlotId::new(0).unwrap(), 0))),
                AudioCall::SetRecordCapture(None),
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
        assert_eq!(app.status(), "stop failed");
    }

    #[test]
    fn retry_at_a_new_rate_rebuilds_all_editable_pattern_slots() {
        let failed = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed));
        app.maintain_audio();

        app.retry_with(Box::new(FakeAudio::ready(44_100, 2)));

        assert_eq!(app.patterns().sample_rates(), [44_100; 16]);
    }

    #[test]
    fn device_modal_retry_wins_over_the_r_pad_key() {
        let mut app = App::without_audio("no output device");

        app.apply_key(key('r', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(app.device_retry_requests(), 1);
        assert_eq!(app.selected_pad(), 0);
    }

    #[test]
    fn dismissed_startup_and_runtime_failures_keep_an_explicit_retry_route() {
        let startup_failure = App::without_audio("no output device");
        let runtime_audio = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut runtime_failure = App::with_audio(Box::new(runtime_audio));
        runtime_failure.maintain_audio();

        for mut app in [startup_failure, runtime_failure] {
            app.apply_key(KeyEvent::new_with_kind(
                KeyCode::Esc,
                KeyModifiers::NONE,
                KeyEventKind::Press,
            ));
            assert!(app.status().contains("Ctrl+R"));

            app.apply_key(key('r', KeyModifiers::NONE, KeyEventKind::Press));
            assert_eq!(app.selected_pad(), 7);
            assert_eq!(app.device_retry_requests(), 0);

            app.open_help();
            app.apply_key(key('r', KeyModifiers::CONTROL, KeyEventKind::Press));
            assert_eq!(app.device_retry_requests(), 1);
        }
    }

    #[test]
    fn control_q_quits_even_when_a_modal_is_open() {
        let mut app = App::without_audio("no output device");

        app.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));

        assert!(app.should_quit());
    }

    #[test]
    fn pasted_text_only_changes_the_open_palette() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply_terminal_event(Event::Paste("stop-all".into()));
        assert!(calls.snapshot().is_empty());
        app.open_palette();
        app.apply_terminal_event(Event::Paste("stop-all".into()));

        assert_eq!(app.palette_text(), "stop-all");
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn pad_presses_remain_global_over_help_and_picker_overlays() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.open_help();
        app.apply_key(key('q', KeyModifiers::NONE, KeyEventKind::Press));
        app.close_overlay();
        app.open_picker();
        app.apply_key(key('q', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 4), 64, 1.0),
                AudioCall::Trigger(pad(0, 4), 64, 1.0),
            ]
        );
    }

    #[test]
    fn modal_overlay_does_not_swallow_a_held_pad_release() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });
        app.apply_key(key('1', KeyModifiers::NONE, KeyEventKind::Press));
        app.open_help();

        app.apply_key(key('1', KeyModifiers::NONE, KeyEventKind::Release));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::Release(pad(0, 0), 64),
            ]
        );
        assert!(!app.is_pad_held(0));
        assert_eq!(app.overlay(), Some(&super::Overlay::Help));
    }

    #[test]
    fn modal_overlay_does_not_swallow_shift_escape_stop_all() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.apply(InputAction::PadPress(0));
        app.open_help();

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::SHIFT,
            KeyEventKind::Press,
        ));

        assert_eq!(
            calls.snapshot(),
            [AudioCall::Trigger(pad(0, 0), 64, 1.0), AudioCall::StopAll]
        );
        assert!(!app.is_pad_held(0));
        assert_eq!(app.overlay(), Some(&super::Overlay::Help));
    }

    #[test]
    fn enter_triggers_the_selected_pad_in_perform_mode() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Right,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));

        assert_eq!(calls.snapshot(), [AudioCall::Trigger(pad(0, 1), 64, 1.0)]);
        assert!(!app.is_pad_held(1));
    }

    #[test]
    fn invalid_palette_command_stays_open_with_inline_error() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("select 0".into()));

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));

        assert_eq!(app.overlay(), Some(&super::Overlay::Palette));
        assert_eq!(app.palette_error(), Some("select expects 1..=16"));
    }

    #[test]
    fn palette_error_survives_multibyte_and_no_op_navigation_but_typing_clears_it() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("wat한".into()));
        let press = |code| KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Press);
        app.apply_key(press(KeyCode::Enter));
        let error = Some("unknown command: wat한");
        assert_eq!(app.palette_error(), error);

        app.apply_key(press(KeyCode::Left));
        assert_eq!(app.palette_cursor(), 3);
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::Right));
        assert_eq!(app.palette_cursor(), 6);
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::End));
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::Home));
        app.apply_key(press(KeyCode::Home));
        app.apply_key(press(KeyCode::Left));
        app.apply_key(press(KeyCode::Backspace));
        assert_eq!(app.palette_cursor(), 0);
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::End));
        app.apply_key(press(KeyCode::Delete));
        app.apply_terminal_event(Event::Paste(String::new()));
        assert_eq!(app.palette_cursor(), 6);
        assert_eq!(app.palette_error(), error);

        app.apply_key(key('x', KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(app.palette_text(), "wat한x");
        assert_eq!(app.palette_error(), None);
    }

    #[test]
    fn closing_the_palette_clears_its_inline_error() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("wat".into()));
        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        assert_eq!(app.palette_error(), Some("unknown command: wat"));

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));

        assert_eq!(app.overlay(), None);
        assert_eq!(app.palette_error(), None);
    }

    #[test]
    fn shifted_question_mark_opens_help_without_triggering_a_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply_key(key('?', KeyModifiers::SHIFT, KeyEventKind::Press));

        assert_eq!(app.overlay(), Some(&super::Overlay::Help));
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn picker_for_a_relative_filename_starts_in_the_current_directory() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.begin_load(pad(0, 0), path("kick.wav"));

        app.open_picker();

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one scan request")
        };
        assert_eq!(path, &std::env::current_dir().unwrap());
    }

    #[test]
    fn picker_resolves_a_nested_relative_source_and_backs_up_to_current_directory() {
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let request = app.begin_load(pad(0, 0), path("samples/kick.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };
        app.apply_worker_result(loaded(pad(0, 0), generation, "samples/kick.wav"));

        app.open_picker();

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one nested scan request")
        };
        assert!(path.is_absolute());
        assert_eq!(path, &current_dir.join("samples"));

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one parent scan request")
        };
        assert_eq!(path, &current_dir);
    }

    #[test]
    fn empty_relative_picker_directory_maps_to_current_directory_before_parent_navigation() {
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.open_picker_at("");

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one normalized scan request")
        };
        assert_eq!(path, &current_dir);

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one normalized parent scan request")
        };
        assert_eq!(path, current_dir.parent().unwrap());
        assert!(!path.as_os_str().is_empty());
    }

    #[test]
    fn relative_picker_directory_is_lexically_normalized() {
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.open_picker_at("samples/../drums/.");

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one normalized scan request")
        };
        assert_eq!(path, &current_dir.join("drums"));
    }

    #[test]
    fn repeated_hidden_toggles_queue_the_pending_directory_and_supersede_prior_scans() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/one");
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id,
                path: committed_path,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected committed-directory scan")
        };
        assert!(app.apply_worker_result(WorkerResult::Scanned {
            request_id: *request_id,
            path: committed_path.clone(),
            result: Ok(DirectoryScan::complete(Vec::new())),
        }));

        app.open_picker_at("/two");
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id: first_id,
                path: first_path,
                show_hidden: false,
            },
        ] = requests.as_slice()
        else {
            panic!("expected initial pending scan")
        };
        assert_eq!(first_path, path("/two"));

        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id: second_id,
                path: second_path,
                show_hidden: true,
            },
        ] = requests.as_slice()
        else {
            panic!("expected first hidden rescan")
        };
        assert_eq!(second_path, path("/two"));

        app.apply_key(key('.', KeyModifiers::NONE, KeyEventKind::Press));
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id: third_id,
                path: third_path,
                show_hidden: false,
            },
        ] = requests.as_slice()
        else {
            panic!("expected second hidden rescan")
        };
        assert_eq!(third_path, path("/two"));
        assert!(*first_id < *second_id && *second_id < *third_id);

        assert!(!app.apply_worker_result(WorkerResult::Scanned {
            request_id: *first_id,
            path: first_path.clone(),
            result: Ok(DirectoryScan::complete(Vec::new())),
        }));
        assert!(app.apply_worker_result(WorkerResult::Scanned {
            request_id: *third_id,
            path: third_path.clone(),
            result: Ok(DirectoryScan::complete(Vec::new())),
        }));
        assert_eq!(app.file_picker().directory(), path("/two"));
    }

    #[cfg(unix)]
    #[test]
    fn relative_picker_normalization_preserves_non_unicode_components() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        let relative = PathBuf::from(OsString::from_vec(vec![b's', 0x80, b'm', b'p']));
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.open_picker_at(relative.clone());

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one lossless scan request")
        };
        assert_eq!(path, &current_dir.join(relative));
    }

    #[test]
    fn picker_without_a_source_reopens_at_the_current_directory() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/other");
        app.close_overlay();
        app.take_worker_requests();

        app.open_picker();

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one scan request")
        };
        assert_eq!(path, &std::env::current_dir().unwrap());
    }

    #[test]
    fn stale_picker_error_for_the_same_directory_is_silent() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/samples");
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id: stale_id,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected one stale scan request")
        };
        let stale_id = *stale_id;
        app.open_picker_at("/samples");
        app.take_worker_requests();

        let applied = app.apply_worker_result(WorkerResult::Scanned {
            request_id: stale_id,
            path: "/samples".into(),
            result: Err("stale failure".to_owned()),
        });

        assert!(!applied);
        assert_eq!(app.status(), "");
    }

    #[test]
    fn rejected_current_scan_clears_pending_state_and_keeps_old_entries() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/samples");
        let request = app.take_worker_requests().pop().unwrap();

        app.apply_worker_send_error(request, WorkerSendError::WorkerBusy);

        assert!(!app.file_picker().is_scanning());
        assert_eq!(app.file_picker().failed_directory(), Some(path("/samples")));
        assert_eq!(app.file_picker().error(), Some("loader busy"));
        assert_eq!(app.status(), "loader busy");
    }

    #[test]
    fn rejected_stale_scan_for_the_same_directory_is_silent() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/samples");
        let stale = app.take_worker_requests().pop().unwrap();
        app.open_picker_at("/samples");
        app.take_worker_requests();

        assert!(!app.apply_worker_send_error(stale, WorkerSendError::WorkerBusy));

        assert!(app.file_picker().is_scanning());
        assert_eq!(app.status(), "");
    }

    #[test]
    fn sample_enter_confirms_apply_without_triggering_a_pad_and_escape_discards_explicitly() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        calls.clear();

        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(app.overlay(), Some(super::Overlay::ApplySample { pad: actual, .. }) if *actual == pad)
        );
        assert!(calls.snapshot().is_empty());

        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.overlay(), None);
        assert!(app.sample_editor().draft().normalize);
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.overlay(), Some(&super::Overlay::DiscardSample { pad }));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.sample_editor().draft().normalize);
    }

    #[test]
    fn sample_plain_z_stays_a_global_pad_and_control_z_is_editor_undo() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        calls.clear();

        app.apply_key(key('z', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(key('z', KeyModifiers::CONTROL, KeyEventKind::Press));

        assert_eq!(calls.snapshot(), [AudioCall::Trigger(pad(0, 12), 64, 1.0)]);
        assert_eq!(app.status(), "selected pad is empty");
    }

    #[test]
    fn failed_sample_setting_admission_keeps_pad_and_editor_settings_unchanged() {
        let fake = FakeAudio::ready(48_000, 2).failing_update_once("settings queue full");
        let mut app = App::with_audio(Box::new(fake));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let prior = app.pad(pad).settings;

        app.apply_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(app.pad(pad).settings, prior);
        assert_eq!(app.sample_editor().settings(), prior);
        assert_eq!(app.status(), "settings queue full");
    }

    #[test]
    fn palette_sample_commands_reject_an_empty_selected_pad() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("normalize on".into()));

        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.palette_error(), Some("selected pad is empty"));
    }

    #[test]
    fn dirty_sample_blocks_view_exit_and_pad_selection_until_discarded() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('2', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
        assert_eq!(app.selected_pad(), 0);
        assert_eq!(app.sample_editor().pad(), pad);
        assert!(
            matches!(app.overlay(), Some(super::Overlay::DiscardSample { pad: actual }) if *actual == pad)
        );
    }

    #[test]
    fn backtab_cycles_backward_through_every_workspace_and_keeps_shift_tab_compatibility() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Mixer);
        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Pattern);
        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Perform);

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Mixer);
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
    }

    #[test]
    fn backtab_uses_the_same_dirty_sample_discard_fence_as_shift_tab() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));

        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));

        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
        assert_eq!(app.overlay(), Some(&super::Overlay::DiscardSample { pad }));
    }

    #[test]
    fn sample_apply_pending_rejects_repeated_apply_and_undo_requests() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let pending_generation = app.sample_editor.pending[0].as_ref().unwrap().generation;

        for _ in 0..4 {
            app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            app.apply_key(key('z', KeyModifiers::CONTROL, KeyEventKind::Press));
        }

        assert_eq!(
            app.sample_editor.pending[0].as_ref().unwrap().generation,
            pending_generation
        );
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Pending
        ));
        assert_eq!(app.overlay(), None);
    }

    #[test]
    fn external_source_replacement_marks_a_dirty_editor_and_requires_discard_before_apply() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "first.wav").unwrap()
        else {
            panic!("expected initial load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "first.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));

        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "replacement.wav").unwrap()
        else {
            panic!("expected replacement load");
        };
        assert!(app.apply_worker_result(loaded_with_frames(
            pad,
            generation,
            "replacement.wav",
            48_000,
            1,
        )));

        assert_eq!(app.pad(pad).sample.as_ref().unwrap().frames(), 1);
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Error(
                crate::SampleEditorError::SelectedPadReplaced
            )
        ));
        assert_eq!(app.overlay(), None);
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.overlay(), None);
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.overlay(), None);
        let draft = app.sample_editor().draft();
        let settings = app.sample_editor().settings();
        for key_code in [
            KeyCode::Left,
            KeyCode::Char('n'),
            KeyCode::Up,
            KeyCode::Char('o'),
        ] {
            app.apply_key(KeyEvent::new(key_code, KeyModifiers::NONE));
        }
        app.open_palette();
        app.apply_terminal_event(Event::Paste("trim-start 0".into()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.sample_editor().draft(), draft);
        assert_eq!(app.sample_editor().settings(), settings);
        app.close_overlay();

        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.sample_editor().base_frames(), Some(1));
        assert!(!app.sample_editor().is_dirty());
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        assert!(app.sample_editor().draft().normalize);
    }

    #[test]
    fn failed_stale_and_rejected_user_loads_keep_the_committed_source_identity() {
        let setup = || {
            let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
            let pad = pad(0, 0);
            let WorkerRequest::LoadSample { generation, .. } =
                app.begin_load(pad, "first.wav").unwrap()
            else {
                panic!("expected initial load");
            };
            assert!(app.apply_worker_result(loaded(pad, generation, "first.wav")));
            let identity = app.sample_editor_context(pad).source_generation;
            (app, pad, identity)
        };

        let (mut decode_failed, pad, identity) = setup();
        let WorkerRequest::LoadSample { generation, .. } =
            decode_failed.begin_load(pad, "decode-failed.wav").unwrap()
        else {
            panic!("expected replacement load");
        };
        assert!(decode_failed.apply_worker_result(WorkerResult::Loaded {
            pad,
            generation,
            purpose: LoadPurpose::User,
            path: "decode-failed.wav".into(),
            result: Err(LoadSampleError::Decode("bad payload".to_owned())),
        }));
        assert_eq!(
            decode_failed.sample_editor_context(pad).source_generation,
            identity
        );

        let (mut stale, pad, identity) = setup();
        let stale_result = stale.begin_load(pad, "stale.wav").unwrap();
        let _newer = stale.begin_load(pad, "newer.wav").unwrap();
        let WorkerRequest::LoadSample { generation, .. } = stale_result else {
            panic!("expected stale load");
        };
        assert!(!stale.apply_worker_result(loaded(pad, generation, "stale.wav")));
        assert_eq!(stale.sample_editor_context(pad).source_generation, identity);

        let (mut rejected, pad, identity) = setup();
        let WorkerRequest::LoadSample { generation, .. } =
            rejected.begin_load(pad, "rejected.wav").unwrap()
        else {
            panic!("expected replacement load");
        };
        rejected.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_install("install rejected"),
        ));
        assert!(rejected.apply_worker_result(loaded(pad, generation, "rejected.wav")));
        assert_eq!(
            rejected.sample_editor_context(pad).source_generation,
            identity
        );
    }

    #[test]
    fn device_rate_recovery_keeps_the_committed_source_identity() {
        let mut app = App::with_audio(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        let pad = pad(0, 0);
        let settings = PadSettings {
            gain_db: -2.0,
            ..PadSettings::default()
        };
        app.update_pad_settings(pad, settings).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected initial load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        let identity = app.sample_editor_context(pad).source_generation;
        let revision = app.project_revision();
        let fingerprint = app.sample_editor.commits[0].fingerprint;
        let recipe = app.sample_editor.commits[0].recipe;
        assert!(app.maintain_audio());

        app.retry_with(Box::new(FakeAudio::ready(44_100, 2)));
        let request = app.take_worker_requests().pop().unwrap();
        let WorkerRequest::LoadSample {
            generation,
            purpose,
            path,
            ..
        } = request
        else {
            panic!("expected recovery load");
        };
        let mut recovery = loaded_with_purpose_recipe_and_frames(
            pad,
            generation,
            purpose,
            path.to_str().unwrap(),
            44_100,
            1,
            SampleEditRecipe::identity(),
        );
        let WorkerResult::Loaded {
            result: Ok(loaded), ..
        } = &mut recovery
        else {
            panic!("expected successful recovery result");
        };
        loaded.fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(std::path::Path::new("changed.wav"), &[1])
                .unwrap();
        assert!(app.apply_worker_result(recovery));

        assert_eq!(app.sample_editor_context(pad).source_generation, identity);
        assert_eq!(app.sample_editor.commits[0].fingerprint, fingerprint);
        assert_eq!(app.sample_editor.commits[0].recipe, recipe);
        assert_eq!(app.pad(pad).settings, settings);
        assert_eq!(app.project_revision(), revision);
    }

    #[test]
    fn recovery_result_precedes_same_path_user_result_without_consuming_the_user_slot() {
        let (mut app, pad, recipe, source_generation, recovery, user) =
            changed_rate_recovery_colliding_with_same_path_user_load();
        let WorkerRequest::LoadSample {
            generation: recovery_generation,
            purpose: recovery_purpose,
            path: recovery_path,
            ..
        } = recovery
        else {
            panic!("expected recovery load");
        };
        let WorkerRequest::LoadSample {
            generation: user_generation,
            purpose: user_purpose,
            path: user_path,
            ..
        } = user
        else {
            panic!("expected user load");
        };
        assert_eq!(recovery_generation, user_generation);
        assert_eq!(recovery_path, user_path);
        assert_eq!(recovery_purpose, LoadPurpose::Recovery);
        assert_eq!(user_purpose, LoadPurpose::User);

        assert!(
            app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad,
                recovery_generation,
                recovery_purpose,
                "same.wav",
                44_100,
                2,
                recipe,
            ))
        );

        assert!(app.committed_recovery_loads[0].is_none());
        assert!(matches!(
            app.pending_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::WorkerQueued)
        ));
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            source_generation
        );
        assert_eq!(app.sample_editor.commits[0].recipe, recipe);
        assert_eq!(app.base_sample(pad).unwrap().frames(), 2);

        assert!(
            app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad,
                user_generation,
                user_purpose,
                "same.wav",
                44_100,
                3,
                SampleEditRecipe::identity(),
            ))
        );
        assert!(app.pending_loads[0].is_none());
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            user_generation
        );
        assert_eq!(
            app.sample_editor.commits[0].recipe,
            SampleEditRecipe::identity()
        );
        assert_eq!(app.base_sample(pad).unwrap().frames(), 3);
    }

    #[test]
    fn same_path_user_result_precedes_recovery_without_restoring_the_old_recipe() {
        let (mut app, pad, recipe, _source_generation, recovery, user) =
            changed_rate_recovery_colliding_with_same_path_user_load();
        let WorkerRequest::LoadSample {
            generation: recovery_generation,
            purpose: recovery_purpose,
            ..
        } = recovery
        else {
            panic!("expected recovery load");
        };
        let WorkerRequest::LoadSample {
            generation: user_generation,
            purpose: user_purpose,
            ..
        } = user
        else {
            panic!("expected user load");
        };

        assert_eq!(recovery_purpose, LoadPurpose::Recovery);
        assert_eq!(user_purpose, LoadPurpose::User);
        assert!(
            app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad,
                user_generation,
                user_purpose,
                "same.wav",
                44_100,
                3,
                SampleEditRecipe::identity(),
            ))
        );
        let committed = Arc::clone(app.base_sample(pad).unwrap());
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            user_generation
        );
        assert_eq!(
            app.sample_editor.commits[0].recipe,
            SampleEditRecipe::identity()
        );
        assert!(app.committed_recovery_loads[0].is_none());

        assert!(
            !app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad,
                recovery_generation,
                recovery_purpose,
                "same.wav",
                44_100,
                2,
                recipe,
            ))
        );
        assert!(Arc::ptr_eq(app.base_sample(pad).unwrap(), &committed));
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            user_generation
        );
        assert_eq!(
            app.sample_editor.commits[0].recipe,
            SampleEditRecipe::identity()
        );
    }

    #[test]
    fn recovery_decode_error_does_not_fail_the_colliding_user_load() {
        let (mut app, pad, recipe, source_generation, recovery, user) =
            changed_rate_recovery_colliding_with_same_path_user_load();
        let WorkerRequest::LoadSample {
            generation: recovery_generation,
            purpose: recovery_purpose,
            path,
            ..
        } = recovery
        else {
            panic!("expected recovery load");
        };
        let WorkerRequest::LoadSample {
            generation: user_generation,
            purpose: user_purpose,
            ..
        } = user
        else {
            panic!("expected user load");
        };

        assert!(app.apply_worker_result(WorkerResult::Loaded {
            pad,
            generation: recovery_generation,
            purpose: recovery_purpose,
            path,
            result: Err(LoadSampleError::Decode("recovery decode failed".to_owned())),
        }));

        assert!(matches!(
            app.committed_recovery_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::Failed)
        ));
        assert!(matches!(
            app.pending_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::WorkerQueued)
        ));
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            source_generation
        );
        assert_eq!(app.sample_editor.commits[0].recipe, recipe);

        assert_eq!(recovery_purpose, LoadPurpose::Recovery);
        assert_eq!(user_purpose, LoadPurpose::User);
        assert!(
            app.apply_worker_result(loaded_with_purpose_recipe_and_frames(
                pad,
                user_generation,
                user_purpose,
                "same.wav",
                44_100,
                3,
                SampleEditRecipe::identity(),
            ))
        );
        assert_eq!(
            app.sample_editor_context(pad).source_generation,
            user_generation
        );
        assert_eq!(
            app.sample_editor.commits[0].recipe,
            SampleEditRecipe::identity()
        );
    }

    #[test]
    fn load_send_errors_mutate_only_the_colliding_request_slot() {
        let (mut busy_app, _pad, _recipe, source_generation, recovery, user) =
            changed_rate_recovery_colliding_with_same_path_user_load();
        assert!(busy_app.apply_worker_send_error(recovery, WorkerSendError::WorkerBusy));
        assert!(matches!(
            busy_app.committed_recovery_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::AwaitingWorker)
        ));
        assert!(matches!(
            busy_app.pending_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::WorkerQueued)
        ));
        assert!(busy_app.apply_worker_send_error(user, WorkerSendError::WorkerClosed));
        assert!(matches!(
            busy_app.committed_recovery_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::AwaitingWorker)
        ));
        assert!(matches!(
            busy_app.pending_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::Failed)
        ));
        assert_eq!(
            busy_app.sample_editor_context(pad(0, 0)).source_generation,
            source_generation
        );

        let (mut closed_app, _pad, recipe, source_generation, recovery, _user) =
            changed_rate_recovery_colliding_with_same_path_user_load();
        assert!(closed_app.apply_worker_send_error(recovery, WorkerSendError::WorkerClosed));
        assert!(matches!(
            closed_app.committed_recovery_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::Failed)
        ));
        assert!(matches!(
            closed_app.pending_loads[0]
                .as_deref()
                .map(|pending| &pending.phase),
            Some(super::PendingLoadPhase::WorkerQueued)
        ));
        assert_eq!(
            closed_app
                .sample_editor_context(pad(0, 0))
                .source_generation,
            source_generation
        );
        assert_eq!(closed_app.sample_editor.commits[0].recipe, recipe);
    }

    #[test]
    fn device_failure_and_retry_preserve_the_uncommitted_editor_draft() {
        let mut app = App::with_audio(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(app.maintain_audio());
        assert!(app.sample_editor().draft().normalize);
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Error(crate::SampleEditorError::DeviceUnavailable)
        ));

        app.retry_with(Box::new(FakeAudio::ready(48_000, 2)));
        assert!(app.sample_editor().draft().normalize);
        assert!(app.sample_editor().is_dirty());
    }

    #[test]
    fn apply_confirmation_rejects_a_replaced_source_without_queueing_an_edit() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "first.wav").unwrap()
        else {
            panic!("expected initial load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "first.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::ApplySample { .. })
        ));

        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "second.wav").unwrap()
        else {
            panic!("expected replacement load");
        };
        assert!(app.apply_worker_result(loaded_with_frames(
            pad,
            generation,
            "second.wav",
            48_000,
            2,
        )));
        let edit_generation = app.sample_editor.generations[0];

        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.sample_editor.generations[0], edit_generation);
        assert!(app.sample_editor.pending[0].is_none());
        assert!(app.sample_editor().draft().normalize);
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Error(
                crate::SampleEditorError::SelectedPadReplaced
            )
        ));
        assert_eq!(app.overlay(), None);
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.overlay(), None);
    }

    #[test]
    fn apply_confirmation_with_a_changed_editor_state_closes_without_queueing_work() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.editor
            .observe_error(crate::SampleEditorError::InstallFailed);
        let edit_generation = app.sample_editor.generations[0];

        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.sample_editor.generations[0], edit_generation);
        assert!(app.sample_editor.pending[0].is_none());
        assert_eq!(app.overlay(), None);
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Error(crate::SampleEditorError::InstallFailed)
        ));
    }

    #[test]
    fn apply_rejection_while_replacement_is_pending_closes_confirmation_once_and_keeps_draft() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "first.wav").unwrap()
        else {
            panic!("expected initial load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "first.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        assert!(app.sample_editor().draft().normalize);

        let replacement = app.begin_load(pad, "replacement.wav").unwrap();
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::ApplySample { .. })
        ));
        assert!(app.apply_sample_context.is_some());

        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(app.overlay(), None);
        assert!(app.apply_sample_context.is_none());
        assert!(app.sample_editor().draft().normalize);
        assert!(app.sample_editor.pending[0].is_none());
        let status = app.status().to_owned();
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.overlay(), None);
        assert_eq!(app.status(), status);

        let WorkerRequest::LoadSample { generation, .. } = replacement else {
            panic!("expected replacement load");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "replacement.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::ApplySample { .. })
        ));
    }

    #[test]
    fn empty_sample_keys_match_palette_rejection_without_mutating_settings() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let pad = pad(0, 0);
        let prior = app.pad(pad).settings;

        for key_code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('o'),
            KeyCode::Char('g'),
            KeyCode::Char('l'),
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Char('n'),
            KeyCode::Char('u'),
        ] {
            app.apply_key(KeyEvent::new(key_code, KeyModifiers::NONE));
        }

        assert_eq!(app.pad(pad).settings, prior);
        assert_eq!(app.sample_editor().settings(), prior);
        assert_eq!(app.status(), "selected pad is empty");

        app.apply_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.status(), "selected pad is empty");
    }

    #[test]
    fn pending_sample_edit_blocks_navigation_without_opening_discard() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } = app.begin_load(pad, "kick.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "kick.wav")));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
        app.apply_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.workspace_view(), WorkspaceView::Sample);
        assert_eq!(app.overlay(), None);
        assert!(matches!(
            app.sample_editor().status(),
            crate::WorkspaceSampleEditorStatus::Pending
        ));
    }

    #[test]
    fn palette_exact_trim_rejects_crossing_in_both_directions_without_mutating_the_draft() {
        for (first, crossing) in [
            ("trim-end 3", "trim-start 4"),
            ("trim-start 3", "trim-end 2"),
        ] {
            let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
            let pad = pad(0, 0);
            let WorkerRequest::LoadSample { generation, .. } =
                app.begin_load(pad, "seven.wav").unwrap()
            else {
                panic!("expected load request");
            };
            assert!(app.apply_worker_result(loaded_with_frames(
                pad,
                generation,
                "seven.wav",
                48_000,
                7,
            )));
            app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

            app.open_palette();
            app.apply_terminal_event(Event::Paste(first.to_owned()));
            app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert_eq!(app.palette_error(), None);
            let before = app.sample_editor().draft();

            app.open_palette();
            app.apply_terminal_event(Event::Paste(crossing.to_owned()));
            app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

            assert_eq!(
                app.palette_error(),
                Some("trim marker would cross the other marker")
            );
            assert_eq!(app.sample_editor().draft(), before);
        }
    }

    #[test]
    fn palette_exact_trim_round_trips_non_divisible_frame_counts() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "seven.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded_with_frames(
            pad,
            generation,
            "seven.wav",
            48_000,
            7,
        )));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        for command in ["trim-start 2", "trim-end 5"] {
            app.open_palette();
            app.apply_terminal_event(Event::Paste(command.to_owned()));
            app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert_eq!(app.palette_error(), None, "{command}");
        }

        assert_eq!(app.sample_editor().draft().frame_range(7).unwrap(), 2..5);
    }

    fn project_app() -> App {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "project.wav").unwrap()
        else {
            panic!("expected load request");
        };
        assert!(app.apply_worker_result(loaded_with_frames(
            pad,
            generation,
            "project.wav",
            48_000,
            7,
        )));
        app.patterns.set_view(WorkspaceView::Sample);
        app.sync_editor_to_selected_pad();
        app
    }

    #[test]
    fn project_revision_advances_only_for_committed_mutations() {
        let mut app = project_app();
        assert_eq!(app.project_revision(), 1);

        let revision = app.project_revision();
        app.patterns.toggle_view();
        app.patterns.select_slot(PatternSlotId::new(1).unwrap());
        app.patterns.move_cursor_steps(1);
        app.apply_telemetry(app.telemetry());
        assert_eq!(app.project_revision(), revision);

        app.patterns.select_slot(PatternSlotId::new(0).unwrap());
        for edit in [
            |patterns: &mut PatternWorkspace| patterns.toggle_step(),
            |patterns: &mut PatternWorkspace| patterns.toggle_step(),
            |patterns: &mut PatternWorkspace| {
                patterns.set_tempo(sampler_core::Tempo::new(124.0).unwrap())
            },
            |patterns: &mut PatternWorkspace| patterns.set_bars(2),
            |patterns: &mut PatternWorkspace| {
                patterns.set_resolution(sampler_core::Resolution::Eighth)
            },
            |patterns: &mut PatternWorkspace| patterns.set_swing(0.6),
            |patterns: &mut PatternWorkspace| patterns.set_quantize(0.5),
            |patterns: &mut PatternWorkspace| patterns.clear_selected(),
            |patterns: &mut PatternWorkspace| patterns.undo_clear(),
        ] {
            let before = app.project_revision();
            app.apply_pattern_edit(edit);
            assert_eq!(app.project_revision(), before + 1);
        }

        let before = app.project_revision();
        let settings = PadSettings {
            gain_db: -3.0,
            ..PadSettings::default()
        };
        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_update_once("settings rejected"),
        ));
        assert!(app.update_pad_settings(pad(0, 0), settings).is_err());
        assert_eq!(app.project_revision(), before);
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2)));
        app.update_pad_settings(pad(0, 0), settings).unwrap();
        assert_eq!(app.project_revision(), before + 1);
    }

    #[test]
    fn admitted_apply_and_undo_each_advance_one_project_revision() {
        let mut app = project_app();
        let pad = pad(0, 0);
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };

        let before_apply = app.project_revision();
        app.request_sample_edit(pad, recipe).unwrap();
        let requests = app.take_worker_requests();
        let [WorkerRequest::EditSample { generation, .. }] = requests.as_slice() else {
            panic!("expected apply edit request");
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            *generation,
            recipe,
            48_000,
            vec![-0.4, 0.4],
        )));
        assert!(app.maintain_audio());
        assert_eq!(app.project_revision(), before_apply + 1);

        let before_undo = app.project_revision();
        app.undo_sample_edit(pad).unwrap();
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::EditSample {
                generation,
                recipe: undo_recipe,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected undo edit request");
        };
        assert!(app.apply_worker_result(edited(
            &app,
            pad,
            *generation,
            *undo_recipe,
            48_000,
            vec![0.25, -0.25],
        )));
        assert!(app.maintain_audio());
        assert_eq!(app.project_revision(), before_undo + 1);
    }

    #[test]
    fn snapshot_refuses_dirty_or_pending_sample_state_and_uses_editable_patterns() {
        let mut app = project_app();
        app.editor_mut_for_test().move_marker(1, false);
        assert_eq!(
            app.project_snapshot(),
            Err(ProjectSnapshotError::DirtySampleDraft(pad(0, 0)))
        );

        app.discard_sample_draft();
        app.patterns_mut_for_test().toggle_step().unwrap();
        assert_eq!(app.project_snapshot().unwrap().patterns[0].events.len(), 1);

        let pending_pad = pad(0, 1);
        let _ = app.begin_load(pending_pad, "pending.wav");
        assert_eq!(
            app.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleLoad(pending_pad))
        );
    }

    #[test]
    fn rejected_audio_admission_keeps_tuple_and_revision_exact() {
        let mut app = project_app();
        let pad = pad(0, 0);
        let offset = super::pad_offset(pad);
        let before_revision = app.project_revision();
        let before_source = app.pads[offset].source.clone();
        let before_generation = app.sample_editor.commits[offset].source_generation;
        let before_recipe = app.sample_editor.commits[offset].recipe;
        let before_fingerprint = app.sample_editor.commits[offset].fingerprint;
        let before_settings = app.pads[offset].settings;

        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_install("install rejected"),
        ));
        let WorkerRequest::LoadSample { generation, .. } =
            app.begin_load(pad, "replacement.wav").unwrap()
        else {
            panic!("expected replacement request");
        };
        assert!(app.apply_worker_result(loaded(pad, generation, "replacement.wav")));

        assert_eq!(app.project_revision(), before_revision);
        assert_eq!(app.pads[offset].source, before_source);
        assert_eq!(
            app.sample_editor.commits[offset].source_generation,
            before_generation
        );
        assert_eq!(app.sample_editor.commits[offset].recipe, before_recipe);
        assert_eq!(
            app.sample_editor.commits[offset].fingerprint,
            before_fingerprint
        );
        assert_eq!(app.pads[offset].settings, before_settings);
    }

    #[test]
    fn snapshot_refuses_an_exact_pending_project_operation() {
        let mut app = project_app();
        let token = crate::ProjectToken::new(77);
        app.project_session
            .set_in_flight(Some(crate::ProjectOperationDescriptor {
                token,
                kind: crate::SaveKind::Explicit,
                project_id: app.project_session.project_id(),
                directory: "project".into(),
                revision: app.project_revision(),
            }));

        assert_eq!(
            app.project_snapshot(),
            Err(ProjectSnapshotError::PendingProjectOperation(token))
        );
    }

    #[test]
    fn exhausted_revision_refuses_mutation_without_partial_state_change() {
        let mut app = project_app();
        app.project_session
            .set_current_revision_for_test(i64::MAX as u64);
        let before_settings = app.pad(pad(0, 0)).settings;
        let before_generation = app.pad(pad(0, 0)).generation;
        let before_pattern = app.patterns.export_project_patterns().unwrap();

        let settings = PadSettings {
            gain_db: -6.0,
            ..before_settings
        };
        assert!(app.update_pad_settings(pad(0, 0), settings).is_err());
        app.apply_pattern_edit(|patterns| patterns.toggle_step());
        assert!(app.begin_load(pad(0, 0), "refused.wav").is_none());
        assert_eq!(
            app.request_sample_edit(
                pad(0, 0),
                SampleEditRecipe {
                    reversed: true,
                    ..SampleEditRecipe::identity()
                }
            ),
            Err(super::SampleEditRequestError::ProjectRevisionExhausted)
        );

        assert_eq!(app.pad(pad(0, 0)).settings, before_settings);
        assert_eq!(app.pad(pad(0, 0)).generation, before_generation);
        assert_eq!(
            app.patterns.export_project_patterns().unwrap(),
            before_pattern
        );
        assert_eq!(app.project_revision(), i64::MAX as u64);
    }

    #[test]
    fn device_rate_recovery_preserves_same_revision_project_snapshot() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.apply_pattern_edit(|patterns| patterns.toggle_step());
        let before_revision = app.project_revision();
        let before = app.project_snapshot().unwrap();

        assert!(app.retry_with(Box::new(FakeAudio::ready(44_100, 2))));

        assert_eq!(app.project_revision(), before_revision);
        assert_eq!(app.project_snapshot().unwrap(), before);
        assert_eq!(app.patterns.sample_rates(), [44_100; 16]);
    }

    #[test]
    fn unloaded_pad_settings_are_local_and_do_not_advance_project_revision() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let settings = PadSettings {
            gain_db: -4.0,
            ..PadSettings::default()
        };

        app.update_pad_settings(pad(0, 0), settings).unwrap();

        assert_eq!(app.pad(pad(0, 0)).settings, settings);
        assert_eq!(app.project_revision(), 0);
        assert!(app.project_snapshot().unwrap().pads.is_empty());
    }

    #[test]
    fn accepted_gate_record_advances_once_when_release_commits_complete_note() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pad = pad(0, 0);
        let stamp = TransportStamp {
            slot: PatternSlotId::new(0).unwrap(),
            generation: app.patterns.selected_pattern().generation(),
            origin: 1_000,
            loop_frames: app.patterns.selected_pattern().transport().loop_frames(),
        };
        app.patterns.start_recording(stamp).unwrap();
        app.patterns
            .note_live_trigger(0, LiveCommandId::FIRST, pad, 1.0);
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2).with_live_acks([
            LiveAck {
                id: LiveCommandId::FIRST,
                pad,
                kind: LiveAckKind::Trigger { velocity: 1.0 },
                frame: 1_120,
                transport: Some(stamp),
            },
        ])));
        let before_trigger = app.project_revision();
        assert!(app.maintain_audio());
        assert_eq!(app.project_revision(), before_trigger);
        assert!(
            app.project_snapshot().unwrap().patterns[0]
                .events
                .is_empty()
        );

        app.patterns.note_live_release(0, LiveCommandId::FIRST);
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2).with_live_acks([
            LiveAck {
                id: LiveCommandId::FIRST,
                pad,
                kind: LiveAckKind::Release,
                frame: 1_240,
                transport: Some(stamp),
            },
        ])));
        let before_release = app.project_revision();
        assert!(app.maintain_audio());
        assert_eq!(app.project_revision(), before_release + 1);
        assert_eq!(app.project_snapshot().unwrap().patterns[0].events.len(), 1);
        assert_eq!(
            app.project_snapshot().unwrap().patterns[0].events[0]
                .event
                .duration,
            Some(120)
        );
    }

    #[test]
    fn gate_overflow_at_zero_revision_budget_is_failure_atomic_and_future_recording_is_explicit() {
        let mut app = project_app();
        app.project_session
            .set_current_revision_for_test(crate::MAX_PROJECT_REVISION - 1);
        let target = pad(0, 0);
        let stamp = TransportStamp {
            slot: PatternSlotId::new(0).unwrap(),
            generation: app.patterns.selected_pattern().generation(),
            origin: 1_000,
            loop_frames: app.patterns.selected_pattern().transport().loop_frames(),
        };
        app.patterns.start_recording(stamp).unwrap();
        app.patterns
            .note_live_trigger(0, LiveCommandId::FIRST, target, 1.0);
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2).with_live_acks([
            LiveAck {
                id: LiveCommandId::FIRST,
                pad: target,
                kind: LiveAckKind::Trigger { velocity: 1.0 },
                frame: 1_120,
                transport: Some(stamp),
            },
        ])));
        app.maintain_audio();

        app.project_session
            .set_current_revision_for_test(crate::MAX_PROJECT_REVISION);
        app.project_session
            .mark_explicit_saved(crate::MAX_PROJECT_REVISION);
        app.project_session
            .mark_autosaved(crate::MAX_PROJECT_REVISION);
        let clean = app.project_snapshot().unwrap();
        app.patterns.note_live_release(0, midi_command(2));
        app.telemetry.live_ack_overflows = 1;
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2)));

        app.maintain_audio();

        assert_eq!(app.project_snapshot().unwrap(), clean);
        assert_eq!(app.project_revision(), crate::MAX_PROJECT_REVISION);
        assert_eq!(
            app.project_session.status(),
            crate::project_session::ProjectStatus::Clean
        );

        app.patterns
            .note_live_trigger(0, midi_command(3), target, 1.0);
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2).with_live_acks([
            LiveAck {
                id: midi_command(3),
                pad: target,
                kind: LiveAckKind::Trigger { velocity: 1.0 },
                frame: 1_240,
                transport: Some(stamp),
            },
        ])));
        app.maintain_audio();
        app.patterns.note_live_release(0, midi_command(4));
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2).with_live_acks([
            LiveAck {
                id: midi_command(4),
                pad: target,
                kind: LiveAckKind::Release,
                frame: 1_260,
                transport: Some(stamp),
            },
        ])));
        app.maintain_audio();

        assert_eq!(app.project_snapshot().unwrap(), clean);
        assert_eq!(app.patterns.pending_trigger_id(0), None);
        assert_eq!(app.project_revision(), crate::MAX_PROJECT_REVISION);
    }

    #[test]
    fn disconnected_loaded_pad_rejects_settings_without_changing_snapshot_or_revision() {
        let mut app = project_app();
        let pad = pad(0, 0);
        let before = app.project_snapshot().unwrap();
        let before_revision = app.project_revision();
        let before_settings = app.pad(pad).settings;
        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));
        assert!(app.maintain_audio());
        assert!(!app.current_session_bound[0]);

        let requested = PadSettings {
            gain_db: -8.0,
            ..before_settings
        };
        assert_eq!(
            app.update_pad_settings(pad, requested),
            Err("loaded sample is not admitted to the current audio session".to_owned())
        );

        assert_eq!(app.pad(pad).settings, before_settings);
        assert_eq!(app.project_revision(), before_revision);
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn replacement_decode_failure_restores_the_pre_request_snapshot() {
        let mut app = project_app();
        let pad = pad(0, 0);
        let before = app.project_snapshot().unwrap();
        let before_revision = app.project_revision();
        let WorkerRequest::LoadSample {
            generation,
            purpose,
            path,
            ..
        } = app.begin_load(pad, "broken.wav").unwrap()
        else {
            panic!("expected replacement load request");
        };
        assert_eq!(
            app.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleLoad(pad))
        );

        assert!(app.apply_worker_result(WorkerResult::Loaded {
            pad,
            generation,
            purpose,
            path,
            result: Err(LoadSampleError::Decode(
                "replacement decode failed".to_owned()
            )),
        }));

        assert!(app.status().contains("replacement decode failed"));
        assert_eq!(app.project_revision(), before_revision);
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn apply_render_failure_restores_the_pre_request_snapshot() {
        let mut app = project_app();
        let pad = pad(0, 0);
        let before = app.project_snapshot().unwrap();
        let before_revision = app.project_revision();
        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        app.request_sample_edit(pad, recipe).unwrap();
        let requests = app.take_worker_requests();
        let [WorkerRequest::EditSample { generation, .. }] = requests.as_slice() else {
            panic!("expected edit request");
        };
        assert_eq!(
            app.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleEdit(pad))
        );

        assert!(app.apply_worker_result(WorkerResult::Edited {
            pad,
            generation: *generation,
            recipe,
            result: Err("apply render failed".to_owned()),
        }));

        assert!(app.status().contains("apply render failed"));
        assert_eq!(app.project_revision(), before_revision);
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn snapshot_still_refuses_every_active_sample_operation_phase() {
        let pad = pad(0, 0);

        let mut awaiting_load = project_app();
        awaiting_load.audio = None;
        assert!(awaiting_load.begin_load(pad, "awaiting.wav").is_none());
        assert_eq!(
            awaiting_load.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleLoad(pad))
        );

        let mut ready_load = project_app();
        let WorkerRequest::LoadSample {
            generation,
            purpose,
            path,
            ..
        } = ready_load.begin_load(pad, "ready.wav").unwrap()
        else {
            panic!("expected ready load request");
        };
        ready_load.audio = None;
        assert!(ready_load.apply_worker_result(WorkerResult::Loaded {
            pad,
            generation,
            purpose,
            path,
            result: match loaded(pad, generation, "ready.wav") {
                WorkerResult::Loaded { result, .. } => result,
                _ => unreachable!(),
            },
        }));
        assert_eq!(
            ready_load.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleLoad(pad))
        );

        let recipe = SampleEditRecipe {
            reversed: true,
            ..SampleEditRecipe::identity()
        };
        let mut awaiting_edit = project_app();
        awaiting_edit.request_sample_edit(pad, recipe).unwrap();
        let [request] = awaiting_edit.take_worker_requests().try_into().unwrap();
        assert!(awaiting_edit.apply_worker_send_error(request, WorkerSendError::WorkerBusy));
        assert_eq!(
            awaiting_edit.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleEdit(pad))
        );

        let mut ready_edit = project_app();
        ready_edit.request_sample_edit(pad, recipe).unwrap();
        let requests = ready_edit.take_worker_requests();
        let [WorkerRequest::EditSample { generation, .. }] = requests.as_slice() else {
            panic!("expected ready edit request");
        };
        assert!(ready_edit.apply_worker_result(edited(
            &ready_edit,
            pad,
            *generation,
            recipe,
            48_000,
            vec![-0.5, 0.5],
        )));
        assert_eq!(
            ready_edit.project_snapshot(),
            Err(ProjectSnapshotError::PendingSampleEdit(pad))
        );
    }

    fn name_project(app: &mut App, directory: &str, now: Instant) -> sampler_core::ProjectId {
        let project_id = sampler_core::ProjectId::from_bytes([0x51; 16]);
        app.project_session = crate::ProjectSession::new(
            project_id,
            Some(directory.into()),
            "Beat",
            app.project_revision(),
        );
        app.project_session
            .commit_project_mutation(now, || Ok::<(), ()>(()))
            .unwrap();
        project_id
    }

    fn take_project_save(app: &mut App) -> ProjectSaveWorkerRequest {
        let requests = app.take_worker_requests();
        let [WorkerRequest::SaveProject(request)] = requests.as_slice() else {
            panic!("expected one project save request");
        };
        (**request).clone()
    }

    fn take_recovery_cleanup(app: &mut App) -> RecoveryCleanup {
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::DiscardRecovery {
                token,
                directory,
                project_id,
                revision,
            },
        ] = requests.as_slice()
        else {
            panic!("expected one recovery cleanup request");
        };
        RecoveryCleanup {
            token: *token,
            directory: directory.clone(),
            project_id: *project_id,
            revision: *revision,
        }
    }

    #[test]
    fn dirty_quit_save_waits_for_exact_success_and_failure_stays_open() {
        let now = Instant::now();
        let mut success = project_app();
        name_project(&mut success, "named", now);
        success.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));
        assert_eq!(
            success.overlay(),
            Some(&super::Overlay::UnsavedProject {
                action: super::ProjectAction::Quit,
            })
        );
        success.apply_key(key('y', KeyModifiers::NONE, KeyEventKind::Press));
        assert!(!success.should_quit());
        assert!(success.maintain_project(now));
        let request = take_project_save(&mut success);
        assert!(!success.should_quit());
        assert!(success.apply_worker_result(save_result(&request, Vec::new())));
        assert!(success.should_quit());

        let mut failure = project_app();
        name_project(&mut failure, "named", now);
        failure.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));
        failure.apply_key(key('y', KeyModifiers::NONE, KeyEventKind::Press));
        assert!(failure.maintain_project(now));
        let request = take_project_save(&mut failure);
        assert!(failure.apply_worker_result(save_error(&request, "save before quit")));
        assert!(!failure.should_quit());
        assert_eq!(
            failure.overlay(),
            Some(&super::Overlay::UnsavedProject {
                action: super::ProjectAction::Quit,
            })
        );
        assert!(failure.status().contains("save before quit"));
    }

    #[test]
    fn exact_save_and_quit_fences_a_queued_pattern_edit() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        app.patterns.set_view(WorkspaceView::Pattern);
        app.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));
        app.apply_key(key('y', KeyModifiers::NONE, KeyEventKind::Press));
        assert!(app.maintain_project(now));
        let request = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_result(&request, Vec::new())));
        assert!(app.should_quit());
        let saved_revision = app.project_revision();

        app.apply_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));

        assert_eq!(app.project_revision(), saved_revision);
    }

    fn queue_record_ack(app: &mut App, complete_gate: bool) -> TransportStamp {
        let pad = pad(0, 0);
        let stamp = TransportStamp {
            slot: PatternSlotId::new(0).unwrap(),
            generation: app.patterns.selected_pattern().generation(),
            origin: 1_000,
            loop_frames: app.patterns.selected_pattern().transport().loop_frames(),
        };
        app.patterns.start_recording(stamp).unwrap();
        app.patterns
            .note_live_trigger(0, LiveCommandId::FIRST, pad, 1.0);
        let mut acks = vec![LiveAck {
            id: LiveCommandId::FIRST,
            pad,
            kind: LiveAckKind::Trigger { velocity: 1.0 },
            frame: 1_120,
            transport: Some(stamp),
        }];
        if complete_gate {
            app.patterns.note_live_release(0, midi_command(2));
            acks.push(LiveAck {
                id: midi_command(2),
                pad,
                kind: LiveAckKind::Release,
                frame: 1_240,
                transport: Some(stamp),
            });
        }
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2).with_live_acks(acks)));
        stamp
    }

    #[test]
    fn save_before_quit_reconfirms_if_record_ack_advances_past_the_snapshot() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        queue_record_ack(&mut app, true);
        app.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));
        app.apply_key(key('y', KeyModifiers::NONE, KeyEventKind::Press));
        assert!(app.maintain_project(now));
        let request = take_project_save(&mut app);
        let saved_revision = request.request.snapshot.revision;

        assert!(app.maintain_audio());
        assert_eq!(app.project_revision(), saved_revision + 1);
        assert!(app.apply_worker_result(save_result(&request, Vec::new())));

        assert!(!app.should_quit());
        assert_eq!(app.project_session.saved_revision(), saved_revision);
        assert_eq!(app.project_revision(), saved_revision + 1);
        assert_eq!(
            app.overlay(),
            Some(&super::Overlay::UnsavedProject {
                action: super::ProjectAction::Quit,
            })
        );
    }

    #[test]
    fn post_quit_input_fence_still_accepts_stop_all_and_pad_release() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });
        app.apply_key(key('1', KeyModifiers::NONE, KeyEventKind::Press));
        app.should_quit = true;

        app.apply_key(key('1', KeyModifiers::NONE, KeyEventKind::Release));
        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::SHIFT,
            KeyEventKind::Press,
        ));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::Release(pad(0, 0), 64),
                AudioCall::StopAll,
            ]
        );
    }

    #[test]
    fn untitled_dirty_quit_save_stays_open_with_save_as_instruction() {
        let mut app = project_app();
        app.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));
        app.apply_key(key('y', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(!app.should_quit());
        assert_eq!(
            app.overlay(),
            Some(&super::Overlay::UnsavedProject {
                action: super::ProjectAction::Quit,
            })
        );
        assert!(app.status().contains("save-as <directory>"));
        assert!(app.take_worker_requests().is_empty());
    }

    #[test]
    fn discard_and_quit_waits_for_exact_newer_recovery_deletion() {
        let now = Instant::now();
        let mut app = project_app();
        let project_id = name_project(&mut app, "named", now);
        let revision = app.project_revision();
        app.project_session.mark_autosaved(revision);
        app.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(!app.should_quit());
        assert!(app.maintain_project(now));
        let cleanup = take_recovery_cleanup(&mut app);
        assert_eq!(cleanup.project_id, project_id);
        assert_eq!(cleanup.revision, revision);
        assert!(!app.should_quit());
        assert!(!app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token: crate::ProjectToken::new(cleanup.token.get() + 1),
            directory: cleanup.directory.clone(),
            project_id,
            revision,
            result: Ok(()),
        }));
        assert!(!app.should_quit());
        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token: cleanup.token,
            directory: cleanup.directory,
            project_id,
            revision,
            result: Ok(()),
        }));
        assert!(app.should_quit());
    }

    #[test]
    fn discard_before_quit_reconfirms_if_release_ack_advances_past_the_action_revision() {
        let now = Instant::now();
        let mut app = project_app();
        let stamp = queue_record_ack(&mut app, false);
        assert!(app.maintain_audio());
        let project_id = name_project(&mut app, "named", now);
        let action_revision = app.project_revision();
        app.project_session.mark_autosaved(action_revision);
        app.patterns.note_live_release(0, midi_command(2));
        app.audio = Some(Box::new(FakeAudio::ready(48_000, 2).with_live_acks([
            LiveAck {
                id: midi_command(2),
                pad: pad(0, 0),
                kind: LiveAckKind::Release,
                frame: 1_240,
                transport: Some(stamp),
            },
        ])));
        app.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        assert!(app.maintain_project(now));
        let cleanup = take_recovery_cleanup(&mut app);

        assert!(app.maintain_audio());
        assert_eq!(app.project_revision(), action_revision + 1);
        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token: cleanup.token,
            directory: cleanup.directory,
            project_id,
            revision: cleanup.revision,
            result: Ok(()),
        }));

        assert!(!app.should_quit());
        assert_eq!(app.project_revision(), action_revision + 1);
        assert_eq!(
            app.overlay(),
            Some(&super::Overlay::UnsavedProject {
                action: super::ProjectAction::Quit,
            })
        );
        assert_eq!(
            app.project_session.autosaved_revision(),
            app.project_session.saved_revision()
        );
        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        assert!(app.should_quit());
        assert!(app.take_worker_requests().is_empty());
    }

    #[test]
    fn sample_draft_discard_and_apply_resolve_before_dirty_quit_choice() {
        let now = Instant::now();
        let mut discard = project_app();
        name_project(&mut discard, "named", now);
        discard.editor_mut_for_test().move_marker(1, false);
        discard.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));
        assert_eq!(
            discard.overlay(),
            Some(&super::Overlay::ResolveSampleDraft {
                pad: pad(0, 0),
                action: super::ProjectAction::Quit,
            })
        );
        discard.apply_key(KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        assert!(!discard.sample_editor().is_dirty());
        assert_eq!(
            discard.overlay(),
            Some(&super::Overlay::UnsavedProject {
                action: super::ProjectAction::Quit,
            })
        );

        let mut apply = project_app();
        name_project(&mut apply, "named", now);
        apply.editor_mut_for_test().move_marker(1, false);
        apply.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));
        apply.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        let requests = apply.take_worker_requests();
        let [
            WorkerRequest::EditSample {
                generation, recipe, ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected sample draft apply request");
        };
        assert!(apply.apply_worker_result(edited(
            &apply,
            pad(0, 0),
            *generation,
            *recipe,
            48_000,
            vec![0.25, -0.25],
        )));
        assert!(apply.maintain_audio());
        assert_eq!(
            apply.overlay(),
            Some(&super::Overlay::UnsavedProject {
                action: super::ProjectAction::Quit,
            })
        );
        assert!(!apply.should_quit());
    }

    fn begin_quit_draft_apply(app: &mut App, now: Instant) -> WorkerRequest {
        name_project(app, "named", now);
        app.editor_mut_for_test().move_marker(1, false);
        app.apply_key(key('q', KeyModifiers::CONTROL, KeyEventKind::Press));
        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        let requests = app.take_worker_requests();
        let [request @ WorkerRequest::EditSample { .. }] = requests.as_slice() else {
            panic!("expected lifecycle sample edit request");
        };
        request.clone()
    }

    #[test]
    fn draft_apply_render_failure_returns_to_cancelable_resolution() {
        let now = Instant::now();
        let mut app = project_app();
        let request = begin_quit_draft_apply(&mut app, now);
        let WorkerRequest::EditSample {
            pad,
            generation,
            recipe,
            ..
        } = request
        else {
            unreachable!()
        };

        assert!(app.apply_worker_result(WorkerResult::Edited {
            pad,
            generation,
            recipe,
            result: Err("render failed".to_owned()),
        }));

        assert_eq!(app.project_lifecycle_wait, None);
        assert!(app.pending_project_action.is_some());
        assert!(app.sample_editor().is_dirty());
        assert_eq!(
            app.overlay(),
            Some(&super::Overlay::ResolveSampleDraft {
                pad,
                action: super::ProjectAction::Quit,
            })
        );
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.overlay(), None);
        assert!(app.pending_project_action.is_none());
        assert!(!app.should_quit());
    }

    #[test]
    fn draft_apply_closed_worker_returns_to_cancelable_resolution() {
        let now = Instant::now();
        let mut app = project_app();
        let request = begin_quit_draft_apply(&mut app, now);
        let pad = match &request {
            WorkerRequest::EditSample { pad, .. } => *pad,
            _ => unreachable!(),
        };

        assert!(app.apply_worker_send_error(request, WorkerSendError::WorkerClosed));

        assert_eq!(app.project_lifecycle_wait, None);
        assert!(app.pending_project_action.is_some());
        assert_eq!(
            app.overlay(),
            Some(&super::Overlay::ResolveSampleDraft {
                pad,
                action: super::ProjectAction::Quit,
            })
        );
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.pending_project_action.is_none());
    }

    #[test]
    fn draft_apply_device_failure_exposes_error_then_cancelable_resolution() {
        let now = Instant::now();
        let mut app = project_app();
        let _request = begin_quit_draft_apply(&mut app, now);
        app.audio = Some(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device disconnected"),
        ));

        assert!(app.maintain_audio());

        assert_eq!(app.project_lifecycle_wait, None);
        assert!(app.pending_project_action.is_some());
        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::DeviceError(_))
        ));
        assert!(app.retry_with(Box::new(FakeAudio::ready(48_000, 2))));
        assert!(matches!(
            app.overlay(),
            Some(super::Overlay::ResolveSampleDraft { .. })
        ));
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.pending_project_action.is_none());
        assert!(!app.should_quit());
    }

    #[test]
    fn palette_open_project_prompts_before_replacing_a_modified_project() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        app.open_palette();
        app.apply_terminal_event(Event::Paste("open-project next-project".to_owned()));
        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        assert_eq!(
            app.overlay(),
            Some(&super::Overlay::UnsavedProject {
                action: super::ProjectAction::Open,
            })
        );
        assert!(app.take_worker_requests().is_empty());

        app.apply_key(key('n', KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(app.overlay(), Some(&super::Overlay::ProjectOpenProgress));
        let requests = app.take_worker_requests();
        let [WorkerRequest::ProbeProject { directory, .. }] = requests.as_slice() else {
            panic!("expected open probe after discard choice");
        };
        assert_eq!(directory, path("next-project"));
    }

    #[test]
    fn palette_save_on_untitled_project_closes_to_the_actionable_status() {
        let mut app = project_app();
        app.open_palette();
        app.apply_terminal_event(Event::Paste("save".to_owned()));
        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));

        assert_eq!(app.overlay(), None);
        assert_eq!(app.status(), "Untitled project: use save-as <directory>");
    }

    fn save_result(
        request: &ProjectSaveWorkerRequest,
        mappings: Vec<ProjectAssetMapping>,
    ) -> WorkerResult {
        let save = &request.request;
        WorkerResult::ProjectSaved {
            token: request.token,
            kind: save.kind,
            project_id: save.snapshot.project_id,
            directory: save.directory.clone(),
            revision: save.snapshot.revision,
            result: Ok(SaveReceipt {
                directory: save.directory.clone(),
                kind: save.kind,
                project_id: save.snapshot.project_id,
                revision: save.snapshot.revision,
                canonical_toml: "saved".to_owned(),
                mappings,
            }),
        }
    }

    fn save_error(request: &ProjectSaveWorkerRequest, message: &'static str) -> WorkerResult {
        let save = &request.request;
        WorkerResult::ProjectSaved {
            token: request.token,
            kind: save.kind,
            project_id: save.snapshot.project_id,
            directory: save.directory.clone(),
            revision: save.snapshot.revision,
            result: Err(ProjectStoreError::Filesystem {
                operation: message,
                path: save.directory.clone(),
                kind: std::io::ErrorKind::PermissionDenied,
            }),
        }
    }

    fn project_open_document(
        project_id: sampler_core::ProjectId,
        name: &str,
        revision: u64,
        pads: Vec<sampler_core::ProjectPad>,
    ) -> sampler_core::ProjectDocument {
        sampler_core::ProjectDocument::new_v4(
            project_id,
            name,
            revision,
            pads,
            PatternWorkspace::new(48_000)
                .export_project_patterns()
                .unwrap(),
            sampler_core::MasterMixSettings::default(),
            sampler_core::MidiSettings::default(),
        )
        .unwrap()
    }

    fn project_open_document_with_midi(
        project_id: sampler_core::ProjectId,
        name: &str,
        revision: u64,
        pads: Vec<sampler_core::ProjectPad>,
        midi: MidiSettings,
    ) -> sampler_core::ProjectDocument {
        sampler_core::ProjectDocument::new_v4(
            project_id,
            name,
            revision,
            pads,
            PatternWorkspace::new(48_000)
                .export_project_patterns()
                .unwrap(),
            sampler_core::MasterMixSettings::default(),
            midi,
        )
        .unwrap()
    }

    fn project_open_pad(pad: PadId, settings: PadSettings) -> sampler_core::ProjectPad {
        let fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(path("fixture.wav"), &[]).unwrap();
        sampler_core::ProjectPad::new(
            pad,
            format!("audio/{}.wav", fingerprint.digest),
            fingerprint.digest,
            settings,
            sampler_core::PadMixSettings::default(),
            SampleEditRecipe::identity(),
        )
        .unwrap()
    }

    fn staged_project_result(
        request: &crate::StageProjectSampleRequest,
        fingerprint: crate::SourceFingerprint,
    ) -> WorkerResult {
        let rendered = Arc::new(SampleBuffer::new(request.engine_rate, vec![0.25, -0.25]).unwrap());
        WorkerResult::ProjectSampleStaged {
            token: request.token,
            generation: request.generation,
            pad: request.pad,
            revision: request.revision,
            path: request.path.clone(),
            recipe: request.recipe,
            result: Ok(LoadedSample {
                fingerprint,
                base: Arc::clone(&rendered),
                base_preview: Arc::new([PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS]),
                rendered,
                rendered_preview: Arc::new(
                    [PreviewColumn { min: -2, max: 2 }; EDIT_PREVIEW_COLUMNS],
                ),
                recipe: request.recipe,
                source_rate: request.engine_rate,
                source_frames: 1,
                duration: Duration::from_secs_f64(1.0 / f64::from(request.engine_rate)),
            }),
        }
    }

    fn stage_project_open(app: &mut App, directory: &str, document: sampler_core::ProjectDocument) {
        let token = app.request_open_project(directory).unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: directory.into(),
            result: Ok(crate::ProjectProbe {
                directory: directory.into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        let fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(path("fixture.wav"), &[]).unwrap();
        while app.project_open_stage().unwrap().staged_pads
            < app.project_open_stage().unwrap().total_pads
        {
            assert!(app.maintain_project(Instant::now()));
            let requests = app.take_worker_requests();
            let [WorkerRequest::StageProjectSample(request)] = requests.as_slice() else {
                panic!("expected one staged decode request");
            };
            assert!(app.apply_worker_result(staged_project_result(request, fingerprint)));
        }
    }

    #[test]
    fn project_open_stale_probe_and_cancel_preserve_the_complete_old_tuple() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let token = app.request_open_project("project-b").unwrap();
        let stale = crate::ProjectToken::new(token.get() + 1);
        let candidate = project_open_document(
            sampler_core::ProjectId::from_bytes([0x72; 16]),
            "Project B",
            9,
            Vec::new(),
        );

        assert!(!app.apply_worker_result(WorkerResult::ProjectProbed {
            token: stale,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(candidate.clone())),
                recovery: None,
            }),
        }));
        assert_eq!(app.project_snapshot().unwrap(), before);
        app.cancel_project_open().unwrap();
        assert_eq!(app.project_snapshot().unwrap(), before);
        assert!(!app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(candidate)),
                recovery: None,
            }),
        }));
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_refuses_unresolved_sample_state_before_allocating_a_probe() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let pending_pad = pad(0, 2);
        assert!(app.begin_load(pending_pad, "pending.wav").is_some());

        assert!(matches!(
            app.request_open_project("project-b"),
            Err(crate::ProjectOpenError::UnresolvedState(_))
        ));
        assert!(app.project_open_stage().is_none());
        assert!(app.take_worker_requests().is_empty());
    }

    #[test]
    fn project_open_stages_one_asset_per_maintenance_without_audio_commands() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let project_id = sampler_core::ProjectId::from_bytes([0x73; 16]);
        let pads = vec![
            project_open_pad(pad(0, 0), PadSettings::default()),
            project_open_pad(
                pad(0, 1),
                PadSettings::new(PlaybackMode::Gate, -3.0, 0.25, 2.0, None).unwrap(),
            ),
        ];
        let document = project_open_document(project_id, "Project B", 4, pads);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));

        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(first)] = requests.as_slice() else {
            panic!("expected exactly one staged decode request");
        };
        assert_eq!(first.pad, pad(0, 0));
        assert!(calls.snapshot().is_empty());
        assert!(!app.maintain_project(Instant::now()));
        assert!(app.take_worker_requests().is_empty());
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn project_open_worker_backpressure_then_device_loss_pauses_staging_without_panicking() {
        let audio = FakeAudio::ready(48_000, 2).failing_runtime("device lost");
        let mut app = App::with_audio(Box::new(audio));
        let before = app.project_snapshot().unwrap();
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x8a; 16]),
            "Project B",
            5,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        assert!(app.maintain_project(Instant::now()));
        let [request] = app.take_worker_requests().try_into().unwrap();
        assert!(app.apply_worker_send_error(request, WorkerSendError::WorkerBusy));
        assert!(app.maintain_audio());
        assert_eq!(app.audio_format(), None);

        let maintained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.maintain_project(Instant::now())
        }));
        assert!(matches!(maintained, Ok(false)));
        assert!(app.take_worker_requests().is_empty());
        assert_eq!(app.project_snapshot().unwrap(), before);
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::Staging
        );

        assert!(app.retry_with(Box::new(FakeAudio::ready(48_000, 2))));
        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(request)] = requests.as_slice() else {
            panic!("expected the paused stage request to restart");
        };
        assert_eq!(request.token, token);
        assert_eq!(request.pad, pad(0, 0));
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_rate_change_ignores_in_flight_old_rate_result_and_reissues_the_pad() {
        let audio = FakeAudio::ready(48_000, 2).failing_runtime("device lost");
        let mut app = App::with_audio(Box::new(audio));
        let before = app.project_snapshot().unwrap();
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x91; 16]),
            "Project B",
            5,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(first_request)] = requests.as_slice() else {
            panic!("expected the first 48 kHz stage request");
        };
        assert_eq!(first_request.engine_rate, 48_000);
        let first_request = (**first_request).clone();

        assert!(app.maintain_audio());
        assert!(app.retry_with(Box::new(FakeAudio::ready(44_100, 2))));
        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(reissued)] = requests.as_slice() else {
            panic!("expected the pad to be reissued at 44.1 kHz");
        };
        assert_eq!(reissued.token, token);
        assert_ne!(reissued.generation, first_request.generation);
        assert_eq!(reissued.pad, pad(0, 0));
        assert_eq!(reissued.engine_rate, 44_100);
        let reissued = (**reissued).clone();
        let fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(path("fixture.wav"), &[]).unwrap();
        assert!(!app.apply_worker_result(staged_project_result(&first_request, fingerprint,)));
        assert!(app.project_open_error().is_none());
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::Staging
        );
        assert_eq!(app.project_snapshot().unwrap(), before);

        assert!(app.apply_worker_result(staged_project_result(&reissued, fingerprint)));
        assert_eq!(app.project_open_stage().unwrap().staged_pads, 1);
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_probe_failure_is_retained_as_the_exact_typed_error() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        let error = ProjectStoreError::Filesystem {
            operation: "probe project",
            path: "project-b".into(),
            kind: std::io::ErrorKind::PermissionDenied,
        };

        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Err(error.clone()),
        }));

        assert_eq!(
            app.project_open_error(),
            Some(&crate::ProjectOpenError::Probe(error))
        );
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_build_failures_are_retained_as_typed_errors() {
        let mut unavailable = App::with_audio(Box::new(
            FakeAudio::ready(48_000, 2).failing_runtime("device lost"),
        ));
        let before = unavailable.project_snapshot().unwrap();
        let token = unavailable.request_open_project("project-b").unwrap();
        unavailable.take_worker_requests();
        assert!(unavailable.maintain_audio());
        assert!(
            unavailable.apply_worker_result(WorkerResult::ProjectProbed {
                token,
                directory: "project-b".into(),
                result: Ok(crate::ProjectProbe {
                    directory: "project-b".into(),
                    explicit: Some(Ok(project_open_document(
                        sampler_core::ProjectId::from_bytes([0x8b; 16]),
                        "Project B",
                        3,
                        Vec::new(),
                    ))),
                    recovery: None,
                }),
            })
        );
        assert_eq!(
            unavailable.project_open_error(),
            Some(&crate::ProjectOpenError::AudioUnavailable)
        );
        assert_eq!(unavailable.project_snapshot().unwrap(), before);

        let mut invalid = project_app();
        let before = invalid.project_snapshot().unwrap();
        let token = invalid.request_open_project("project-c").unwrap();
        invalid.take_worker_requests();
        let mut document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x8c; 16]),
            "Project C",
            4,
            Vec::new(),
        );
        document.patterns[0].name.clear();
        assert!(invalid.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-c".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-c".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        assert!(matches!(
            invalid.project_open_error(),
            Some(crate::ProjectOpenError::InvalidPatterns(_))
        ));
        assert_eq!(invalid.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_rejects_malformed_mixer_document_before_staging_or_audio_admission() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let before = app.project_snapshot().unwrap();
        let token = app.request_open_project("project-invalid-mixer").unwrap();
        app.take_worker_requests();
        let mut document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x9a; 16]),
            "Invalid mixer",
            7,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        document.master_mix.gain_db = f32::NAN;

        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-invalid-mixer".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-invalid-mixer".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));

        assert!(matches!(
            app.project_open_error(),
            Some(crate::ProjectOpenError::InvalidDocument(_))
        ));
        assert_eq!(app.project_snapshot().unwrap(), before);
        assert!(calls.snapshot().is_empty());
        assert!(app.take_worker_requests().is_empty());
    }

    #[test]
    fn project_open_stage_failure_is_retained_as_the_exact_typed_error() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x8d; 16]),
            "Project B",
            5,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(request)] = requests.as_slice() else {
            panic!("expected one staged decode request");
        };
        let load_error = LoadSampleError::Prepare("recipe failed".to_owned());
        assert!(app.apply_worker_result(WorkerResult::ProjectSampleStaged {
            token: request.token,
            generation: request.generation,
            pad: request.pad,
            revision: request.revision,
            path: request.path.clone(),
            recipe: request.recipe,
            result: Err(load_error.clone()),
        }));
        assert_eq!(
            app.project_open_error(),
            Some(&crate::ProjectOpenError::Stage {
                pad: pad(0, 0),
                error: crate::ProjectStageError::Load(load_error),
            })
        );
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_rejects_stale_and_digest_mismatched_stage_results_atomically() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let before = app.project_snapshot().unwrap();
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x74; 16]),
            "Project B",
            5,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(request)] = requests.as_slice() else {
            panic!("expected staged decode request");
        };
        let mut stale_request = (**request).clone();
        stale_request.token = crate::ProjectToken::new(token.get() + 1);
        let exact_fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(path("fixture.wav"), &[]).unwrap();
        assert!(
            !app.apply_worker_result(staged_project_result(&stale_request, exact_fingerprint,))
        );
        assert!(app.project_open_stage().is_some());

        let mut mismatched = exact_fingerprint;
        mismatched.digest = sampler_core::AssetDigest::from_bytes([0x99; 32]);
        assert!(app.apply_worker_result(staged_project_result(request, mismatched)));
        assert!(app.project_open_stage().is_none());
        assert_eq!(app.project_snapshot().unwrap(), before);
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn stale_project_stage_result_cannot_consume_the_newer_midi_candidate() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let first_midi = MidiSettings::default()
            .learn_swap(BankId::new(1).unwrap(), 0, midi_note(80))
            .unwrap();
        let first = project_open_document_with_midi(
            sampler_core::ProjectId::from_bytes([0xb1; 16]),
            "First",
            7,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
            first_midi,
        );
        let first_token = app.request_open_project("first").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token: first_token,
            directory: "first".into(),
            result: Ok(crate::ProjectProbe {
                directory: "first".into(),
                explicit: Some(Ok(first)),
                recovery: None,
            }),
        }));
        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(first_request)] = requests.as_slice() else {
            panic!("expected first stage request")
        };
        let first_request = (**first_request).clone();
        app.cancel_project_open().unwrap();

        let newer_midi = MidiSettings::default()
            .with_channel(MidiChannelFilter::Channel(midi_channel(12)))
            .learn_swap(BankId::new(8).unwrap(), 4, midi_note(101))
            .unwrap();
        let newer = project_open_document_with_midi(
            sampler_core::ProjectId::from_bytes([0xb2; 16]),
            "Newer",
            8,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
            newer_midi,
        );
        let newer_token = app.request_open_project("newer").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token: newer_token,
            directory: "newer".into(),
            result: Ok(crate::ProjectProbe {
                directory: "newer".into(),
                explicit: Some(Ok(newer)),
                recovery: None,
            }),
        }));
        assert!(app.maintain_project(Instant::now()));
        let requests = app.take_worker_requests();
        let [WorkerRequest::StageProjectSample(newer_request)] = requests.as_slice() else {
            panic!("expected newer stage request")
        };
        let newer_request = (**newer_request).clone();
        let fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(path("fixture.wav"), &[]).unwrap();

        assert!(!app.apply_worker_result(staged_project_result(&first_request, fingerprint,)));
        let Some(super::ProjectOpenOperation::Staging(candidate)) = app.project_open.as_ref()
        else {
            panic!("newer candidate must remain staged")
        };
        assert_eq!(candidate.document.midi, newer_midi);
        assert_eq!(
            candidate.decode_in_flight,
            Some((pad(0, 0), newer_request.generation))
        );
    }

    #[test]
    fn project_open_collects_all_exact_stage_results_before_audio_admission() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x75; 16]),
            "Project B",
            6,
            vec![
                project_open_pad(pad(0, 0), PadSettings::default()),
                project_open_pad(pad(0, 1), PadSettings::default()),
            ],
        );
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(document)),
                recovery: None,
            }),
        }));
        let fingerprint =
            crate::SourceFingerprint::from_encoded_bytes(path("fixture.wav"), &[]).unwrap();
        for expected in [pad(0, 0), pad(0, 1)] {
            assert!(app.maintain_project(Instant::now()));
            let requests = app.take_worker_requests();
            let [WorkerRequest::StageProjectSample(request)] = requests.as_slice() else {
                panic!("expected one staged decode request");
            };
            assert_eq!(request.pad, expected);
            assert!(app.apply_worker_result(staged_project_result(request, fingerprint)));
            assert!(calls.snapshot().is_empty());
        }
        let stage = app.project_open_stage().unwrap();
        assert_eq!(stage.staged_pads, 2);
        assert_eq!(stage.phase, crate::ProjectOpenPhase::Staging);
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn project_open_recovery_prompts_only_for_same_id_higher_revision_and_cancel_preserves() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let project_id = sampler_core::ProjectId::from_bytes([0x76; 16]);
        let explicit = project_open_document(project_id, "Explicit", 4, Vec::new());
        let recovery = project_open_document(project_id, "Recovery", 6, Vec::new());
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();

        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(explicit)),
                recovery: Some(Ok(recovery)),
            }),
        }));
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::AwaitingRecoveryChoice
        );
        assert_eq!(app.project_snapshot().unwrap(), before);
        app.choose_project_recovery(crate::RecoveryChoice::Cancel)
            .unwrap();
        assert!(app.project_open_stage().is_none());
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_recovery_lower_is_ignored_and_other_identity_is_rejected() {
        let project_id = sampler_core::ProjectId::from_bytes([0x77; 16]);
        let mut lower = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let token = lower.request_open_project("project-b").unwrap();
        lower.take_worker_requests();
        assert!(lower.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    7,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));
        assert_eq!(lower.project_open_stage().unwrap().revision, Some(7));
        assert_eq!(
            lower.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::Staging
        );

        let mut mismatch = project_app();
        let before = mismatch.project_snapshot().unwrap();
        let token = mismatch.request_open_project("project-c").unwrap();
        mismatch.take_worker_requests();
        assert!(mismatch.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-c".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-c".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    7,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    sampler_core::ProjectId::from_bytes([0x78; 16]),
                    "Other",
                    8,
                    Vec::new(),
                ))),
            }),
        }));
        assert!(mismatch.project_open_stage().is_none());
        assert_eq!(
            mismatch.project_open_error(),
            Some(&crate::ProjectOpenError::RecoveryMismatch)
        );
        assert!(mismatch.status().contains("recovery identity"));
        assert_eq!(mismatch.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_discard_waits_for_exact_recovery_deletion_before_staging_explicit() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let project_id = sampler_core::ProjectId::from_bytes([0x79; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    4,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));

        app.choose_project_recovery(crate::RecoveryChoice::Discard)
            .unwrap();
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::DiscardRecovery {
                token: discard_token,
                directory,
                project_id: discarded_id,
                revision,
            },
        ] = requests.as_slice()
        else {
            panic!("expected exact recovery discard");
        };
        assert_eq!(*discard_token, token);
        assert_eq!(directory, path("project-b"));
        assert_eq!(*discarded_id, project_id);
        assert_eq!(*revision, 6);
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::AwaitingRecoveryChoice
        );
        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token,
            directory: "project-b".into(),
            project_id,
            revision: 6,
            result: Ok(()),
        }));
        assert_eq!(app.project_open_stage().unwrap().revision, Some(4));
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::Staging
        );
    }

    #[test]
    fn project_open_restore_and_discard_commit_the_chosen_exact_midi_document() {
        let project_id = sampler_core::ProjectId::from_bytes([0xac; 16]);
        let explicit_midi = MidiSettings::default()
            .with_channel(MidiChannelFilter::Channel(midi_channel(3)))
            .learn_swap(BankId::new(0).unwrap(), 1, midi_note(70))
            .unwrap();
        let recovery_midi = MidiSettings::default()
            .with_channel(MidiChannelFilter::Channel(midi_channel(14)))
            .learn_swap(BankId::new(9).unwrap(), 15, midi_note(7))
            .unwrap()
            .unmap(BankId::new(2).unwrap(), 5)
            .unwrap();

        for (choice, expected) in [
            (crate::RecoveryChoice::Restore, recovery_midi),
            (crate::RecoveryChoice::Discard, explicit_midi),
        ] {
            let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
            let token = app.request_open_project("choice").unwrap();
            app.take_worker_requests();
            assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
                token,
                directory: "choice".into(),
                result: Ok(crate::ProjectProbe {
                    directory: "choice".into(),
                    explicit: Some(Ok(project_open_document_with_midi(
                        project_id,
                        "Explicit",
                        4,
                        Vec::new(),
                        explicit_midi,
                    ))),
                    recovery: Some(Ok(project_open_document_with_midi(
                        project_id,
                        "Recovery",
                        6,
                        Vec::new(),
                        recovery_midi,
                    ))),
                }),
            }));
            app.choose_project_recovery(choice).unwrap();
            if choice == crate::RecoveryChoice::Discard {
                app.take_worker_requests();
                assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
                    token,
                    directory: "choice".into(),
                    project_id,
                    revision: 6,
                    result: Ok(()),
                }));
            }
            while app.project_open_stage().is_some() {
                assert!(app.maintain_project(Instant::now()));
            }
            assert_eq!(app.midi_settings, expected);
        }
    }

    #[test]
    fn project_open_discard_cannot_be_cancelled_after_exact_deletion_is_queued() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let project_id = sampler_core::ProjectId::from_bytes([0x8e; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    4,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));
        app.choose_project_recovery(crate::RecoveryChoice::Discard)
            .unwrap();
        let requests = app.take_worker_requests();

        assert_eq!(
            app.choose_project_recovery(crate::RecoveryChoice::Cancel),
            Err(crate::ProjectOpenError::CancellationLocked)
        );
        assert_eq!(
            app.cancel_project_open(),
            Err(crate::ProjectOpenError::CancellationLocked)
        );
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::AwaitingRecoveryChoice
        );
        assert_eq!(app.project_snapshot().unwrap(), before);
        assert_eq!(requests.len(), 1);
        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token,
            directory: "project-b".into(),
            project_id,
            revision: 6,
            result: Ok(()),
        }));
        assert_eq!(app.project_open_stage().unwrap().revision, Some(4));
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_discard_can_be_cancelled_before_exact_deletion_is_dispatched() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let project_id = sampler_core::ProjectId::from_bytes([0x90; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    4,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));
        app.pending_worker_requests = vec![WorkerRequest::Shutdown; super::WORKER_CHANNEL_CAPACITY];
        app.choose_project_recovery(crate::RecoveryChoice::Discard)
            .unwrap();

        app.choose_project_recovery(crate::RecoveryChoice::Cancel)
            .unwrap();
        assert!(app.project_open_stage().is_none());
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_discard_failure_is_retained_as_the_exact_typed_error() {
        let mut app = project_app();
        let before = app.project_snapshot().unwrap();
        let project_id = sampler_core::ProjectId::from_bytes([0x8f; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    4,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));
        app.choose_project_recovery(crate::RecoveryChoice::Discard)
            .unwrap();
        app.take_worker_requests();
        let error = ProjectStoreError::Filesystem {
            operation: "discard recovery",
            path: "project-b/.sampler-tui-recovery.toml".into(),
            kind: std::io::ErrorKind::PermissionDenied,
        };

        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token,
            directory: "project-b".into(),
            project_id,
            revision: 6,
            result: Err(error.clone()),
        }));
        assert_eq!(
            app.project_open_error(),
            Some(&crate::ProjectOpenError::RecoveryDiscard(error))
        );
        assert_eq!(app.project_snapshot().unwrap(), before);
    }

    #[test]
    fn project_open_admits_stop_pads_and_patterns_one_per_maintenance_then_commits() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let project_id = sampler_core::ProjectId::from_bytes([0x7a; 16]);
        let settings = PadSettings::new(PlaybackMode::Gate, -4.0, 0.25, 3.0, None).unwrap();
        let document = project_open_document(
            project_id,
            "Project B",
            12,
            vec![project_open_pad(pad(0, 1), settings)],
        );
        let old_snapshot = app.project_snapshot().unwrap();
        stage_project_open(&mut app, "project-b", document);

        app.maintain_audio();
        assert!(calls.snapshot().is_empty());
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(calls.snapshot(), [AudioCall::StopAll]);
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::Admitting
        );
        app.apply(InputAction::PadPress(0));
        assert_eq!(calls.snapshot(), [AudioCall::StopAll]);
        assert_eq!(app.project_snapshot().unwrap(), old_snapshot);

        assert!(app.maintain_project(Instant::now()));
        assert_eq!(calls.snapshot(), [AudioCall::StopAll]);
        assert_eq!(app.project_open_stage().unwrap().admitted_actions, 3);
        assert_eq!(app.project_snapshot().unwrap(), old_snapshot);

        for offset in 0..super::PAD_VIEW_COUNT {
            let before = calls.snapshot().len();
            assert!(app.maintain_project(Instant::now()));
            let after = calls.snapshot();
            assert_eq!(after.len(), before + 1);
            let expected_pad = super::pad_from_offset(offset);
            if offset == 1 {
                assert_eq!(after.last(), Some(&AudioCall::Install(expected_pad)));
            } else {
                assert_eq!(after.last(), Some(&AudioCall::RemoveSample(expected_pad)));
            }
            assert!(app.project_open_stage().is_some());
            assert_eq!(app.project_snapshot().unwrap(), old_snapshot);
        }

        for index in 0..sampler_core::PATTERN_SLOT_COUNT {
            let before = calls.snapshot().len();
            assert!(app.maintain_project(Instant::now()));
            let after = calls.snapshot();
            assert_eq!(after.len(), before + 1);
            assert_eq!(after.last(), Some(&AudioCall::InstallPattern));
            if index + 1 < sampler_core::PATTERN_SLOT_COUNT {
                assert!(app.project_open_stage().is_some());
                assert_eq!(app.project_snapshot().unwrap(), old_snapshot);
            }
        }

        assert!(app.project_open_stage().is_none());
        assert!(app.overlay().is_none());
        assert_eq!(app.project_revision(), 12);
        assert_eq!(app.project_header(), "Project B · SAVED");
        assert_eq!(app.pad(pad(0, 1)).settings, settings);
        assert_eq!(app.pad(pad(0, 1)).state, PadLoadState::Ready);
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Empty);
    }

    #[test]
    fn project_open_midi_release_failure_preserves_exact_old_engine_and_app_for_cancel() {
        let audio =
            FakeAudio::ready(48_000, 2).failing_owned_release_at(2, "second release rejected");
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        app.patterns
            .start_recording(TransportStamp {
                slot: PatternSlotId::new(0).unwrap(),
                generation: 0,
                origin: 0,
                loop_frames: 96_000,
            })
            .unwrap();
        app.apply_midi_event(midi_on(2, 36, 127));
        app.apply_midi_event(midi_on(2, 37, 127));
        let old_snapshot = app.project_snapshot().unwrap();
        let old_owners = app.midi_owned_pads.clone();
        let old_pattern = app.patterns.selected_pattern().events().to_vec();
        let candidate_midi = MidiSettings::default()
            .with_channel(MidiChannelFilter::Channel(midi_channel(5)))
            .learn_swap(BankId::new(6).unwrap(), 1, midi_note(99))
            .unwrap();
        let document = project_open_document_with_midi(
            sampler_core::ProjectId::from_bytes([0xa7; 16]),
            "Candidate",
            23,
            Vec::new(),
            candidate_midi,
        );
        stage_project_open(&mut app, "candidate", document);
        app.arm_midi_learn();
        let old_learn = app.midi_learn_target();
        calls.clear();

        assert!(!app.maintain_project(Instant::now()));

        assert!(app.project_open_stage().is_some());
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::Staging
        );
        assert_eq!(calls.snapshot(), Vec::<AudioCall>::new());
        assert_eq!(app.project_snapshot().unwrap(), old_snapshot);
        assert_eq!(app.midi_owned_pads, old_owners);
        assert_eq!(app.patterns.selected_pattern().events(), old_pattern);
        assert_eq!(app.midi_learn_target(), old_learn);
        assert!(app.status().contains("second release rejected"));
        app.cancel_project_open().unwrap();
        assert!(app.project_open_stage().is_none());
        assert_eq!(app.project_snapshot().unwrap(), old_snapshot);
    }

    #[test]
    fn project_open_midi_release_failure_retries_before_any_candidate_audio() {
        let audio = FakeAudio::ready(48_000, 2).failing_release_once("release rejected");
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        app.apply_midi_event(midi_on(2, 36, 127));
        let candidate_midi =
            MidiSettings::default().with_channel(MidiChannelFilter::Channel(midi_channel(5)));
        let document = project_open_document_with_midi(
            sampler_core::ProjectId::from_bytes([0xa8; 16]),
            "Candidate",
            24,
            Vec::new(),
            candidate_midi,
        );
        stage_project_open(&mut app, "candidate-retry", document);
        calls.clear();

        assert!(!app.maintain_project(Instant::now()));
        assert_eq!(calls.snapshot(), Vec::<AudioCall>::new());
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
                AudioCall::StopAll,
            ]
        );

        while app.project_open_stage().is_some() {
            assert!(app.maintain_project(Instant::now()));
        }
        assert!(app.project_open_stage().is_none());
        assert_eq!(app.midi_settings, candidate_midi);
        assert_eq!(app.project_revision(), 24);
        assert!(app.midi_owned_pads.iter().all(Option::is_none));
    }

    #[test]
    fn project_open_replacement_releases_the_exact_managed_id_through_all_backpressure_and_result_paths()
     {
        let mut app = project_app();
        let managed_id = ManagedCaptureId::new(701);
        app.sample_editor.commits[0].managed_capture = Some(managed_id);
        let replacement = project_open_document(
            sampler_core::ProjectId::from_bytes([0x7d; 16]),
            "Replacement",
            8,
            Vec::new(),
        );
        stage_project_open(&mut app, "replacement-project", replacement);
        while app.project_open_stage().is_some() {
            assert!(app.maintain_project(Instant::now()));
        }

        for index in 0..WORKER_CHANNEL_CAPACITY {
            app.pending_worker_requests
                .push(WorkerRequest::ReleaseManagedCapture {
                    id: ManagedCaptureId::new(800 + index as u64),
                });
        }
        assert!(!app.maintain_capture());
        assert_eq!(app.managed_release_in_flight(), None);
        app.pending_worker_requests.clear();

        assert!(app.maintain_capture());
        let [release] = app.take_worker_requests().try_into().unwrap();
        let WorkerRequest::ReleaseManagedCapture { id } = release else {
            panic!("expected managed release request")
        };
        assert_eq!(id, managed_id);
        let release = WorkerRequest::ReleaseManagedCapture { id };
        assert!(app.apply_worker_send_error(release, WorkerSendError::WorkerBusy));
        assert_eq!(app.managed_release_in_flight(), None);

        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture { id: managed_id }]
        );
        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: ManagedCaptureId::new(999),
                result: Ok(()),
            })
        );
        assert!(!app.maintain_capture());
        assert_eq!(app.managed_release_in_flight(), Some(managed_id));

        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: managed_id,
                result: Err(CaptureStoreError::NotLive { id: managed_id }),
            })
        );
        assert!(app.maintain_capture());
        assert_eq!(app.managed_release_in_flight(), None);
        assert!(app.maintain_capture());
        assert_eq!(
            app.take_worker_requests(),
            [WorkerRequest::ReleaseManagedCapture { id: managed_id }]
        );

        assert!(
            app.apply_worker_result(WorkerResult::ManagedCaptureReleased {
                id: managed_id,
                result: Ok(()),
            })
        );
        assert!(app.maintain_capture());
        assert_eq!(app.managed_release_in_flight(), None);
        assert!(app.pending_managed_releases.is_empty());
    }

    #[test]
    fn project_open_install_failure_restores_then_restarts_admission_from_stop_all() {
        let audio = FakeAudio::ready(48_000, 2).failing_install("install rejected");
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        app.patterns
            .start_recording(TransportStamp {
                slot: PatternSlotId::new(0).unwrap(),
                generation: 0,
                origin: 0,
                loop_frames: 96_000,
            })
            .unwrap();
        app.apply_midi_event(midi_on(2, 36, 127));
        app.apply_midi_event(midi_on(2, 37, 127));
        let old_snapshot = app.project_snapshot().unwrap();
        let old_owners = app.midi_owned_pads.clone();
        let old_pattern = app.patterns.selected_pattern().events().to_vec();
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x7b; 16]),
            "Project B",
            13,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        stage_project_open(&mut app, "project-b", document);
        calls.clear();
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
                AudioCall::TrackedOwnedRelease(pad(0, 1), midi_command(2)),
                AudioCall::StopAll,
            ]
        );

        assert!(app.maintain_project(Instant::now()));
        assert_eq!(app.project_open_stage().unwrap().admitted_actions, 3);

        assert!(!app.maintain_project(Instant::now()));
        assert_eq!(app.project_open_stage().unwrap().admitted_actions, 1);
        assert!(app.status().contains("committed audio restored"));
        assert_eq!(app.project_snapshot().unwrap(), old_snapshot);
        assert_ne!(app.midi_owned_pads, old_owners);
        assert!(app.midi_owned_pads.iter().all(Option::is_none));
        assert_eq!(app.patterns.selected_pattern().events(), old_pattern);

        assert!(app.maintain_project(Instant::now()));
        assert_eq!(calls.snapshot().last(), Some(&AudioCall::StopAll));
        assert_eq!(app.project_open_stage().unwrap().admitted_actions, 2);
    }

    #[test]
    fn project_open_device_retry_restarts_admission_on_the_empty_engine() {
        let audio = FakeAudio::ready(48_000, 2).failing_runtime("device lost");
        let old_calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        let old_snapshot = app.project_snapshot().unwrap();
        let document = project_open_document(
            sampler_core::ProjectId::from_bytes([0x7c; 16]),
            "Project B",
            14,
            vec![project_open_pad(pad(0, 0), PadSettings::default())],
        );
        stage_project_open(&mut app, "project-b", document);
        assert!(app.maintain_project(Instant::now()));
        assert!(app.maintain_project(Instant::now()));
        assert!(app.maintain_project(Instant::now()));
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(
            old_calls.snapshot(),
            [
                AudioCall::StopAll,
                AudioCall::Install(pad(0, 0)),
                AudioCall::RemoveSample(pad(0, 1)),
            ]
        );
        assert!(app.maintain_audio());
        assert_eq!(app.audio_format(), None);
        assert_eq!(app.project_snapshot().unwrap(), old_snapshot);

        let replacement = FakeAudio::ready(48_000, 2);
        let replacement_calls = replacement.call_log();
        assert!(app.retry_with(Box::new(replacement)));
        replacement_calls.clear();
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(replacement_calls.snapshot(), [AudioCall::StopAll]);
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(replacement_calls.snapshot(), [AudioCall::StopAll]);
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(
            replacement_calls.snapshot(),
            [AudioCall::StopAll, AudioCall::Install(pad(0, 0))]
        );
        assert_eq!(app.project_snapshot().unwrap(), old_snapshot);
    }

    #[test]
    fn project_open_restore_commits_recovery_as_modified_against_explicit_revision() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let project_id = sampler_core::ProjectId::from_bytes([0x7d; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_id,
                    "Explicit",
                    4,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    6,
                    Vec::new(),
                ))),
            }),
        }));
        app.choose_project_recovery(crate::RecoveryChoice::Restore)
            .unwrap();
        while app.project_open_stage().is_some() {
            assert!(app.maintain_project(Instant::now()));
        }

        assert_eq!(app.project_revision(), 6);
        assert_eq!(app.project_session.saved_revision(), 4);
        assert_eq!(app.project_session.autosaved_revision(), 6);
        assert_eq!(app.project_header(), "Recovery · MODIFIED");
    }

    #[test]
    fn project_open_can_restore_valid_recovery_when_explicit_document_is_corrupt() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let project_id = sampler_core::ProjectId::from_bytes([0x7e; 16]);
        let token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Err(ProjectStoreError::DocumentInvalid {
                    path: "project-b/project.toml".into(),
                    message: "corrupt TOML".to_owned(),
                })),
                recovery: Some(Ok(project_open_document(
                    project_id,
                    "Recovery",
                    3,
                    Vec::new(),
                ))),
            }),
        }));
        assert_eq!(
            app.project_open_stage().unwrap().phase,
            crate::ProjectOpenPhase::AwaitingRecoveryChoice
        );
        app.choose_project_recovery(crate::RecoveryChoice::Restore)
            .unwrap();
        assert_eq!(app.project_open_stage().unwrap().revision, Some(3));
    }

    #[test]
    fn project_save_refuses_untitled_dirty_draft_and_pending_operations() {
        let now = Instant::now();
        let mut untitled = project_app();
        assert_eq!(
            untitled.request_save(),
            Err(super::ProjectSaveError::Untitled)
        );

        let mut dirty = project_app();
        name_project(&mut dirty, "named", now);
        dirty.editor_mut_for_test().move_marker(1, false);
        assert!(matches!(
            dirty.request_save(),
            Err(super::ProjectSaveError::Snapshot(
                ProjectSnapshotError::DirtySampleDraft(_)
            ))
        ));

        let mut pending = project_app();
        name_project(&mut pending, "named", now);
        let _ = pending.begin_load(pad(0, 1), "pending.wav");
        assert!(matches!(
            pending.request_save(),
            Err(super::ProjectSaveError::Snapshot(
                ProjectSnapshotError::PendingSampleLoad(_)
            ))
        ));
    }

    #[test]
    fn save_as_reuses_generated_identity_after_an_error() {
        let now = Instant::now();
        let mut app = project_app();
        app.request_save_as("new-project").unwrap();
        assert!(app.maintain_project(now));
        let first = take_project_save(&mut app);
        assert_ne!(first.request.snapshot.project_id.as_bytes(), &[0; 16]);
        assert!(first.request.save_as);
        assert!(app.apply_worker_result(save_error(&first, "save-as")));

        app.request_save_as("new-project").unwrap();
        assert!(app.maintain_project(now + Duration::from_secs(1)));
        let retry = take_project_save(&mut app);
        assert_eq!(
            retry.request.snapshot.project_id,
            first.request.snapshot.project_id
        );
        assert_eq!(retry.request.directory, first.request.directory);
        assert!(retry.request.save_as);
    }

    #[test]
    fn save_as_move_and_fresh_open_preserve_the_exact_midi_settings() {
        let now = Instant::now();
        let midi = MidiSettings::default()
            .with_channel(MidiChannelFilter::Channel(midi_channel(10)))
            .learn_swap(BankId::new(1).unwrap(), 6, midi_note(82))
            .unwrap()
            .learn_swap(BankId::new(8).unwrap(), 11, midi_note(23))
            .unwrap()
            .unmap(BankId::new(5).unwrap(), 4)
            .unwrap();
        let mut source = project_app();
        source.update_midi_settings(midi).unwrap();

        source.request_save_as("first-location").unwrap();
        assert!(source.maintain_project(now));
        let first = take_project_save(&mut source);
        assert_eq!(first.request.snapshot.midi, midi);
        assert!(source.apply_worker_result(save_result(&first, Vec::new())));

        source.request_save_as("moved-location").unwrap();
        assert!(source.maintain_project(now + Duration::from_secs(1)));
        let moved = take_project_save(&mut source);
        assert!(moved.request.save_as);
        assert_eq!(moved.request.snapshot.midi, midi);
        assert!(source.apply_worker_result(save_result(&moved, Vec::new())));

        let mut fresh = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        let document = project_open_document_with_midi(
            moved.request.snapshot.project_id,
            &moved.request.snapshot.name,
            moved.request.snapshot.revision,
            Vec::new(),
            moved.request.snapshot.midi,
        );
        stage_project_open(&mut fresh, "moved-location", document);
        while fresh.project_open_stage().is_some() {
            assert!(fresh.maintain_project(now + Duration::from_secs(2)));
        }
        assert_eq!(fresh.midi_settings, midi);
    }

    #[test]
    fn project_save_accepts_only_exact_worker_and_receipt_identity() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        app.request_save().unwrap();
        app.maintain_project(now);
        let request = take_project_save(&mut app);
        let exact = save_result(&request, Vec::new());

        let WorkerResult::ProjectSaved {
            token,
            kind,
            project_id,
            directory,
            revision,
            result,
        } = exact
        else {
            unreachable!()
        };
        for stale in [
            WorkerResult::ProjectSaved {
                token: crate::ProjectToken::new(token.get() + 1),
                kind,
                project_id,
                directory: directory.clone(),
                revision,
                result: result.clone(),
            },
            WorkerResult::ProjectSaved {
                token,
                kind,
                project_id,
                directory: "other".into(),
                revision,
                result: result.clone(),
            },
            WorkerResult::ProjectSaved {
                token,
                kind,
                project_id,
                directory: directory.clone(),
                revision: revision + 1,
                result: result.clone(),
            },
        ] {
            assert!(!app.apply_worker_result(stale));
        }
        let mut wrong_receipt = result.unwrap();
        wrong_receipt.project_id = sampler_core::ProjectId::from_bytes([0x99; 16]);
        assert!(!app.apply_worker_result(WorkerResult::ProjectSaved {
            token,
            kind,
            project_id,
            directory: directory.clone(),
            revision,
            result: Ok(wrong_receipt),
        }));
        assert!(app.apply_worker_result(save_result(&request, Vec::new())));
        assert_eq!(app.project_session.saved_revision(), revision);
    }

    #[test]
    fn save_mapping_requires_exact_generation_and_fingerprint_before_path_adoption() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        let snapshot = app.project_snapshot().unwrap();
        let saved_pad = snapshot.pads[0].clone();
        let original = app.pad(saved_pad.pad).source.clone();
        app.request_save().unwrap();
        app.maintain_project(now);
        let request = take_project_save(&mut app);
        let stale_fingerprint = crate::SourceFingerprint::from_encoded_bytes(
            std::path::Path::new("other.wav"),
            b"other",
        )
        .unwrap();
        assert!(app.apply_worker_result(save_result(
            &request,
            vec![
                ProjectAssetMapping {
                    pad: saved_pad.pad,
                    source_generation: saved_pad.source_generation + 1,
                    fingerprint: saved_pad.fingerprint,
                    project_path: "named/audio/wrong-generation.wav".into(),
                },
                ProjectAssetMapping {
                    pad: saved_pad.pad,
                    source_generation: saved_pad.source_generation,
                    fingerprint: stale_fingerprint,
                    project_path: "named/audio/wrong-digest.wav".into(),
                },
            ],
        )));
        assert_eq!(app.pad(saved_pad.pad).source, original);
    }

    #[test]
    fn explicit_and_recovery_save_adopt_current_internal_paths_but_only_explicit_is_clean() {
        let now = Instant::now();
        let mut recovery = project_app();
        let project_id = name_project(&mut recovery, "named", now);
        recovery.maintain_project(now + Duration::from_secs(2));
        let autosave = take_project_save(&mut recovery);
        assert_eq!(autosave.request.kind, SaveKind::Recovery);
        let pad = autosave.request.snapshot.pads[0].clone();
        let internal = PathBuf::from("named/audio/internal.wav");
        assert!(recovery.apply_worker_result(save_result(
            &autosave,
            vec![ProjectAssetMapping {
                pad: pad.pad,
                source_generation: pad.source_generation,
                fingerprint: pad.fingerprint,
                project_path: internal.clone(),
            }],
        )));
        assert_eq!(
            recovery.pad(pad.pad).source.as_deref(),
            Some(internal.as_path())
        );
        assert_eq!(
            recovery.project_session.autosaved_revision(),
            recovery.project_revision()
        );
        assert_ne!(
            recovery.project_session.saved_revision(),
            recovery.project_revision()
        );
        assert_eq!(recovery.project_session.project_id(), project_id);

        recovery.request_save().unwrap();
        recovery.maintain_project(now + Duration::from_secs(3));
        let explicit = take_project_save(&mut recovery);
        assert!(recovery.apply_worker_result(save_result(&explicit, Vec::new())));
        assert_eq!(
            recovery.project_session.saved_revision(),
            recovery.project_revision()
        );
    }

    #[test]
    fn autosave_debounces_two_seconds_coalesces_when_busy_and_explicit_has_priority() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        assert!(!app.maintain_project(now + Duration::from_millis(1_999)));
        assert!(app.take_worker_requests().is_empty());

        for request_id in 0..crate::loader::WORKER_CHANNEL_CAPACITY as u64 {
            app.pending_worker_requests
                .push(WorkerRequest::ScanDirectory {
                    request_id,
                    path: PathBuf::from("."),
                    show_hidden: false,
                });
        }
        assert!(app.maintain_project(now + Duration::from_secs(2)));
        assert!(app.project_session.pending_autosave().is_some());
        app.pending_worker_requests.clear();
        app.project_session
            .commit_project_mutation(now + Duration::from_secs(3), || Ok::<(), ()>(()))
            .unwrap();
        app.request_save().unwrap();
        app.maintain_project(now + Duration::from_secs(5));
        let request = take_project_save(&mut app);
        assert_eq!(request.request.kind, SaveKind::Explicit);
        assert_eq!(request.request.snapshot.revision, app.project_revision());
    }

    #[test]
    fn midi_mapping_revision_autosaves_exactly_but_runtime_port_lifecycle_does_not() {
        let now = Instant::now();
        let mut mapping = project_app();
        name_project(&mut mapping, "mapped", now);
        mapping.request_save().unwrap();
        assert!(mapping.maintain_project(now));
        let initial_save = take_project_save(&mut mapping);
        assert!(mapping.apply_worker_result(save_result(&initial_save, Vec::new())));
        assert!(mapping.maintain_project(now));
        let cleanup = take_recovery_cleanup(&mut mapping);
        assert!(
            mapping.apply_worker_result(WorkerResult::RecoveryDiscarded {
                token: cleanup.token,
                directory: cleanup.directory,
                project_id: cleanup.project_id,
                revision: cleanup.revision,
                result: Ok(()),
            })
        );
        let midi = MidiSettings::default()
            .with_channel(MidiChannelFilter::Channel(midi_channel(6)))
            .learn_swap(BankId::new(0).unwrap(), 2, midi_note(88))
            .unwrap()
            .learn_swap(BankId::new(7).unwrap(), 13, midi_note(19))
            .unwrap()
            .unmap(BankId::new(3).unwrap(), 9)
            .unwrap();
        mapping.update_midi_settings(midi).unwrap();
        assert!(mapping.maintain_project(now + Duration::from_secs(3)));
        let recovery = take_project_save(&mut mapping);
        assert_eq!(recovery.request.kind, SaveKind::Recovery);
        assert_eq!(recovery.request.snapshot.midi, midi);
        assert_eq!(
            recovery.request.snapshot.revision,
            mapping.project_revision()
        );

        let (service, _) = fake_midi_service([("a", "Keys")]);
        let mut runtime = App::with_audio_and_midi(Box::new(FakeAudio::ready(48_000, 2)), service);
        name_project(&mut runtime, "runtime", now);
        runtime.request_save().unwrap();
        assert!(runtime.maintain_project(now));
        let clean_save = take_project_save(&mut runtime);
        assert!(runtime.apply_worker_result(save_result(&clean_save, Vec::new())));
        assert!(runtime.maintain_project(now));
        let cleanup = take_recovery_cleanup(&mut runtime);
        assert!(
            runtime.apply_worker_result(WorkerResult::RecoveryDiscarded {
                token: cleanup.token,
                directory: cleanup.directory,
                project_id: cleanup.project_id,
                revision: cleanup.revision,
                result: Ok(()),
            })
        );
        let clean_revision = runtime.project_revision();
        runtime.list_midi_ports().unwrap();
        runtime.connect_midi_port(0).unwrap();
        runtime.disconnect_midi_port().unwrap();
        assert_eq!(runtime.project_revision(), clean_revision);
        assert!(!runtime.maintain_project(now + Duration::from_secs(3)));
        assert!(runtime.take_worker_requests().is_empty());
    }

    #[test]
    fn autosave_replaces_a_busy_pending_snapshot_with_the_newest_quiet_revision() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        for request_id in 0..crate::loader::WORKER_CHANNEL_CAPACITY as u64 {
            app.pending_worker_requests
                .push(WorkerRequest::ScanDirectory {
                    request_id,
                    path: PathBuf::from("."),
                    show_hidden: false,
                });
        }
        app.maintain_project(now + Duration::from_secs(2));
        let first_revision = app.project_session.pending_autosave().unwrap().revision;

        app.project_session
            .commit_project_mutation(now + Duration::from_secs(3), || Ok::<(), ()>(()))
            .unwrap();
        app.maintain_project(now + Duration::from_secs(5));
        assert!(app.project_revision() > first_revision);
        assert_eq!(
            app.project_session.pending_autosave().unwrap().revision,
            app.project_revision()
        );

        app.pending_worker_requests.clear();
        app.maintain_project(now + Duration::from_secs(5));
        assert_eq!(
            take_project_save(&mut app).request.snapshot.revision,
            app.project_revision()
        );
    }

    #[test]
    fn autosave_withholds_a_stale_pending_snapshot_until_the_newest_revision_is_quiet() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        for request_id in 0..crate::loader::WORKER_CHANNEL_CAPACITY as u64 {
            app.pending_worker_requests
                .push(WorkerRequest::ScanDirectory {
                    request_id,
                    path: PathBuf::from("."),
                    show_hidden: false,
                });
        }
        app.maintain_project(now + Duration::from_secs(2));
        let stale_revision = app.project_session.pending_autosave().unwrap().revision;

        app.project_session
            .commit_project_mutation(now + Duration::from_secs(3), || Ok::<(), ()>(()))
            .unwrap();
        app.pending_worker_requests.clear();
        assert!(app.maintain_project(now + Duration::from_secs(4)));
        assert!(app.take_worker_requests().is_empty());
        assert!(app.project_session.pending_autosave().is_none());
        assert!(app.project_revision() > stale_revision);

        assert!(app.maintain_project(now + Duration::from_secs(5)));
        assert_eq!(
            take_project_save(&mut app).request.snapshot.revision,
            app.project_revision()
        );
    }

    #[test]
    fn explicit_save_cancels_covered_autosave_and_does_not_recreate_recovery_while_clean() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        for request_id in 0..crate::loader::WORKER_CHANNEL_CAPACITY as u64 {
            app.pending_worker_requests
                .push(WorkerRequest::ScanDirectory {
                    request_id,
                    path: PathBuf::from("."),
                    show_hidden: false,
                });
        }
        app.maintain_project(now + Duration::from_secs(2));
        assert!(app.project_session.pending_autosave().is_some());
        app.pending_worker_requests.clear();

        app.request_save().unwrap();
        app.maintain_project(now + Duration::from_secs(2));
        let explicit = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_result(&explicit, Vec::new())));
        assert_eq!(app.project_session.pending_autosave(), None);

        app.pending_recovery_cleanup.clear();
        assert!(!app.maintain_project(now + Duration::from_secs(20)));
        assert!(app.take_worker_requests().is_empty());
    }

    #[test]
    fn autosave_error_retries_after_another_quiet_interval_and_untitled_never_autosaves() {
        let now = Instant::now();
        let mut untitled = project_app();
        assert!(!untitled.maintain_project(now + Duration::from_secs(20)));
        assert!(untitled.take_worker_requests().is_empty());

        let mut app = project_app();
        name_project(&mut app, "named", now);
        app.maintain_project(now + Duration::from_secs(2));
        let failed = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_error(&failed, "autosave")));
        assert!(app.project_save_error().is_some());
        app.apply(InputAction::PadPress(99));
        assert!(app.status().contains("outside 0..16"));
        assert!(app.project_header().contains("AUTOSAVE ERROR"));
        assert!(app.maintain_project(now + Duration::from_secs(2)));
        assert!(app.take_worker_requests().is_empty());
        assert!(!app.maintain_project(now + Duration::from_millis(3_999)));
        assert!(app.maintain_project(now + Duration::from_secs(4)));
        let retry = take_project_save(&mut app);
        assert_eq!(retry.request.kind, SaveKind::Recovery);
        assert_eq!(
            retry.request.snapshot.revision,
            failed.request.snapshot.revision
        );
    }

    #[test]
    fn autosave_error_retry_waits_for_two_seconds_after_a_newer_mutation() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        app.maintain_project(now + Duration::from_secs(2));
        let failed = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_error(&failed, "autosave")));
        app.maintain_project(now + Duration::from_secs(2));

        app.project_session
            .commit_project_mutation(now + Duration::from_secs(3), || Ok::<(), ()>(()))
            .unwrap();
        assert!(!app.maintain_project(now + Duration::from_secs(4)));
        assert!(app.take_worker_requests().is_empty());
        assert!(app.maintain_project(now + Duration::from_secs(5)));
        assert_eq!(
            take_project_save(&mut app).request.snapshot.revision,
            app.project_revision()
        );
    }

    #[test]
    fn explicit_save_cleanup_failure_is_a_warning_after_clean_truth() {
        let now = Instant::now();
        let mut app = project_app();
        let project_id = name_project(&mut app, "named", now);
        app.request_save().unwrap();
        app.maintain_project(now);
        let explicit = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_result(&explicit, Vec::new())));
        assert_eq!(app.project_session.saved_revision(), app.project_revision());

        app.maintain_project(now + Duration::from_secs(1));
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::DiscardRecovery {
                token,
                directory,
                project_id: cleanup_project_id,
                revision,
            },
        ] = requests.as_slice()
        else {
            panic!("expected recovery cleanup request");
        };
        assert_eq!(*cleanup_project_id, project_id);
        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token: *token,
            directory: directory.clone(),
            project_id,
            revision: *revision,
            result: Err(ProjectStoreError::Filesystem {
                operation: "delete recovery",
                path: directory.clone(),
                kind: std::io::ErrorKind::PermissionDenied,
            }),
        }));
        assert!(app.recovery_cleanup_warning().is_some());
        assert_eq!(app.project_session.saved_revision(), app.project_revision());
    }

    #[test]
    fn recovery_cleanup_interleaving_preserves_fifo_order_and_busy_restores_the_exact_front() {
        let now = Instant::now();
        let mut app = project_app();
        let project_a = name_project(&mut app, "project-a", now);
        app.request_save().unwrap();
        app.maintain_project(now);
        let save_a = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_result(&save_a, Vec::new())));

        app.request_save_as("project-b").unwrap();
        app.maintain_project(now + Duration::from_secs(1));
        let save_b = take_project_save(&mut app);
        let project_b = save_b.request.snapshot.project_id;
        assert!(app.apply_worker_result(save_result(&save_b, Vec::new())));

        app.maintain_project(now + Duration::from_secs(2));
        let cleanup_a = take_recovery_cleanup(&mut app);
        assert_eq!(cleanup_a.directory, PathBuf::from("project-a"));
        assert_eq!(cleanup_a.project_id, project_a);
        assert!(app.apply_worker_send_error(
            WorkerRequest::DiscardRecovery {
                token: cleanup_a.token,
                directory: cleanup_a.directory.clone(),
                project_id: cleanup_a.project_id,
                revision: cleanup_a.revision,
            },
            WorkerSendError::WorkerBusy,
        ));

        app.maintain_project(now + Duration::from_secs(3));
        assert_eq!(take_recovery_cleanup(&mut app), cleanup_a);
        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token: cleanup_a.token,
            directory: cleanup_a.directory.clone(),
            project_id: cleanup_a.project_id,
            revision: cleanup_a.revision,
            result: Ok(()),
        }));

        app.maintain_project(now + Duration::from_secs(4));
        let cleanup_b = take_recovery_cleanup(&mut app);
        assert_eq!(cleanup_b.directory, PathBuf::from("project-b"));
        assert_eq!(cleanup_b.project_id, project_b);
    }

    #[test]
    fn delayed_cleanup_from_old_project_does_not_clear_opened_project_recovery_truth() {
        let now = Instant::now();
        let mut app = project_app();
        let project_a = name_project(&mut app, "project-a", now);
        app.request_save().unwrap();
        app.maintain_project(now);
        let save_a = take_project_save(&mut app);
        assert!(app.apply_worker_result(save_result(&save_a, Vec::new())));
        let cleanup_revision = app
            .pending_recovery_cleanup
            .front()
            .expect("explicit save queues recovery cleanup")
            .revision;

        let project_b = sampler_core::ProjectId::from_bytes([0x81; 16]);
        let open_token = app.request_open_project("project-b").unwrap();
        app.take_worker_requests();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token: open_token,
            directory: "project-b".into(),
            result: Ok(crate::ProjectProbe {
                directory: "project-b".into(),
                explicit: Some(Ok(project_open_document(
                    project_b,
                    "Explicit B",
                    cleanup_revision - 1,
                    Vec::new(),
                ))),
                recovery: Some(Ok(project_open_document(
                    project_b,
                    "Recovery B",
                    cleanup_revision,
                    Vec::new(),
                ))),
            }),
        }));
        app.choose_project_recovery(crate::RecoveryChoice::Restore)
            .unwrap();
        while app.project_open_stage().is_some() {
            assert!(app.maintain_project(now));
        }

        assert_eq!(app.project_session.project_id(), project_b);
        assert_eq!(app.project_session.directory(), Some(path("project-b")));
        assert_eq!(app.project_session.autosaved_revision(), cleanup_revision);
        assert!(app.maintain_project(now));
        let cleanup_a = take_recovery_cleanup(&mut app);
        assert_eq!(cleanup_a.project_id, project_a);
        assert_eq!(cleanup_a.directory, path("project-a"));
        assert_eq!(cleanup_a.revision, cleanup_revision);
        assert!(app.apply_worker_result(WorkerResult::RecoveryDiscarded {
            token: cleanup_a.token,
            directory: cleanup_a.directory,
            project_id: cleanup_a.project_id,
            revision: cleanup_a.revision,
            result: Ok(()),
        }));

        assert_eq!(app.project_session.project_id(), project_b);
        assert_eq!(app.project_session.directory(), Some(path("project-b")));
        assert_eq!(app.project_session.autosaved_revision(), cleanup_revision);
        assert_eq!(app.project_session.saved_revision(), cleanup_revision - 1);
    }

    #[test]
    fn explicit_save_is_refused_when_the_bounded_cleanup_backlog_is_full() {
        let now = Instant::now();
        let mut app = project_app();
        name_project(&mut app, "named", now);
        for second in 0..crate::loader::WORKER_CHANNEL_CAPACITY {
            app.request_save().unwrap();
            app.maintain_project(now + Duration::from_secs(second as u64));
            let save = take_project_save(&mut app);
            assert!(app.apply_worker_result(save_result(&save, Vec::new())));
        }

        assert_eq!(
            app.request_save(),
            Err(super::ProjectSaveError::OperationPending)
        );
    }

    fn midi_channel(value: u8) -> MidiChannel {
        MidiChannel::new(value).expect("test MIDI channel is valid")
    }

    fn midi_note(value: u8) -> MidiNote {
        MidiNote::new(value).expect("test MIDI note is valid")
    }

    fn midi_on(channel: u8, note: u8, velocity: u8) -> MidiEvent {
        MidiEvent::NoteOn {
            channel: midi_channel(channel),
            note: midi_note(note),
            velocity,
        }
    }

    fn midi_off(channel: u8, note: u8) -> MidiEvent {
        MidiEvent::NoteOff {
            channel: midi_channel(channel),
            note: midi_note(note),
        }
    }

    fn midi_owner_index(channel: u8, note: u8) -> usize {
        usize::from(channel - 1) * MIDI_NOTE_COUNT + usize::from(note)
    }

    fn midi_command(value: u64) -> LiveCommandId {
        LiveCommandId::new(value).expect("test MIDI command id is nonzero")
    }

    #[test]
    fn midi_task5_default_mapping_preserves_velocity_and_latches_bank_for_release() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));

        app.apply_midi_event(midi_on(1, 36, 64));
        app.apply_midi_event(midi_on(1, 36, 0));
        app.apply_midi_event(midi_off(1, 99));
        app.apply(InputAction::BankDelta(1));
        app.apply_midi_event(midi_on(1, 37, 127));
        app.apply(InputAction::BankDelta(1));
        app.apply_midi_event(midi_off(1, 37));

        assert_eq!(
            calls.snapshot(),
            vec![
                AudioCall::TrackedTrigger(pad(0, 0), f32::from(64_u8) / 127.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
                AudioCall::TrackedTrigger(pad(1, 1), 1.0),
                AudioCall::TrackedOwnedRelease(pad(1, 1), midi_command(3)),
            ]
        );
    }

    #[test]
    fn midi_task5_numbered_filter_and_omni_channel_ownership_are_independent() {
        let filtered_audio = FakeAudio::ready(48_000, 2);
        let filtered_calls = filtered_audio.call_log();
        let mut filtered = App::with_audio(Box::new(filtered_audio));
        filtered.midi_settings =
            MidiSettings::default().with_channel(MidiChannelFilter::Channel(midi_channel(2)));

        filtered.apply_midi_event(midi_on(1, 36, 127));
        filtered.apply_midi_event(midi_on(2, 36, 127));
        assert_eq!(
            filtered_calls.snapshot(),
            vec![AudioCall::TrackedTrigger(pad(0, 0), 1.0)]
        );

        let omni_audio = FakeAudio::ready(48_000, 2);
        let omni_calls = omni_audio.call_log();
        let mut omni = App::with_audio(Box::new(omni_audio));
        omni.apply_midi_event(midi_on(1, 36, 127));
        omni.apply_midi_event(midi_on(2, 36, 127));
        omni.apply_midi_event(midi_off(1, 36));
        assert_eq!(
            omni.midi_owned_pads[midi_owner_index(2, 36)].map(|owner| owner.pad),
            Some(pad(0, 0))
        );
        omni.apply_midi_event(midi_off(2, 36));

        assert_eq!(
            omni_calls.snapshot(),
            vec![
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(2)),
            ]
        );
    }

    #[test]
    fn midi_channel_transaction_releases_rejected_owners_before_one_project_commit() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        app.apply_midi_event(midi_on(2, 36, 127));
        let revision = app.project_revision();

        app.update_midi_channel(MidiChannelFilter::Channel(midi_channel(1)))
            .unwrap();

        assert_eq!(
            calls.snapshot(),
            vec![
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
            ]
        );
        assert_eq!(
            app.midi_settings.channel(),
            MidiChannelFilter::Channel(midi_channel(1))
        );
        assert_eq!(app.midi_owned_pads[midi_owner_index(2, 36)], None);
        assert_eq!(app.project_revision(), revision + 1);
    }

    #[test]
    fn midi_settings_release_failure_preserves_settings_revision_and_exact_owner() {
        let audio = FakeAudio::ready(48_000, 2).failing_release_once("release queue full");
        let mut app = App::with_audio(Box::new(audio));
        app.apply_midi_event(midi_on(2, 36, 127));
        let before = app.midi_settings;
        let revision = app.project_revision();

        assert_eq!(
            app.update_midi_channel(MidiChannelFilter::Channel(midi_channel(1))),
            Err("release queue full".to_owned())
        );

        assert_eq!(app.midi_settings, before);
        assert_eq!(app.project_revision(), revision);
        assert_eq!(
            app.midi_owned_pads[midi_owner_index(2, 36)].map(|owner| owner.pad),
            Some(pad(0, 0))
        );
    }

    #[test]
    fn midi_settings_second_release_failure_is_collectively_atomic_for_audio_app_and_pattern() {
        let audio =
            FakeAudio::ready(48_000, 2).failing_owned_release_at(2, "second release queue full");
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        app.patterns
            .start_recording(TransportStamp {
                slot: PatternSlotId::new(0).unwrap(),
                generation: 0,
                origin: 0,
                loop_frames: 96_000,
            })
            .unwrap();
        app.apply_midi_event(midi_on(2, 36, 127));
        app.apply_midi_event(midi_on(2, 37, 127));
        calls.clear();
        let before_settings = app.midi_settings;
        let before_revision = app.project_revision();
        let before_pattern = app.patterns.selected_pattern().events().to_vec();

        assert_eq!(
            app.update_midi_channel(MidiChannelFilter::Channel(midi_channel(1))),
            Err("second release queue full".to_owned())
        );

        assert_eq!(calls.snapshot(), Vec::<AudioCall>::new());
        assert_eq!(app.midi_settings, before_settings);
        assert_eq!(app.project_revision(), before_revision);
        assert!(app.midi_owned_pads[midi_owner_index(2, 36)].is_some());
        assert!(app.midi_owned_pads[midi_owner_index(2, 37)].is_some());
        assert_eq!(app.patterns.selected_pattern().events(), before_pattern);
    }

    #[test]
    fn midi_collective_release_admits_the_exact_maximum_ownership_table_on_production_audio() {
        let (audio, controller, mut engine) = EngineAudio::harness(48_000);
        let mut app = App::with_audio(Box::new(audio));
        engine.render_frames(1, |_| {});
        assert_eq!(app.midi_owned_pads.len(), MIDI_OWNERSHIP_COUNT);
        for (index, owner) in app.midi_owned_pads.iter_mut().enumerate() {
            *owner = Some(MidiOwnedVoice {
                pad: pad(0, (index % usize::from(PADS_PER_BANK)) as u8),
                trigger_id: LiveCommandId::new((index + 1) as u64).unwrap(),
            });
        }

        app.release_all_midi_owners().unwrap();

        assert!(app.midi_owned_pads.iter().all(Option::is_none));
        assert_eq!(controller.borrow().command_overflows(), 0);
    }

    #[test]
    fn midi_settings_revision_exhaustion_prevents_audio_and_domain_mutation() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        app.apply_midi_event(midi_on(2, 36, 127));
        calls.clear();
        app.project_session
            .set_current_revision_for_test(crate::MAX_PROJECT_REVISION);
        let before = app.midi_settings;

        assert_eq!(
            app.update_midi_channel(MidiChannelFilter::Channel(midi_channel(1))),
            Err("project revision is exhausted".to_owned())
        );

        assert_eq!(calls.snapshot(), Vec::<AudioCall>::new());
        assert_eq!(app.midi_settings, before);
        assert_eq!(app.project_revision(), crate::MAX_PROJECT_REVISION);
        assert!(app.midi_owned_pads[midi_owner_index(2, 36)].is_some());
    }

    #[test]
    fn midi_unmap_reset_and_channel_noops_have_exact_revision_counts() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        assert!(app.select_pad(3));
        let initial = app.project_revision();

        app.update_midi_channel(MidiChannelFilter::Omni).unwrap();
        assert_eq!(app.project_revision(), initial);
        assert_eq!(calls.snapshot(), Vec::<AudioCall>::new());

        app.unmap_selected_midi().unwrap();
        assert_eq!(
            app.midi_settings.bank(BankId::new(0).unwrap()).note(3),
            Ok(None)
        );
        assert_eq!(app.project_revision(), initial + 1);
        app.unmap_selected_midi().unwrap();
        assert_eq!(app.project_revision(), initial + 1);

        app.reset_active_midi_bank().unwrap();
        assert_eq!(
            app.midi_settings.bank(BankId::new(0).unwrap()).note(3),
            Ok(Some(midi_note(39)))
        );
        assert_eq!(app.project_revision(), initial + 2);
        app.reset_active_midi_bank().unwrap();
        assert_eq!(app.project_revision(), initial + 2);
        assert_eq!(calls.snapshot(), Vec::<AudioCall>::new());
    }

    #[test]
    fn midi_settings_palette_commands_route_to_transactional_app_reducers() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        assert!(app.select_pad(3));

        app.open_palette();
        app.apply_terminal_event(Event::Paste("midi-channel 2".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.midi_settings.channel(),
            MidiChannelFilter::Channel(midi_channel(2))
        );
        assert_eq!(app.project_revision(), 1);

        app.open_palette();
        app.apply_terminal_event(Event::Paste("midi-learn".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.midi_learn_target(), Some(pad(0, 3)));
        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        app.open_palette();
        app.apply_terminal_event(Event::Paste("midi-unmap".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.midi_settings.bank(BankId::new(0).unwrap()).note(3),
            Ok(None)
        );
        assert_eq!(app.project_revision(), 2);

        app.open_palette();
        app.apply_terminal_event(Event::Paste("midi-reset-bank".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.midi_settings.bank(BankId::new(0).unwrap()).note(3),
            Ok(Some(midi_note(39)))
        );
        assert_eq!(app.project_revision(), 3);
    }

    #[test]
    fn midi_port_commands_control_the_runtime_service_without_project_revisions() {
        let (service, state) = fake_midi_service([("a", "Keys"), ("b", "Pads")]);
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio_and_midi(Box::new(audio), service);
        let revision = app.project_revision();

        app.open_palette();
        app.apply_terminal_event(Event::Paste("midi-ports".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.status().contains("#0 Keys"));
        assert!(app.status().contains("#1 Pads"));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("midi-connect 0".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.apply_midi_event(midi_on(2, 36, 127));
        app.arm_midi_learn();
        app.open_palette();
        app.apply_terminal_event(Event::Paste("midi-connect 1".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(state.borrow().connected, ["a", "b"]);
        assert_eq!(state.borrow().closed, 1);
        assert_eq!(app.midi_learn_target(), None);
        assert!(app.midi_owned_pads[midi_owner_index(2, 36)].is_none());

        app.apply_midi_event(midi_on(2, 36, 127));
        app.arm_midi_learn();
        app.open_palette();
        app.apply_terminal_event(Event::Paste("midi-disconnect".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(state.borrow().closed, 2);
        assert_eq!(app.midi_learn_target(), None);
        assert!(app.midi_owned_pads[midi_owner_index(2, 36)].is_none());
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(3)),
            ]
        );
        assert_eq!(app.project_revision(), revision);
    }

    #[test]
    fn midi_device_disappearance_cancels_learn_and_releases_held_owners() {
        let (mut service, state) = fake_midi_service([("a", "Keys")]);
        let now = Instant::now();
        service.startup(now).unwrap();
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio_and_midi(Box::new(audio), service);
        app.apply_midi_event(midi_on(2, 36, 127));
        app.arm_midi_learn();
        let revision = app.project_revision();
        state.borrow_mut().ports.clear();

        assert!(app.maintain_midi_service(now + Duration::from_secs(1)));

        assert_eq!(state.borrow().closed, 1);
        assert_eq!(app.midi_learn_target(), None);
        assert!(app.midi_owned_pads[midi_owner_index(2, 36)].is_none());
        assert!(app.status().contains("MIDI port disappeared: Keys"));
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
            ]
        );
        assert_eq!(app.project_revision(), revision);
    }

    #[test]
    fn midi_reconnect_retries_owner_retained_after_disappearance_release_failure() {
        let (mut service, state) = fake_midi_service([("a", "Keys")]);
        let now = Instant::now();
        service.startup(now).unwrap();
        let audio = FakeAudio::ready(48_000, 2).failing_release_once("release queue full");
        let calls = audio.call_log();
        let mut app = App::with_audio_and_midi(Box::new(audio), service);
        app.apply_midi_event(midi_on(2, 36, 127));
        state.borrow_mut().ports.clear();

        assert!(app.maintain_midi_service(now + Duration::from_secs(1)));
        assert!(
            app.status()
                .contains("held-note release failed: release queue full")
        );
        assert!(app.midi_owned_pads[midi_owner_index(2, 36)].is_some());
        assert_eq!(state.borrow().closed, 1);

        state.borrow_mut().ports.push(MidiBackendPort {
            backend_id: "a".to_owned(),
            name: "Keys".to_owned(),
        });
        app.list_midi_ports().unwrap();
        app.connect_midi_port(0).unwrap();

        assert!(app.midi_owned_pads[midi_owner_index(2, 36)].is_none());
        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
            ]
        );
        assert_eq!(
            app.midi_service
                .as_ref()
                .and_then(MidiService::connected_port)
                .map(|port| port.backend_id()),
            Some("a")
        );
    }

    #[test]
    fn midi_replacement_release_failure_preserves_old_connection_owners_and_learn_for_retry() {
        let (service, state) = fake_midi_service([("a", "Keys"), ("b", "Pads")]);
        let audio = FakeAudio::ready(48_000, 2).failing_release_once("release queue full");
        let calls = audio.call_log();
        let mut app = App::with_audio_and_midi(Box::new(audio), service);
        app.list_midi_ports().unwrap();
        app.connect_midi_port(0).unwrap();
        app.apply_midi_event(midi_on(2, 36, 127));
        app.arm_midi_learn();
        let revision = app.project_revision();

        assert_eq!(
            app.connect_midi_port(1),
            Err("release queue full".to_owned())
        );

        assert_eq!(
            app.midi_service
                .as_ref()
                .and_then(MidiService::connected_port)
                .map(|port| port.backend_id()),
            Some("a")
        );
        assert_eq!(state.borrow().closed, 1);
        assert!(app.midi_owned_pads[midi_owner_index(2, 36)].is_some());
        assert_eq!(app.midi_learn_target(), Some(pad(0, 0)));
        assert_eq!(
            calls.snapshot(),
            [AudioCall::TrackedTrigger(pad(0, 0), 1.0)]
        );
        assert_eq!(app.project_revision(), revision);

        app.connect_midi_port(1).unwrap();
        assert_eq!(
            app.midi_service
                .as_ref()
                .and_then(MidiService::connected_port)
                .map(|port| port.backend_id()),
            Some("b")
        );
        assert_eq!(state.borrow().closed, 2);
        assert!(app.midi_owned_pads[midi_owner_index(2, 36)].is_none());
        assert_eq!(app.midi_learn_target(), None);
    }

    #[test]
    fn midi_failed_replacement_preserves_healthy_connection_owners_and_learn() {
        let (service, state) = fake_midi_service([("a", "Keys"), ("b", "Pads")]);
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio_and_midi(Box::new(audio), service);
        app.list_midi_ports().unwrap();
        app.connect_midi_port(0).unwrap();
        app.apply_midi_event(midi_on(2, 36, 127));
        app.arm_midi_learn();
        state.borrow_mut().connect_error_for = Some("b".to_owned());
        let revision = app.project_revision();

        assert_eq!(
            app.connect_midi_port(1),
            Err("could not connect MIDI input: candidate refused".to_owned())
        );

        assert_eq!(
            app.midi_service
                .as_ref()
                .and_then(MidiService::connected_port)
                .map(|port| port.backend_id()),
            Some("a")
        );
        assert_eq!(state.borrow().closed, 0);
        assert!(app.midi_owned_pads[midi_owner_index(2, 36)].is_some());
        assert_eq!(app.midi_learn_target(), Some(pad(0, 0)));
        assert_eq!(
            calls.snapshot(),
            [AudioCall::TrackedTrigger(pad(0, 0), 1.0)]
        );
        assert_eq!(app.project_revision(), revision);
    }

    #[test]
    fn app_drops_midi_callback_ownership_before_audio() {
        let lifecycle = Rc::new(RefCell::new(Vec::new()));
        let (mut service, state) = fake_midi_service([("a", "Keys")]);
        state.borrow_mut().lifecycle = Some(Rc::clone(&lifecycle));
        service.startup(Instant::now()).unwrap();
        let audio = FakeAudio::ready(48_000, 2).with_shutdown_log(Rc::clone(&lifecycle));

        drop(App::with_audio_and_midi(Box::new(audio), service));

        assert_eq!(*lifecycle.borrow(), ["close-midi", "drop-audio"]);
    }

    #[test]
    fn midi_learn_escape_cancels_the_captured_target_without_mutating_the_project() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        assert!(app.select_pad(5));
        let revision = app.project_revision();
        app.arm_midi_learn();
        assert_eq!(app.midi_learn_target(), Some(pad(0, 5)));

        app.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(app.midi_learn_target(), None);
        assert_eq!(app.project_revision(), revision);
    }

    #[test]
    fn midi_learn_is_canceled_when_project_open_is_admitted() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        assert!(app.select_pad(7));
        app.arm_midi_learn();

        app.request_open_project("next-project").unwrap();

        assert_eq!(app.midi_learn_target(), None);
    }

    #[test]
    fn midi_learn_is_canceled_when_save_or_save_as_is_admitted() {
        let mut save = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        name_project(&mut save, "project-a", Instant::now());
        save.arm_midi_learn();

        save.request_save().unwrap();

        assert_eq!(save.midi_learn_target(), None);

        let mut save_as = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        save_as.arm_midi_learn();

        save_as.request_save_as("project-b").unwrap();

        assert_eq!(save_as.midi_learn_target(), None);
    }

    #[test]
    fn midi_learn_is_canceled_on_audio_device_loss() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.arm_midi_learn();

        app.fail_audio("device disappeared".to_owned());

        assert_eq!(app.midi_learn_target(), None);
    }

    #[test]
    fn midi_learn_swaps_the_captured_pad_releases_displaced_owner_and_performs_once() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        app.apply_midi_event(midi_on(1, 36, 64));
        assert!(app.select_pad(5));
        app.arm_midi_learn();
        let revision = app.project_revision();

        app.apply_midi_event(midi_on(1, 36, 96));

        assert_eq!(
            calls.snapshot(),
            vec![
                AudioCall::TrackedTrigger(pad(0, 0), f32::from(64_u8) / 127.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
                AudioCall::TrackedTrigger(pad(0, 5), f32::from(96_u8) / 127.0),
            ]
        );
        assert_eq!(
            app.midi_settings.bank(BankId::new(0).unwrap()).note(5),
            Ok(Some(midi_note(36)))
        );
        assert_eq!(
            app.midi_settings.bank(BankId::new(0).unwrap()).note(0),
            Ok(Some(midi_note(41)))
        );
        assert_eq!(app.project_revision(), revision + 1);
        assert_eq!(app.midi_learn_target(), None);
        assert_eq!(app.selected_pad(), 5);
        assert_eq!(
            app.midi_owned_pads[midi_owner_index(1, 36)].map(|owner| owner.pad),
            Some(pad(0, 5))
        );
    }

    #[test]
    fn midi_release_targets_the_trigger_owned_by_that_channel_and_note() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));

        app.apply_midi_event(midi_on(1, 36, 127));
        app.apply_midi_event(midi_on(2, 36, 127));
        app.apply_midi_event(midi_off(1, 36));

        assert_eq!(
            calls.snapshot(),
            vec![
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), LiveCommandId::FIRST),
            ]
        );
        assert!(app.midi_owned_pads[midi_owner_index(2, 36)].is_some());
    }

    #[test]
    fn midi_task5_repeated_note_releases_old_owner_before_remapped_trigger() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        app.apply_midi_event(midi_on(1, 36, 127));
        app.midi_settings = app
            .midi_settings
            .learn_swap(BankId::new(0).unwrap(), 1, midi_note(36))
            .unwrap();

        app.apply_midi_event(midi_on(1, 36, 32));

        assert_eq!(
            calls.snapshot(),
            vec![
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
                AudioCall::TrackedTrigger(pad(0, 1), f32::from(32_u8) / 127.0),
            ]
        );
        assert_eq!(
            app.midi_owned_pads[midi_owner_index(1, 36)].map(|owner| owner.pad),
            Some(pad(0, 1))
        );
    }

    #[test]
    fn midi_task5_failed_repeated_release_retains_old_owner_and_aborts_trigger() {
        let audio = FakeAudio::ready(48_000, 2).failing_release_once("release queue full");
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        app.apply_midi_event(midi_on(1, 36, 127));
        app.midi_settings = app
            .midi_settings
            .learn_swap(BankId::new(0).unwrap(), 1, midi_note(36))
            .unwrap();

        app.apply_midi_event(midi_on(1, 36, 64));
        assert_eq!(
            app.midi_owned_pads[midi_owner_index(1, 36)].map(|owner| owner.pad),
            Some(pad(0, 0))
        );
        app.apply_midi_event(midi_off(1, 36));

        assert_eq!(
            calls.snapshot(),
            vec![
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
            ]
        );
        assert_eq!(app.midi_owned_pads[midi_owner_index(1, 36)], None);
    }

    #[test]
    fn midi_task5_trigger_and_noteoff_failures_preserve_exact_ownership() {
        let trigger_audio = FakeAudio::ready(48_000, 2).failing_trigger("trigger queue full");
        let mut trigger_app = App::with_audio(Box::new(trigger_audio));
        trigger_app.apply_midi_event(midi_on(1, 36, 127));
        trigger_app.apply_midi_event(midi_off(1, 36));
        assert_eq!(trigger_app.midi_owned_pads[midi_owner_index(1, 36)], None);

        let release_audio = FakeAudio::ready(48_000, 2).failing_release_once("release queue full");
        let release_calls = release_audio.call_log();
        let mut release_app = App::with_audio(Box::new(release_audio));
        release_app.apply_midi_event(midi_on(1, 36, 127));
        release_app.apply_midi_event(midi_off(1, 36));
        assert_eq!(
            release_app.midi_owned_pads[midi_owner_index(1, 36)].map(|owner| owner.pad),
            Some(pad(0, 0))
        );
        release_app.apply_midi_event(midi_off(1, 36));
        assert_eq!(release_app.midi_owned_pads[midi_owner_index(1, 36)], None);
        assert_eq!(
            release_calls.snapshot(),
            vec![
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
            ]
        );
    }

    #[test]
    fn midi_task5_stop_all_clears_owners_only_after_audio_admission() {
        let audio = FakeAudio::ready(48_000, 2).failing_stop_all_once("stop queue full");
        let mut app = App::with_audio(Box::new(audio));
        app.apply_midi_event(midi_on(1, 36, 127));

        app.apply(InputAction::StopAll);
        assert_eq!(
            app.midi_owned_pads[midi_owner_index(1, 36)].map(|owner| owner.pad),
            Some(pad(0, 0))
        );
        app.apply(InputAction::StopAll);
        assert_eq!(app.midi_owned_pads[midi_owner_index(1, 36)], None);
    }

    #[test]
    fn midi_task5_trigger_fences_allow_releases_and_ordinary_overlays_remain_playable() {
        let audio = FakeAudio::ready(48_000, 2);
        let calls = audio.call_log();
        let mut app = App::with_audio(Box::new(audio));
        app.apply_midi_event(midi_on(1, 36, 127));
        app.should_quit = true;
        app.apply_midi_event(midi_on(1, 37, 127));
        app.apply_midi_event(midi_off(1, 36));
        app.should_quit = false;

        app.overlay = Some(super::Overlay::CaptureConfirm);
        app.apply_midi_event(midi_on(1, 38, 127));
        app.overlay = Some(super::Overlay::ProjectSaveProgress);
        app.apply_midi_event(midi_on(1, 38, 127));
        app.overlay = Some(super::Overlay::Help);
        app.apply_midi_event(midi_on(1, 39, 127));
        app.apply_midi_event(midi_on(1, 41, 127));
        let operation = app
            .build_project_open_candidate(
                crate::ProjectToken::new(991),
                PathBuf::from("midi-project"),
                project_open_document(
                    sampler_core::ProjectId::from_bytes([0x99; 16]),
                    "MIDI project",
                    0,
                    Vec::new(),
                ),
                0,
                false,
            )
            .unwrap();
        let super::ProjectOpenOperation::Staging(mut candidate) = operation else {
            panic!("project fixture must enter staging")
        };
        candidate.progress.phase = crate::ProjectOpenPhase::Admitting;
        candidate.admission = super::ProjectAdmission::MidiOwners;
        app.project_open = Some(super::ProjectOpenOperation::Staging(candidate));
        app.apply_midi_event(midi_on(1, 40, 127));
        app.apply_midi_event(midi_off(1, 39));
        assert!(app.maintain_project(Instant::now()));
        assert_eq!(app.midi_owned_pads[midi_owner_index(1, 41)], None);
        app.project_open = None;

        app.overlay = None;
        app.capture_session
            .begin(sampler_audio::CaptureSource::Resample, pad(0, 0), 48_000, 4)
            .unwrap();
        app.capture_session.mark_arming().unwrap();
        app.capture_session.mark_recording().unwrap();
        app.apply_midi_event(midi_on(1, 41, 127));

        assert_eq!(
            calls.snapshot(),
            vec![
                AudioCall::TrackedTrigger(pad(0, 0), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 0), midi_command(1)),
                AudioCall::TrackedTrigger(pad(0, 3), 1.0),
                AudioCall::TrackedTrigger(pad(0, 5), 1.0),
                AudioCall::TrackedOwnedRelease(pad(0, 3), midi_command(3)),
                AudioCall::TrackedOwnedRelease(pad(0, 5), midi_command(4)),
                AudioCall::StopAll,
                AudioCall::TrackedTrigger(pad(0, 5), 1.0),
            ]
        );
    }

    #[test]
    fn midi_task5_unavailable_audio_never_creates_ownership() {
        let mut app = App::without_audio("no output device");

        app.apply_midi_event(midi_on(1, 36, 127));
        app.apply_midi_event(midi_off(1, 36));

        assert_eq!(app.midi_owned_pads[midi_owner_index(1, 36)], None);
        assert_eq!(app.status, "no output device");
    }

    #[test]
    fn midi_engine_loss_discards_stale_ownership_before_retrigger() {
        let failed = FakeAudio::ready(48_000, 2).failing_runtime("device disconnected");
        let mut app = App::with_audio(Box::new(failed));
        app.apply_midi_event(midi_on(1, 36, 127));

        app.maintain_audio();
        assert!(app.midi_owned_pads[midi_owner_index(1, 36)].is_none());

        let replacement = FakeAudio::ready(48_000, 2);
        let replacement_calls = replacement.call_log();
        assert!(app.retry_with(Box::new(replacement)));
        app.apply_midi_event(midi_on(1, 36, 64));

        assert_eq!(
            replacement_calls.snapshot(),
            vec![AudioCall::TrackedTrigger(
                pad(0, 0),
                f32::from(64_u8) / 127.0,
            )]
        );
    }

    #[test]
    fn midi_task5_real_engine_acks_record_exact_velocity_frame_and_gate_duration() {
        let (audio, controller, mut engine) = EngineAudio::harness(100);
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0, 0);
        let settings = PadSettings {
            mode: PlaybackMode::Gate,
            ..PadSettings::default()
        };
        app.pads[super::pad_offset(target)].settings = settings;
        controller
            .borrow_mut()
            .install(
                target,
                Arc::new(SampleBuffer::new(100, [0.25, -0.25].repeat(1_000)).unwrap()),
                settings,
                sampler_core::PadMixSettings::default(),
            )
            .unwrap();

        app.maintain_audio();
        engine.render_frames(4, |_| {});
        app.maintain_audio();
        assert!(app.patterns.is_slot_ready(app.patterns.selected_slot()));
        app.start_pattern_recording();
        engine.render_frames(4, |_| {});
        app.maintain_audio();
        assert!(app.patterns.is_recording());

        let origin = app.telemetry.pattern_origin.unwrap();
        let trigger_absolute = engine.rendered_frame() + 64;
        app.apply_midi_event(midi_on(1, 36, 50));
        engine.render_frames(65, |_| {});
        engine.render_frames(9, |_| {});
        let release_absolute = engine.rendered_frame() + 64;
        app.apply_midi_event(midi_off(1, 36));
        engine.render_frames(65, |_| {});
        app.maintain_audio();

        let event = app.patterns.selected_pattern().events()[0];
        assert_eq!(event.pad, target);
        assert_eq!(
            event.frame,
            (trigger_absolute - origin) % app.patterns.selected_pattern().transport().loop_frames()
        );
        assert_eq!(event.velocity, f32::from(50_u8) / 127.0);
        assert_eq!(event.duration, Some(release_absolute - trigger_absolute));
    }

    #[test]
    fn midi_same_pad_gate_noteoff_releases_only_its_owned_engine_voice() {
        let (audio, controller, mut engine) = EngineAudio::harness(100);
        let mut app = App::with_audio(Box::new(audio));
        let target = pad(0, 0);
        let settings = PadSettings {
            mode: PlaybackMode::Gate,
            ..PadSettings::default()
        };
        app.pads[super::pad_offset(target)].settings = settings;
        controller
            .borrow_mut()
            .install(
                target,
                Arc::new(SampleBuffer::new(100, [0.25, -0.25].repeat(1_000)).unwrap()),
                settings,
                sampler_core::PadMixSettings::default(),
            )
            .unwrap();
        engine.render_frames(0, |_| {});

        app.apply_midi_event(midi_on(1, 36, 127));
        app.apply_midi_event(midi_on(2, 36, 127));
        engine.render_frames(65, |_| {});
        assert_eq!(engine.active_voices(), 2);

        app.apply_midi_event(midi_off(1, 36));
        engine.render_frames(129, |_| {});
        assert_eq!(engine.active_voices(), 1);

        app.apply_midi_event(midi_off(2, 36));
        engine.render_frames(129, |_| {});
        assert_eq!(engine.active_voices(), 0);
    }

    #[test]
    fn midi_task5_real_engine_preserves_choke_group_behavior() {
        let (audio, controller, mut engine) = EngineAudio::harness(100);
        let mut app = App::with_audio(Box::new(audio));
        let group = sampler_core::ChokeGroup::new(1).unwrap();
        let settings = PadSettings {
            choke_group: Some(group),
            ..PadSettings::default()
        };
        let sample = Arc::new(SampleBuffer::new(100, [0.25, -0.25].repeat(1_000)).unwrap());
        for target in [pad(0, 0), pad(0, 1)] {
            controller
                .borrow_mut()
                .install(
                    target,
                    Arc::clone(&sample),
                    settings,
                    sampler_core::PadMixSettings::default(),
                )
                .unwrap();
        }
        engine.render_frames(0, |_| {});

        app.apply_midi_event(midi_on(1, 36, 127));
        engine.render_frames(65, |_| {});
        assert_eq!(engine.active_voices(), 1);
        app.apply_midi_event(midi_on(1, 37, 127));
        engine.render_frames(129, |_| {});

        assert_eq!(engine.active_voices(), 1);
    }

    #[test]
    fn project_snapshot_contains_the_exact_app_owned_midi_settings() {
        let mut app = project_app();
        let midi = MidiSettings::default()
            .with_channel(MidiChannelFilter::Channel(MidiChannel::new(9).unwrap()))
            .learn_swap(BankId::new(1).unwrap(), 3, MidiNote::new(90).unwrap())
            .unwrap()
            .learn_swap(BankId::new(8).unwrap(), 12, MidiNote::new(17).unwrap())
            .unwrap()
            .unmap(BankId::new(4).unwrap(), 6)
            .unwrap();
        app.midi_settings = midi;

        assert_eq!(app.project_snapshot().unwrap().midi, midi);
    }
}
