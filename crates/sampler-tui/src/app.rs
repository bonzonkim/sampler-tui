use std::array;
use std::path::PathBuf;
use std::sync::Arc;

use sampler_audio::SampleBuffer;
use sampler_core::pad::{BANK_COUNT, PADS_PER_BANK};
use sampler_core::{BankId, PadId, PadSettings};

use crate::audio::AudioPort;
use crate::input::InputAction;
use crate::loader::{WorkerRequest, WorkerResult};

pub const PAD_VIEW_COUNT: usize = 160;
pub const PREVIEW_COLUMNS: usize = 64;
const LIVE_SCHEDULE_AHEAD_FRAMES: u64 = 64;

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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Help,
    Palette,
    FilePicker,
    ConfirmQuit,
    DeviceError(String),
}

pub struct App {
    active_bank: BankId,
    selected_pad: usize,
    pads: [PadView; PAD_VIEW_COUNT],
    audio: Option<Box<dyn AudioPort>>,
    held_pad_by_key: [Option<PadId>; PADS_PER_BANK as usize],
    overlay: Option<Overlay>,
    status: String,
    audio_unavailable_message: Option<String>,
    should_quit: bool,
}

impl App {
    pub fn with_audio(audio: Box<dyn AudioPort>) -> Self {
        Self::new(Some(audio), None)
    }

    pub fn without_audio(error: impl Into<String>) -> Self {
        let error = error.into();
        Self::new(None, Some(error))
    }

    fn new(audio: Option<Box<dyn AudioPort>>, audio_error: Option<String>) -> Self {
        let overlay = audio_error.clone().map(Overlay::DeviceError);
        Self {
            active_bank: BankId::new(0).expect("bank zero is valid"),
            selected_pad: 0,
            pads: array::from_fn(|_| PadView::default()),
            audio,
            held_pad_by_key: [None; PADS_PER_BANK as usize],
            overlay,
            status: audio_error.clone().unwrap_or_default(),
            audio_unavailable_message: audio_error,
            should_quit: false,
        }
    }

    pub fn apply(&mut self, action: InputAction) {
        match action {
            InputAction::PadPress(index) => self.press_pad(index),
            InputAction::PadRelease(index) => self.release_pad(index),
            InputAction::PadStop(index) => self.stop_pad(index),
            InputAction::BankDelta(delta) => self.change_bank(delta),
            InputAction::StopAll => self.stop_all(),
            InputAction::Quit => self.should_quit = true,
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

    pub fn pad(&self, pad: PadId) -> &PadView {
        &self.pads[pad_offset(pad)]
    }

    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn begin_load(&mut self, pad: PadId, path: impl Into<PathBuf>) -> Option<WorkerRequest> {
        let path = path.into();
        let engine_rate = self.audio.as_ref().map(|audio| audio.sample_rate());
        let view = &mut self.pads[pad_offset(pad)];
        view.generation = view.generation.wrapping_add(1);
        view.source = Some(path.clone());
        view.state = if engine_rate.is_some() {
            PadLoadState::Loading
        } else {
            PadLoadState::WaitingForDevice
        };

        engine_rate.map(|engine_rate| WorkerRequest::LoadSample {
            pad,
            generation: view.generation,
            path,
            engine_rate,
        })
    }

    pub fn apply_worker_result(&mut self, result: WorkerResult) -> bool {
        let WorkerResult::Loaded {
            pad,
            generation,
            path,
            result,
        } = result
        else {
            return false;
        };
        let offset = pad_offset(pad);
        if self.pads[offset].generation != generation
            || self.pads[offset].source.as_deref() != Some(path.as_path())
        {
            return false;
        }

        let loaded = match result {
            Ok(loaded) => loaded,
            Err(error) => {
                self.pads[offset].state = PadLoadState::Error(error.clone());
                self.status = error;
                return true;
            }
        };
        let Some(audio) = self.audio.as_mut() else {
            self.pads[offset].state = PadLoadState::WaitingForDevice;
            return true;
        };
        let settings = self.pads[offset].settings;
        if let Err(error) = audio.install(pad, Arc::clone(&loaded.buffer), settings) {
            self.pads[offset].state = PadLoadState::Error(error.clone());
            self.status = error;
            return true;
        }

        let view = &mut self.pads[offset];
        view.label = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        view.source = Some(path);
        view.sample = Some(loaded.buffer);
        view.preview = loaded.preview;
        view.state = PadLoadState::Ready;
        true
    }

    fn press_pad(&mut self, index: usize) {
        if self.held_pad_by_key.get(index).is_some_and(Option::is_some) {
            return;
        }
        let Some(pad) = self.pad_in_active_bank(index) else {
            return;
        };
        self.selected_pad = index;
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        let at = audio
            .render_horizon()
            .saturating_add(LIVE_SCHEDULE_AHEAD_FRAMES);
        match audio.trigger(pad, at, 1.0) {
            Ok(()) => self.held_pad_by_key[index] = Some(pad),
            Err(error) => self.status = error,
        }
    }

    fn release_pad(&mut self, index: usize) {
        if !self.validate_pad_index(index) {
            return;
        }
        let Some(pad) = self.held_pad_by_key[index] else {
            return;
        };
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        let at = audio
            .render_horizon()
            .saturating_add(LIVE_SCHEDULE_AHEAD_FRAMES);
        match audio.release(pad, at) {
            Ok(()) => self.held_pad_by_key[index] = None,
            Err(error) => self.status = error,
        }
    }

    fn stop_pad(&mut self, index: usize) {
        let Some(pad) = self.pad_in_active_bank(index) else {
            return;
        };
        self.selected_pad = index;
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
            Ok(()) => self.held_pad_by_key.fill(None),
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
        let value = u8::try_from(requested).expect("bounded bank fits in u8");
        self.active_bank = BankId::new(value).expect("bounded bank is valid");
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
}

fn pad_offset(pad: PadId) -> usize {
    usize::from(u8::from(pad.bank())) * usize::from(PADS_PER_BANK) + usize::from(pad.index())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    use sampler_audio::{Frame, SampleBuffer, SampleSlot, Telemetry};
    use sampler_core::{BankId, PadId, PadSettings};

    use crate::audio::AudioPort;
    use crate::input::InputAction;

    use crate::loader::{LoadedSample, WorkerRequest, WorkerResult};

    use super::{App, PadLoadState, PreviewColumn};

    #[derive(Debug, Clone, PartialEq)]
    enum AudioCall {
        Install(PadId),
        Trigger(PadId, Frame, f32),
        Release(PadId, Frame),
        StopPad(PadId),
        StopAll,
    }

    #[derive(Clone)]
    struct CallLog(Rc<RefCell<Vec<AudioCall>>>);

    impl CallLog {
        fn snapshot(&self) -> Vec<AudioCall> {
            self.0.borrow().clone()
        }
    }

    struct FakeAudio {
        sample_rate: u32,
        channels: u16,
        horizon: Frame,
        trigger_error: Option<String>,
        release_error: Option<String>,
        stop_pad_error: Option<String>,
        stop_all_error: Option<String>,
        install_error: Option<String>,
        calls: CallLog,
    }

    impl FakeAudio {
        fn ready(sample_rate: u32, channels: u16) -> Self {
            Self {
                sample_rate,
                channels,
                horizon: 0,
                trigger_error: None,
                release_error: None,
                stop_pad_error: None,
                stop_all_error: None,
                install_error: None,
                calls: CallLog(Rc::new(RefCell::new(Vec::new()))),
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

        fn failing_stop_pad_once(mut self, error: &str) -> Self {
            self.stop_pad_error = Some(error.to_owned());
            self
        }

        fn failing_stop_all_once(mut self, error: &str) -> Self {
            self.stop_all_error = Some(error.to_owned());
            self
        }

        fn call_log(&self) -> CallLog {
            self.calls.clone()
        }

        fn failing_install(mut self, error: &str) -> Self {
            self.install_error = Some(error.to_owned());
            self
        }
    }

    impl AudioPort for FakeAudio {
        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn channels(&self) -> u16 {
            self.channels
        }

        fn render_horizon(&self) -> Frame {
            self.horizon
        }

        fn install(
            &mut self,
            pad: PadId,
            _sample: Arc<SampleBuffer>,
            _settings: PadSettings,
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

        fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String> {
            if let Some(error) = self.release_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::Release(pad, at));
            Ok(())
        }

        fn stop_pad(&mut self, pad: PadId) -> Result<(), String> {
            if let Some(error) = self.stop_pad_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::StopPad(pad));
            Ok(())
        }

        fn stop_all(&mut self) -> Result<(), String> {
            if let Some(error) = self.stop_all_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::StopAll);
            Ok(())
        }

        fn update_pad(&mut self, _pad: PadId, _settings: PadSettings) -> Result<(), String> {
            Ok(())
        }

        fn reclaim_retired(&mut self) -> usize {
            0
        }

        fn latest_telemetry(&mut self) -> Option<Telemetry> {
            None
        }

        fn poll_runtime_error(&mut self) -> Option<String> {
            None
        }
    }

    fn pad(bank: u8, index: u8) -> PadId {
        PadId::new(BankId::new(bank).unwrap(), index).unwrap()
    }

    fn path(value: &str) -> &std::path::Path {
        std::path::Path::new(value)
    }

    fn loaded(pad: PadId, generation: u64, source: &str) -> WorkerResult {
        let buffer = Arc::new(SampleBuffer::new(48_000, vec![0.25, -0.25]).unwrap());
        WorkerResult::Loaded {
            pad,
            generation,
            path: source.into(),
            result: Ok(LoadedSample {
                buffer,
                source_rate: 48_000,
                source_frames: 1,
                duration: std::time::Duration::from_nanos(20_833),
                preview: [PreviewColumn { min: -2, max: 2 }; 64],
            }),
        }
    }

    #[test]
    fn app_discards_a_superseded_load_generation() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.begin_load(pad(0, 0), path("old.wav"));
        let old_generation = app.pad(pad(0, 0)).generation;
        app.begin_load(pad(0, 0), path("new.wav"));

        app.apply_worker_result(loaded(pad(0, 0), old_generation, "old.wav"));

        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("new.wav")));
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Loading);
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
        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("kick.wav")));
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::WaitingForDevice);
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
    fn bank_navigation_is_bounded_and_release_targets_the_original_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
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
    fn failed_stop_pad_keeps_the_slot_held_until_stop_retry_succeeds() {
        let fake = FakeAudio::ready(48_000, 2).failing_stop_pad_once("stop queue is full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

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

        assert!(calls.snapshot().is_empty());
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
}
