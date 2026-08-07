use std::array;
use std::path::PathBuf;
use std::sync::Arc;

use sampler_audio::SampleBuffer;
use sampler_core::pad::{BANK_COUNT, PADS_PER_BANK};
use sampler_core::{BankId, PadId, PadSettings};

use crate::audio::AudioPort;
use crate::input::InputAction;

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

    fn press_pad(&mut self, index: usize) {
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
        let Some(pad) = self.held_pad_by_key[index].take() else {
            return;
        };
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        let at = audio
            .render_horizon()
            .saturating_add(LIVE_SCHEDULE_AHEAD_FRAMES);
        if let Err(error) = audio.release(pad, at) {
            self.status = error;
        }
    }

    fn stop_pad(&mut self, index: usize) {
        let Some(pad) = self.pad_in_active_bank(index) else {
            return;
        };
        self.selected_pad = index;
        self.held_pad_by_key[index] = None;
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        if let Err(error) = audio.stop_pad(pad) {
            self.status = error;
        }
    }

    fn stop_all(&mut self) {
        self.held_pad_by_key.fill(None);
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        if let Err(error) = audio.stop_all() {
            self.status = error;
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

    use super::App;

    #[derive(Debug, Clone, PartialEq)]
    enum AudioCall {
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
        calls: CallLog,
    }

    impl FakeAudio {
        fn ready(sample_rate: u32, channels: u16) -> Self {
            Self {
                sample_rate,
                channels,
                horizon: 0,
                trigger_error: None,
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

        fn call_log(&self) -> CallLog {
            self.calls.clone()
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
            _pad: PadId,
            _sample: Arc<SampleBuffer>,
            _settings: PadSettings,
        ) -> Result<SampleSlot, String> {
            Err("sample installation is not used by these tests".to_owned())
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
            self.calls.0.borrow_mut().push(AudioCall::Release(pad, at));
            Ok(())
        }

        fn stop_pad(&mut self, pad: PadId) -> Result<(), String> {
            self.calls.0.borrow_mut().push(AudioCall::StopPad(pad));
            Ok(())
        }

        fn stop_all(&mut self) -> Result<(), String> {
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
