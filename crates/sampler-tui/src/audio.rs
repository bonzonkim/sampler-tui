use std::array;
use std::cell::RefCell;
use std::fmt::Display;
use std::sync::Arc;

use sampler_audio::{
    AudioSession, CaptureBuffer, CaptureCommand, CaptureOutcome, CaptureProgressSnapshot,
    CaptureSendFailure, CaptureSource, CaptureStatus, ControlError, DeviceError, Frame,
    InputCaptureSession, LiveAck, LiveCommandId, PATTERN_SNAPSHOT_SLOT_COUNT, PatternSnapshotSlot,
    PatternSwitch, SAMPLE_SLOT_COUNT, SampleBuffer, SampleSlot, Telemetry,
};
use sampler_core::{
    MasterMixSettings, PadId, PadMixSettings, PadSettings, PatternSlotId, PatternSnapshot,
};

use crate::CaptureError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSupport {
    Unsupported,
    Available,
}

#[derive(Debug)]
pub struct CaptureCommandFailure {
    error: CaptureError,
    command: CaptureCommand,
}

impl CaptureCommandFailure {
    fn from_send(failure: CaptureSendFailure) -> Self {
        let error = CaptureError::Command(failure.error());
        let command = failure.into_command();
        Self { error, command }
    }

    fn rejected(error: CaptureError, command: CaptureCommand) -> Self {
        Self { error, command }
    }

    pub const fn error(&self) -> &CaptureError {
        &self.error
    }

    pub fn into_command(self) -> CaptureCommand {
        self.command
    }
}

#[derive(Debug, Default)]
struct CaptureSourceMaintenance {
    completion: Option<CaptureOutcome>,
    runtime_error: Option<CaptureError>,
}

#[derive(Debug, Default)]
pub struct CaptureMaintenance {
    output: CaptureSourceMaintenance,
    input: CaptureSourceMaintenance,
}

impl CaptureMaintenance {
    pub fn completion(&self, source: CaptureSource) -> Option<&CaptureOutcome> {
        match source {
            CaptureSource::Resample => self.output.completion.as_ref(),
            CaptureSource::Input => self.input.completion.as_ref(),
        }
    }

    pub fn take_completion(&mut self, source: CaptureSource) -> Option<CaptureOutcome> {
        match source {
            CaptureSource::Resample => self.output.completion.take(),
            CaptureSource::Input => self.input.completion.take(),
        }
    }

    pub fn runtime_error(&self, source: CaptureSource) -> Option<&CaptureError> {
        match source {
            CaptureSource::Resample => self.output.runtime_error.as_ref(),
            CaptureSource::Input => self.input.runtime_error.as_ref(),
        }
    }
}

pub trait AudioPort {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn render_horizon(&self) -> Frame;
    fn install(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: PadMixSettings,
    ) -> Result<SampleSlot, String>;
    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: PadMixSettings,
    ) -> Result<SampleSlot, String> {
        self.install(pad, sample, settings, mix)
    }
    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String>;
    fn trigger_live(&mut self, pad: PadId, velocity: f32) -> Result<(), String> {
        let at = self.render_horizon().saturating_add(64);
        self.trigger(pad, at, velocity)
    }
    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String>;
    fn release_live(&mut self, pad: PadId) -> Result<(), String> {
        let at = self.render_horizon().saturating_add(64);
        self.release(pad, at)
    }
    fn trigger_live_tracked(
        &mut self,
        _pad: PadId,
        _velocity: f32,
    ) -> Result<LiveCommandId, String> {
        Err("tracked live audio is unsupported".into())
    }
    fn release_live_tracked(&mut self, _pad: PadId) -> Result<LiveCommandId, String> {
        Err("tracked live audio is unsupported".into())
    }
    fn install_pattern(
        &mut self,
        _snapshot: Arc<PatternSnapshot>,
    ) -> Result<PatternSnapshotSlot, String> {
        Err("pattern audio is unsupported".into())
    }
    fn select_pattern(
        &mut self,
        _slot: PatternSlotId,
        _switch: PatternSwitch,
    ) -> Result<(), String> {
        Err("pattern audio is unsupported".into())
    }
    fn play_pattern(&mut self) -> Result<(), String> {
        Err("pattern audio is unsupported".into())
    }
    fn stop_pattern(&mut self) -> Result<(), String> {
        Err("pattern audio is unsupported".into())
    }
    fn set_record_capture(&mut self, _capture: Option<(PatternSlotId, u64)>) -> Result<(), String> {
        Err("pattern audio is unsupported".into())
    }
    fn drain_live_acks(&mut self, _output: &mut [LiveAck]) -> usize {
        0
    }
    fn reclaim_retired_patterns(&mut self) -> usize {
        0
    }
    fn remove_sample(&mut self, _pad: PadId) -> Result<(), String> {
        Err("sample removal is unsupported".into())
    }
    fn stop_pad(&mut self, pad: PadId) -> Result<(), String>;
    fn stop_all(&mut self) -> Result<(), String>;
    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String>;
    fn update_pad_mix(&mut self, _pad: PadId, _settings: PadMixSettings) -> Result<(), String> {
        Ok(())
    }
    fn update_master_mix(&mut self, _settings: MasterMixSettings) -> Result<(), String> {
        Ok(())
    }
    fn reclaim_retired(&mut self) -> usize;
    fn latest_telemetry(&mut self) -> Option<Telemetry>;
    fn poll_runtime_error(&mut self) -> Option<String>;

    fn capture_support(&self) -> CaptureSupport;

    fn capture_source_rate(&mut self, _source: CaptureSource) -> Result<u32, CaptureError> {
        Err(CaptureError::Unsupported)
    }

    fn begin_capture(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureCommandFailure> {
        Err(CaptureCommandFailure::rejected(
            CaptureError::Unsupported,
            CaptureCommand::Arm(buffer),
        ))
    }

    fn start_capture(
        &mut self,
        _source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        Err(CaptureCommandFailure::rejected(
            CaptureError::Unsupported,
            CaptureCommand::Start { token },
        ))
    }

    fn stop_capture(
        &mut self,
        _source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        Err(CaptureCommandFailure::rejected(
            CaptureError::Unsupported,
            CaptureCommand::Stop { token },
        ))
    }

    fn cancel_capture(
        &mut self,
        _source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        Err(CaptureCommandFailure::rejected(
            CaptureError::Unsupported,
            CaptureCommand::Cancel { token },
        ))
    }

    fn capture_status(&mut self, _source: CaptureSource) -> Option<CaptureStatus> {
        None
    }

    fn capture_completion(&mut self, _source: CaptureSource) -> Option<CaptureOutcome> {
        None
    }

    fn capture_runtime_error(&mut self, _source: CaptureSource) -> Option<CaptureError> {
        None
    }

    fn poll_capture_maintenance(&mut self) -> CaptureMaintenance {
        CaptureMaintenance {
            output: CaptureSourceMaintenance {
                completion: self.capture_completion(CaptureSource::Resample),
                runtime_error: self.capture_runtime_error(CaptureSource::Resample),
            },
            input: CaptureSourceMaintenance {
                completion: self.capture_completion(CaptureSource::Input),
                runtime_error: self.capture_runtime_error(CaptureSource::Input),
            },
        }
    }
}

trait SessionLike {
    type CommandError: Display;
    type RuntimeError: Display;

    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn render_horizon(&mut self) -> Frame;
    fn install(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: PadMixSettings,
    ) -> Result<SampleSlot, Self::CommandError>;
    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: PadMixSettings,
    ) -> Result<SampleSlot, Self::CommandError> {
        self.install(pad, sample, settings, mix)
    }
    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), Self::CommandError>;
    fn trigger_live(&mut self, pad: PadId, velocity: f32) -> Result<(), Self::CommandError> {
        let at = self.render_horizon().saturating_add(64);
        self.trigger(pad, at, velocity)
    }
    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), Self::CommandError>;
    fn release_live(&mut self, pad: PadId) -> Result<(), Self::CommandError> {
        let at = self.render_horizon().saturating_add(64);
        self.release(pad, at)
    }
    fn trigger_live_tracked(
        &mut self,
        pad: PadId,
        velocity: f32,
    ) -> Result<LiveCommandId, Self::CommandError>;
    fn release_live_tracked(&mut self, pad: PadId) -> Result<LiveCommandId, Self::CommandError>;
    fn install_pattern(
        &mut self,
        snapshot: Arc<PatternSnapshot>,
    ) -> Result<PatternSnapshotSlot, Self::CommandError>;
    fn select_pattern(
        &mut self,
        slot: PatternSlotId,
        switch: PatternSwitch,
    ) -> Result<(), Self::CommandError>;
    fn play_pattern(&mut self) -> Result<(), Self::CommandError>;
    fn stop_pattern(&mut self) -> Result<(), Self::CommandError>;
    fn set_record_capture(
        &mut self,
        capture: Option<(PatternSlotId, u64)>,
    ) -> Result<(), Self::CommandError>;
    fn drain_live_acks(&mut self, output: &mut [LiveAck]) -> usize;
    fn reclaim_retired_pattern(&mut self) -> Option<PatternSnapshotSlot>;
    // The adapter consumes exact tokens above so it can ignore stale retirements.
    #[expect(
        dead_code,
        reason = "the count-only controller boundary is retained for SessionLike parity"
    )]
    fn reclaim_retired_patterns(&mut self) -> usize;
    fn remove_sample(&mut self, pad: PadId) -> Result<(), Self::CommandError>;
    fn stop_pad(&mut self, pad: PadId) -> Result<(), Self::CommandError>;
    fn stop_all(&mut self) -> Result<(), Self::CommandError>;
    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), Self::CommandError>;
    fn update_pad_mix(
        &mut self,
        _pad: PadId,
        _settings: PadMixSettings,
    ) -> Result<(), Self::CommandError> {
        Ok(())
    }
    fn update_master_mix(
        &mut self,
        _settings: MasterMixSettings,
    ) -> Result<(), Self::CommandError> {
        Ok(())
    }
    fn reclaim_retired_slot(&mut self) -> Option<SampleSlot> {
        None
    }
    fn reclaim_retired(&mut self) -> usize;
    fn latest_telemetry(&mut self) -> Option<Telemetry>;
    fn poll_error(&mut self) -> Option<Self::RuntimeError>;
    fn begin_capture(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureSendFailure>;
    fn start_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure>;
    fn stop_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure>;
    fn cancel_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure>;
    fn capture_status(&mut self) -> Option<CaptureStatus>;
    fn capture_completion(&mut self) -> Option<CaptureOutcome>;
}

trait InputSessionLike {
    fn sample_rate(&self) -> u32;
    fn begin_capture(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureSendFailure>;
    fn start_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure>;
    fn stop_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure>;
    fn cancel_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure>;
    fn capture_status(&mut self) -> Option<CaptureStatus>;
    fn capture_progress(&mut self) -> Option<CaptureProgressSnapshot>;
    fn capture_completion(&mut self) -> Option<CaptureOutcome>;
    fn capture_state(&mut self) -> sampler_audio::CaptureState;
    fn poll_error(&mut self) -> Option<String>;
}

impl InputSessionLike for InputCaptureSession {
    fn sample_rate(&self) -> u32 {
        InputCaptureSession::sample_rate(self)
    }

    fn begin_capture(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureSendFailure> {
        self.controller_mut().arm(buffer)
    }

    fn start_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
        self.controller_mut().start(token)
    }

    fn stop_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
        self.controller_mut().stop(token)
    }

    fn cancel_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
        self.controller_mut().cancel(token)
    }

    fn capture_status(&mut self) -> Option<CaptureStatus> {
        None
    }

    fn capture_progress(&mut self) -> Option<CaptureProgressSnapshot> {
        self.controller_mut().progress()
    }

    fn capture_completion(&mut self) -> Option<CaptureOutcome> {
        self.controller_mut().try_next_outcome()
    }

    fn capture_state(&mut self) -> sampler_audio::CaptureState {
        self.controller_mut().state()
    }

    fn poll_error(&mut self) -> Option<String> {
        InputCaptureSession::poll_error(self).map(|error| error.to_string())
    }
}

impl SessionLike for AudioSession {
    type CommandError = ControlError;
    type RuntimeError = DeviceError;

    fn sample_rate(&self) -> u32 {
        AudioSession::sample_rate(self)
    }

    fn channels(&self) -> u16 {
        AudioSession::channels(self)
    }

    fn render_horizon(&mut self) -> Frame {
        self.controller_mut().render_horizon()
    }

    fn install(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: PadMixSettings,
    ) -> Result<SampleSlot, Self::CommandError> {
        self.controller_mut().install(pad, sample, settings, mix)
    }

    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: PadMixSettings,
    ) -> Result<SampleSlot, Self::CommandError> {
        self.controller_mut()
            .install_recovery(pad, sample, settings, mix)
    }

    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), Self::CommandError> {
        self.controller_mut().trigger(pad, at, velocity)
    }

    fn trigger_live(&mut self, pad: PadId, velocity: f32) -> Result<(), Self::CommandError> {
        self.controller_mut().trigger_live(pad, velocity)
    }

    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), Self::CommandError> {
        self.controller_mut().release(pad, at)
    }

    fn release_live(&mut self, pad: PadId) -> Result<(), Self::CommandError> {
        self.controller_mut().release_live(pad)
    }

    fn trigger_live_tracked(
        &mut self,
        pad: PadId,
        velocity: f32,
    ) -> Result<LiveCommandId, Self::CommandError> {
        self.controller_mut().trigger_live_tracked(pad, velocity)
    }

    fn release_live_tracked(&mut self, pad: PadId) -> Result<LiveCommandId, Self::CommandError> {
        self.controller_mut().release_live_tracked(pad)
    }

    fn install_pattern(
        &mut self,
        snapshot: Arc<PatternSnapshot>,
    ) -> Result<PatternSnapshotSlot, Self::CommandError> {
        self.controller_mut().install_pattern(snapshot)
    }

    fn select_pattern(
        &mut self,
        slot: PatternSlotId,
        switch: PatternSwitch,
    ) -> Result<(), Self::CommandError> {
        self.controller_mut().select_pattern(slot, switch)
    }

    fn play_pattern(&mut self) -> Result<(), Self::CommandError> {
        self.controller_mut().play_pattern()
    }

    fn stop_pattern(&mut self) -> Result<(), Self::CommandError> {
        self.controller_mut().stop_pattern()
    }

    fn set_record_capture(
        &mut self,
        capture: Option<(PatternSlotId, u64)>,
    ) -> Result<(), Self::CommandError> {
        self.controller_mut().set_record_capture(capture)
    }

    fn drain_live_acks(&mut self, output: &mut [LiveAck]) -> usize {
        self.controller_mut().drain_live_acks(output)
    }

    fn reclaim_retired_pattern(&mut self) -> Option<PatternSnapshotSlot> {
        self.controller_mut().reclaim_retired_pattern()
    }

    fn reclaim_retired_patterns(&mut self) -> usize {
        let mut reclaimed = 0;
        while self.reclaim_retired_pattern().is_some() {
            reclaimed += 1;
        }
        reclaimed
    }

    fn remove_sample(&mut self, pad: PadId) -> Result<(), Self::CommandError> {
        self.controller_mut().remove_sample(pad)
    }

    fn stop_pad(&mut self, pad: PadId) -> Result<(), Self::CommandError> {
        self.controller_mut().stop_pad(pad)
    }

    fn stop_all(&mut self) -> Result<(), Self::CommandError> {
        self.controller_mut().stop_all()
    }

    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), Self::CommandError> {
        self.controller_mut().update_pad(pad, settings)
    }

    fn update_pad_mix(
        &mut self,
        pad: PadId,
        settings: PadMixSettings,
    ) -> Result<(), Self::CommandError> {
        self.controller_mut().update_pad_mix(pad, settings)
    }

    fn update_master_mix(&mut self, settings: MasterMixSettings) -> Result<(), Self::CommandError> {
        self.controller_mut().update_master_mix(settings)
    }

    fn reclaim_retired(&mut self) -> usize {
        self.controller_mut().reclaim_retired()
    }

    fn reclaim_retired_slot(&mut self) -> Option<SampleSlot> {
        self.controller_mut().reclaim_retired_slot()
    }

    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.controller_mut().latest_telemetry()
    }

    fn poll_error(&mut self) -> Option<Self::RuntimeError> {
        AudioSession::poll_error(self)
    }

    fn begin_capture(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureSendFailure> {
        self.controller_mut().arm_capture(buffer)
    }

    fn start_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
        self.controller_mut().start_capture(token)
    }

    fn stop_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
        self.controller_mut().stop_capture(token)
    }

    fn cancel_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
        self.controller_mut().cancel_capture(token)
    }

    fn capture_status(&mut self) -> Option<CaptureStatus> {
        self.controller_mut().capture_status()
    }

    fn capture_completion(&mut self) -> Option<CaptureOutcome> {
        self.controller_mut().try_capture_completion()
    }
}

pub struct SessionAudioPort<S = AudioSession> {
    session: Option<RefCell<S>>,
    input: Option<Box<dyn InputSessionLike>>,
    input_opener: Box<dyn FnMut() -> Result<Box<dyn InputSessionLike>, String>>,
    active_capture: Option<AdapterCaptureIdentity>,
    retained_samples: [Option<Arc<SampleBuffer>>; SAMPLE_SLOT_COUNT],
    retained_patterns: [Option<Arc<PatternSnapshot>>; PATTERN_SNAPSHOT_SLOT_COUNT],
    retained_pattern_slots: [Option<PatternSnapshotSlot>; PATTERN_SNAPSHOT_SLOT_COUNT],
}

#[derive(Clone, Copy)]
struct AdapterCaptureIdentity {
    token: u64,
    target: PadId,
    source: CaptureSource,
    max_frames: usize,
}

impl<S> SessionAudioPort<S> {
    fn new(session: S) -> Self {
        Self::new_with_input_opener(session, || {
            InputCaptureSession::open_default()
                .map(|session| Box::new(session) as Box<dyn InputSessionLike>)
                .map_err(|error| error.to_string())
        })
    }

    fn new_with_input_opener(
        session: S,
        input_opener: impl FnMut() -> Result<Box<dyn InputSessionLike>, String> + 'static,
    ) -> Self {
        Self {
            session: Some(RefCell::new(session)),
            input: None,
            input_opener: Box::new(input_opener),
            active_capture: None,
            retained_samples: array::from_fn(|_| None),
            retained_patterns: array::from_fn(|_| None),
            retained_pattern_slots: array::from_fn(|_| None),
        }
    }

    fn session(&self) -> &RefCell<S> {
        self.session
            .as_ref()
            .expect("audio session exists until adapter drop")
    }

    fn session_mut(&mut self) -> &mut S {
        self.session
            .as_mut()
            .expect("audio session exists until adapter drop")
            .get_mut()
    }

    fn input_mut(&mut self) -> Result<&mut Box<dyn InputSessionLike>, CaptureError> {
        if self.input.is_none() {
            self.input = Some((self.input_opener)().map_err(CaptureError::InputOpen)?);
        }
        Ok(self.input.as_mut().expect("input capture was just opened"))
    }
}

impl<S> Drop for SessionAudioPort<S> {
    fn drop(&mut self) {
        drop(self.input.take());
        drop(self.session.take());
        self.retained_samples.fill(None);
        self.retained_patterns.fill(None);
        self.retained_pattern_slots.fill(None);
    }
}

impl<S> AudioPort for SessionAudioPort<S>
where
    S: SessionLike,
{
    fn sample_rate(&self) -> u32 {
        self.session().borrow().sample_rate()
    }

    fn channels(&self) -> u16 {
        self.session().borrow().channels()
    }

    fn render_horizon(&self) -> Frame {
        self.session().borrow_mut().render_horizon()
    }

    fn install(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: PadMixSettings,
    ) -> Result<SampleSlot, String> {
        let retained = Arc::clone(&sample);
        let slot = self
            .session_mut()
            .install(pad, sample, settings, mix)
            .map_err(|error| error.to_string())?;
        self.retained_samples[slot.index()] = Some(retained);
        Ok(slot)
    }

    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
        mix: PadMixSettings,
    ) -> Result<SampleSlot, String> {
        let retained = Arc::clone(&sample);
        let slot = self
            .session_mut()
            .install_recovery(pad, sample, settings, mix)
            .map_err(|error| error.to_string())?;
        self.retained_samples[slot.index()] = Some(retained);
        Ok(slot)
    }

    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String> {
        self.session_mut()
            .trigger(pad, at, velocity)
            .map_err(|error| error.to_string())
    }

    fn trigger_live(&mut self, pad: PadId, velocity: f32) -> Result<(), String> {
        self.session_mut()
            .trigger_live(pad, velocity)
            .map_err(|error| error.to_string())
    }

    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String> {
        self.session_mut()
            .release(pad, at)
            .map_err(|error| error.to_string())
    }

    fn release_live(&mut self, pad: PadId) -> Result<(), String> {
        self.session_mut()
            .release_live(pad)
            .map_err(|error| error.to_string())
    }

    fn trigger_live_tracked(&mut self, pad: PadId, velocity: f32) -> Result<LiveCommandId, String> {
        self.session_mut()
            .trigger_live_tracked(pad, velocity)
            .map_err(|error| error.to_string())
    }

    fn release_live_tracked(&mut self, pad: PadId) -> Result<LiveCommandId, String> {
        self.session_mut()
            .release_live_tracked(pad)
            .map_err(|error| error.to_string())
    }

    fn install_pattern(
        &mut self,
        snapshot: Arc<PatternSnapshot>,
    ) -> Result<PatternSnapshotSlot, String> {
        let retained = Arc::clone(&snapshot);
        let owner_slot = self
            .session_mut()
            .install_pattern(snapshot)
            .map_err(|error| error.to_string())?;
        self.retained_patterns[owner_slot.index()] = Some(retained);
        self.retained_pattern_slots[owner_slot.index()] = Some(owner_slot);
        Ok(owner_slot)
    }

    fn select_pattern(&mut self, slot: PatternSlotId, switch: PatternSwitch) -> Result<(), String> {
        self.session_mut()
            .select_pattern(slot, switch)
            .map_err(|error| error.to_string())
    }

    fn play_pattern(&mut self) -> Result<(), String> {
        self.session_mut()
            .play_pattern()
            .map_err(|error| error.to_string())
    }

    fn stop_pattern(&mut self) -> Result<(), String> {
        self.session_mut()
            .stop_pattern()
            .map_err(|error| error.to_string())
    }

    fn set_record_capture(&mut self, capture: Option<(PatternSlotId, u64)>) -> Result<(), String> {
        self.session_mut()
            .set_record_capture(capture)
            .map_err(|error| error.to_string())
    }

    fn drain_live_acks(&mut self, output: &mut [LiveAck]) -> usize {
        self.session_mut().drain_live_acks(output)
    }

    fn reclaim_retired_patterns(&mut self) -> usize {
        let mut reclaimed = 0;
        while let Some(owner_slot) = self.session_mut().reclaim_retired_pattern() {
            let index = owner_slot.index();
            if self.retained_pattern_slots[index] == Some(owner_slot) {
                self.retained_patterns[index] = None;
                self.retained_pattern_slots[index] = None;
                reclaimed += 1;
            }
        }
        reclaimed
    }

    fn remove_sample(&mut self, pad: PadId) -> Result<(), String> {
        self.session_mut()
            .remove_sample(pad)
            .map_err(|error| error.to_string())
    }

    fn stop_pad(&mut self, pad: PadId) -> Result<(), String> {
        self.session_mut()
            .stop_pad(pad)
            .map_err(|error| error.to_string())
    }

    fn stop_all(&mut self) -> Result<(), String> {
        self.session_mut()
            .stop_all()
            .map_err(|error| error.to_string())
    }

    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String> {
        self.session_mut()
            .update_pad(pad, settings)
            .map_err(|error| error.to_string())
    }

    fn update_pad_mix(&mut self, pad: PadId, settings: PadMixSettings) -> Result<(), String> {
        self.session_mut()
            .update_pad_mix(pad, settings)
            .map_err(|error| error.to_string())
    }

    fn update_master_mix(&mut self, settings: MasterMixSettings) -> Result<(), String> {
        self.session_mut()
            .update_master_mix(settings)
            .map_err(|error| error.to_string())
    }

    fn reclaim_retired(&mut self) -> usize {
        let mut reclaimed = 0;
        while let Some(slot) = self.session_mut().reclaim_retired_slot() {
            self.retained_samples[slot.index()] = None;
            reclaimed += 1;
        }
        if reclaimed == 0 {
            self.session_mut().reclaim_retired()
        } else {
            reclaimed
        }
    }

    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.session_mut().latest_telemetry()
    }

    fn poll_runtime_error(&mut self) -> Option<String> {
        self.session_mut()
            .poll_error()
            .map(|error| error.to_string())
    }

    fn capture_support(&self) -> CaptureSupport {
        CaptureSupport::Available
    }

    fn capture_source_rate(&mut self, source: CaptureSource) -> Result<u32, CaptureError> {
        match source {
            CaptureSource::Resample => Ok(self.session_mut().sample_rate()),
            CaptureSource::Input => Ok(self.input_mut()?.sample_rate()),
        }
    }

    fn begin_capture(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureCommandFailure> {
        let source = buffer.source();
        let identity = AdapterCaptureIdentity {
            token: buffer.token(),
            target: buffer.target(),
            source,
            max_frames: buffer.max_frames(),
        };
        if buffer.token() == 0 {
            return Err(CaptureCommandFailure::rejected(
                CaptureError::ZeroCommandToken,
                CaptureCommand::Arm(buffer),
            ));
        }
        if let Some(active) = self.active_capture {
            return Err(CaptureCommandFailure::rejected(
                CaptureError::ActiveCapture {
                    source: active.source,
                    token: active.token,
                },
                CaptureCommand::Arm(buffer),
            ));
        }
        let expected_rate = match self.capture_source_rate(source) {
            Ok(sample_rate) => sample_rate,
            Err(error) => {
                return Err(CaptureCommandFailure::rejected(
                    error,
                    CaptureCommand::Arm(buffer),
                ));
            }
        };
        if buffer.sample_rate() != expected_rate {
            return Err(CaptureCommandFailure::rejected(
                CaptureError::CommandRateMismatch {
                    expected: expected_rate,
                    received: buffer.sample_rate(),
                },
                CaptureCommand::Arm(buffer),
            ));
        }
        let result = match source {
            CaptureSource::Resample => self.session_mut().begin_capture(buffer),
            CaptureSource::Input => match self.input_mut() {
                Ok(input) => input.begin_capture(buffer),
                Err(error) => {
                    return Err(CaptureCommandFailure::rejected(
                        error,
                        CaptureCommand::Arm(buffer),
                    ));
                }
            },
        };
        result.map_err(CaptureCommandFailure::from_send)?;
        self.active_capture = Some(identity);
        Ok(())
    }

    fn start_capture(
        &mut self,
        source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        self.require_active_identity(source, token, CaptureCommand::Start { token })?;
        match source {
            CaptureSource::Resample => self.session_mut().start_capture(token),
            CaptureSource::Input => self
                .input_mut()
                .map_err(|error| {
                    CaptureCommandFailure::rejected(error, CaptureCommand::Start { token })
                })?
                .start_capture(token),
        }
        .map_err(CaptureCommandFailure::from_send)
    }

    fn stop_capture(
        &mut self,
        source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        self.require_active_identity(source, token, CaptureCommand::Stop { token })?;
        match source {
            CaptureSource::Resample => self.session_mut().stop_capture(token),
            CaptureSource::Input => self
                .input_mut()
                .map_err(|error| {
                    CaptureCommandFailure::rejected(error, CaptureCommand::Stop { token })
                })?
                .stop_capture(token),
        }
        .map_err(CaptureCommandFailure::from_send)
    }

    fn cancel_capture(
        &mut self,
        source: CaptureSource,
        token: u64,
    ) -> Result<(), CaptureCommandFailure> {
        self.require_active_identity(source, token, CaptureCommand::Cancel { token })?;
        match source {
            CaptureSource::Resample => self.session_mut().cancel_capture(token),
            CaptureSource::Input => self
                .input_mut()
                .map_err(|error| {
                    CaptureCommandFailure::rejected(error, CaptureCommand::Cancel { token })
                })?
                .cancel_capture(token),
        }
        .map_err(CaptureCommandFailure::from_send)
    }

    fn capture_status(&mut self, source: CaptureSource) -> Option<CaptureStatus> {
        match source {
            CaptureSource::Resample => self.session_mut().capture_status(),
            CaptureSource::Input => {
                if let Some(status) = self.input.as_mut()?.capture_status() {
                    return Some(status);
                }
                let identity = self
                    .active_capture
                    .filter(|identity| identity.source == CaptureSource::Input)?;
                let input = self.input.as_mut()?;
                let state = input.capture_state();
                let progress = input.capture_progress()?;
                if progress.token != identity.token {
                    return None;
                }
                Some(CaptureStatus {
                    token: identity.token,
                    source: identity.source,
                    target: identity.target,
                    state,
                    frames: progress.frames,
                    max_frames: identity.max_frames,
                    peak: progress.peak,
                    hard_limit: progress.hard_limit,
                })
            }
        }
    }

    fn capture_completion(&mut self, source: CaptureSource) -> Option<CaptureOutcome> {
        let outcome = match source {
            CaptureSource::Resample => self.session_mut().capture_completion(),
            CaptureSource::Input => self.input.as_mut()?.capture_completion(),
        };
        if outcome.is_some()
            && self
                .active_capture
                .is_some_and(|identity| identity.source == source)
        {
            self.active_capture = None;
        }
        outcome
    }

    fn capture_runtime_error(&mut self, source: CaptureSource) -> Option<CaptureError> {
        match source {
            CaptureSource::Resample => self
                .session_mut()
                .poll_error()
                .map(|error| CaptureError::OutputRuntime(error.to_string())),
            CaptureSource::Input => self
                .input
                .as_mut()?
                .poll_error()
                .map(CaptureError::InputRuntime),
        }
    }
}

impl<S> SessionAudioPort<S> {
    fn require_active_identity(
        &self,
        source: CaptureSource,
        token: u64,
        command: CaptureCommand,
    ) -> Result<(), CaptureCommandFailure> {
        match self.active_capture {
            None => Err(CaptureCommandFailure::rejected(
                CaptureError::NoActiveCapture,
                command,
            )),
            Some(active) if active.source != source => Err(CaptureCommandFailure::rejected(
                CaptureError::CommandSourceMismatch {
                    expected: active.source,
                    received: source,
                },
                command,
            )),
            Some(active) if active.token != token => Err(CaptureCommandFailure::rejected(
                CaptureError::CommandTokenMismatch {
                    expected: active.token,
                    received: token,
                },
                command,
            )),
            Some(_) => Ok(()),
        }
    }
}

pub fn open_default_audio() -> Result<Box<dyn AudioPort>, String> {
    AudioSession::open_default()
        .map(SessionAudioPort::new)
        .map(|port| Box::new(port) as Box<dyn AudioPort>)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
    use std::sync::{Arc, Weak};
    use std::thread::ThreadId;

    use sampler_audio::{
        CaptureBuffer, CaptureCommand, CaptureController, CaptureCore,
        CaptureError as CoreCaptureError, CaptureOutcome, CaptureProgressSnapshot,
        CaptureSendFailure, CaptureSource, CaptureState, CaptureStatus, ControlError, LiveAck,
        LiveCommandId, PatternRetirement, PatternSnapshotSlot, PatternSwitch, SampleBuffer,
        SampleSlot, Telemetry, audio_channels, capture_channels,
    };
    use sampler_core::{
        EditablePattern, Meter, PadId, PadSettings, PatternSlotId, PatternSnapshot, Resolution,
        Tempo, Transport,
    };

    use super::{
        AudioPort, CaptureCommandFailure, CaptureMaintenance, CaptureSupport, InputSessionLike,
        SessionAudioPort, SessionLike,
    };
    use crate::CaptureError;

    struct CaptureTrackingAllocator;

    #[global_allocator]
    static CAPTURE_TRACKING_ALLOCATOR: CaptureTrackingAllocator = CaptureTrackingAllocator;
    static TRACKED_ALLOCATION: AtomicUsize = AtomicUsize::new(0);
    static OWNERSHIP_EVENTS: AtomicPtr<CaptureOwnershipEvents> =
        AtomicPtr::new(std::ptr::null_mut());

    thread_local! {
        static IS_APP_OWNER_THREAD: Cell<bool> = const { Cell::new(false) };
    }

    // SAFETY: Every operation delegates to `System` with the unchanged pointer/layout contract.
    unsafe impl GlobalAlloc for CaptureTrackingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: The caller provides the allocation layout required by `GlobalAlloc`.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            if TRACKED_ALLOCATION
                .compare_exchange(pointer as usize, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let events = OWNERSHIP_EVENTS.load(Ordering::Acquire);
                if !events.is_null() {
                    // SAFETY: The test retains the `Arc<CaptureOwnershipEvents>` until after the
                    // tracked allocation is deallocated and clears this pointer before Arc drop.
                    unsafe { &*events }.record(&unsafe { &*events }.allocation);
                }
            }
            // SAFETY: The pointer/layout pair came from the delegated `System` allocator.
            unsafe { System.dealloc(pointer, layout) }
        }
    }

    struct CaptureOwnershipEvents {
        next: AtomicUsize,
        stream_core: AtomicUsize,
        allocation: AtomicUsize,
        controller: AtomicUsize,
        output: AtomicUsize,
        off_app_thread: AtomicBool,
    }

    impl CaptureOwnershipEvents {
        fn new() -> Self {
            Self {
                next: AtomicUsize::new(1),
                stream_core: AtomicUsize::new(0),
                allocation: AtomicUsize::new(0),
                controller: AtomicUsize::new(0),
                output: AtomicUsize::new(0),
                off_app_thread: AtomicBool::new(false),
            }
        }

        fn record(&self, slot: &AtomicUsize) {
            if !IS_APP_OWNER_THREAD.with(Cell::get) {
                self.off_app_thread.store(true, Ordering::Release);
            }
            slot.store(self.next.fetch_add(1, Ordering::AcqRel), Ordering::Release);
        }
    }

    struct TrackedCaptureCore {
        inner: CaptureCore,
        _completion: Option<OwnershipCompletionProbe>,
    }

    struct TrackedCaptureController {
        inner: CaptureController,
        _completion: Option<OwnershipCompletionProbe>,
    }

    enum OwnershipCompletionKind {
        StreamCore,
        Controller,
        Output,
    }

    struct OwnershipCompletionProbe {
        events: Arc<CaptureOwnershipEvents>,
        kind: OwnershipCompletionKind,
    }

    impl Drop for OwnershipCompletionProbe {
        fn drop(&mut self) {
            match self.kind {
                OwnershipCompletionKind::StreamCore => {
                    self.events.record(&self.events.stream_core);
                }
                OwnershipCompletionKind::Controller => {
                    self.events.record(&self.events.controller);
                }
                OwnershipCompletionKind::Output => {
                    self.events.record(&self.events.output);
                }
            }
        }
    }

    struct FakeSession {
        sample_rate: u32,
        channels: u16,
        horizon: u64,
        trigger_error: Option<ControlError>,
        telemetry: VecDeque<Telemetry>,
        retired: usize,
        retired_slots: VecDeque<SampleSlot>,
        ownership: Option<OwnershipProbe>,
        pattern_calls: Vec<PatternCall>,
        snapshot_ownership: Option<SnapshotOwnershipProbe>,
        next_pattern_slot: Option<PatternSnapshotSlot>,
        retired_pattern_slots: VecDeque<PatternSnapshotSlot>,
        removed_pads: Vec<PadId>,
        capture: Option<CaptureController>,
        capture_polls: Rc<Cell<usize>>,
        runtime_errors: VecDeque<ControlError>,
        capture_output_completion: Option<OwnershipCompletionProbe>,
    }

    #[derive(Clone)]
    struct OwnershipProbe {
        installed: Rc<RefCell<Vec<Weak<SampleBuffer>>>>,
        session_drop: Rc<RefCell<Vec<(ThreadId, bool)>>>,
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum PatternCall {
        TrackedTrigger(LiveCommandId, PadId),
        Select(PatternSlotId, PatternSwitch),
        Play,
    }

    #[derive(Clone)]
    struct SnapshotOwnershipProbe {
        installed: Rc<RefCell<Vec<Weak<PatternSnapshot>>>>,
        session_drop: Rc<RefCell<Vec<(ThreadId, bool)>>>,
    }

    impl FakeSession {
        fn ready(sample_rate: u32, channels: u16) -> Self {
            Self {
                sample_rate,
                channels,
                horizon: 0,
                trigger_error: None,
                telemetry: VecDeque::new(),
                retired: 0,
                retired_slots: VecDeque::new(),
                ownership: None,
                pattern_calls: Vec::new(),
                snapshot_ownership: None,
                next_pattern_slot: None,
                retired_pattern_slots: VecDeque::new(),
                removed_pads: Vec::new(),
                capture: None,
                capture_polls: Rc::new(Cell::new(0)),
                runtime_errors: VecDeque::new(),
                capture_output_completion: None,
            }
        }

        fn with_horizon(mut self, horizon: u64) -> Self {
            self.horizon = horizon;
            self
        }

        fn queue_full_on_trigger(mut self) -> Self {
            self.trigger_error = Some(ControlError::CommandQueueFull);
            self
        }

        fn with_telemetry(telemetry: impl IntoIterator<Item = Telemetry>) -> Self {
            let mut session = Self::ready(48_000, 2);
            session.telemetry.extend(telemetry);
            session
        }

        fn with_retired(mut self, retired: usize) -> Self {
            self.retired = retired;
            self
        }

        fn with_retired_slot(mut self, slot: SampleSlot) -> Self {
            self.retired_slots.push_back(slot);
            self
        }

        fn with_ownership_probe(mut self, ownership: OwnershipProbe) -> Self {
            self.ownership = Some(ownership);
            self
        }

        fn with_snapshot_ownership_probe(mut self, ownership: SnapshotOwnershipProbe) -> Self {
            self.snapshot_ownership = Some(ownership);
            self
        }

        fn with_pattern_snapshot_slot(mut self, slot: PatternSnapshotSlot) -> Self {
            self.next_pattern_slot = Some(slot);
            self
        }

        fn with_retired_pattern_slot(mut self, slot: PatternSnapshotSlot) -> Self {
            self.retired_pattern_slots.push_back(slot);
            self
        }

        fn pattern_calls(&self) -> &[PatternCall] {
            &self.pattern_calls
        }

        fn capture_ready(sample_rate: u32) -> (Self, CaptureCore, Rc<Cell<usize>>) {
            let (controller, core) = capture_channels(4, 1);
            let mut session = Self::ready(sample_rate, 2);
            session.capture = Some(controller);
            let polls = Rc::clone(&session.capture_polls);
            (session, core, polls)
        }

        fn with_runtime_error(mut self, error: ControlError) -> Self {
            self.runtime_errors.push_back(error);
            self
        }

        fn with_capture_ownership_events(mut self, events: Arc<CaptureOwnershipEvents>) -> Self {
            self.capture_output_completion = Some(OwnershipCompletionProbe {
                events,
                kind: OwnershipCompletionKind::Output,
            });
            self
        }
    }

    impl Drop for FakeSession {
        fn drop(&mut self) {
            if let Some(ownership) = &self.ownership {
                let owners_alive = ownership
                    .installed
                    .borrow()
                    .iter()
                    .all(|sample| sample.upgrade().is_some());
                ownership
                    .session_drop
                    .borrow_mut()
                    .push((std::thread::current().id(), owners_alive));
            }
            if let Some(ownership) = &self.snapshot_ownership {
                let owners_alive = ownership
                    .installed
                    .borrow()
                    .iter()
                    .all(|snapshot| snapshot.upgrade().is_some());
                ownership
                    .session_drop
                    .borrow_mut()
                    .push((std::thread::current().id(), owners_alive));
            }
        }
    }

    impl SessionLike for FakeSession {
        type CommandError = ControlError;
        type RuntimeError = ControlError;

        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn channels(&self) -> u16 {
            self.channels
        }

        fn render_horizon(&mut self) -> u64 {
            self.horizon
        }

        fn install(
            &mut self,
            _pad: PadId,
            sample: Arc<SampleBuffer>,
            _settings: PadSettings,
            _mix: sampler_core::PadMixSettings,
        ) -> Result<SampleSlot, Self::CommandError> {
            if let Some(ownership) = &self.ownership {
                ownership
                    .installed
                    .borrow_mut()
                    .push(Arc::downgrade(&sample));
            }
            Ok(SampleSlot::new(0).unwrap())
        }

        fn trigger(
            &mut self,
            _pad: PadId,
            _at: u64,
            _velocity: f32,
        ) -> Result<(), Self::CommandError> {
            if let Some(error) = self.trigger_error {
                return Err(error);
            }
            Ok(())
        }

        fn release(&mut self, _pad: PadId, _at: u64) -> Result<(), Self::CommandError> {
            Ok(())
        }

        fn trigger_live_tracked(
            &mut self,
            pad: PadId,
            _velocity: f32,
        ) -> Result<LiveCommandId, Self::CommandError> {
            let id = LiveCommandId::FIRST;
            self.pattern_calls
                .push(PatternCall::TrackedTrigger(id, pad));
            Ok(id)
        }

        fn release_live_tracked(
            &mut self,
            _pad: PadId,
        ) -> Result<LiveCommandId, Self::CommandError> {
            Ok(LiveCommandId::FIRST)
        }

        fn install_pattern(
            &mut self,
            snapshot: Arc<PatternSnapshot>,
        ) -> Result<PatternSnapshotSlot, Self::CommandError> {
            if let Some(ownership) = &self.snapshot_ownership {
                ownership
                    .installed
                    .borrow_mut()
                    .push(Arc::downgrade(&snapshot));
            }
            Ok(self
                .next_pattern_slot
                .take()
                .expect("test session needs an exact pattern snapshot slot"))
        }

        fn select_pattern(
            &mut self,
            slot: PatternSlotId,
            switch: PatternSwitch,
        ) -> Result<(), Self::CommandError> {
            self.pattern_calls.push(PatternCall::Select(slot, switch));
            Ok(())
        }

        fn play_pattern(&mut self) -> Result<(), Self::CommandError> {
            self.pattern_calls.push(PatternCall::Play);
            Ok(())
        }

        fn stop_pattern(&mut self) -> Result<(), Self::CommandError> {
            Ok(())
        }

        fn set_record_capture(
            &mut self,
            _capture: Option<(PatternSlotId, u64)>,
        ) -> Result<(), Self::CommandError> {
            Ok(())
        }

        fn drain_live_acks(&mut self, _output: &mut [LiveAck]) -> usize {
            0
        }

        fn reclaim_retired_patterns(&mut self) -> usize {
            0
        }

        fn reclaim_retired_pattern(&mut self) -> Option<PatternSnapshotSlot> {
            self.retired_pattern_slots.pop_front()
        }

        fn remove_sample(&mut self, pad: PadId) -> Result<(), Self::CommandError> {
            self.removed_pads.push(pad);
            Ok(())
        }

        fn stop_pad(&mut self, _pad: PadId) -> Result<(), Self::CommandError> {
            Ok(())
        }

        fn stop_all(&mut self) -> Result<(), Self::CommandError> {
            Ok(())
        }

        fn update_pad(
            &mut self,
            _pad: PadId,
            _settings: PadSettings,
        ) -> Result<(), Self::CommandError> {
            Ok(())
        }

        fn reclaim_retired(&mut self) -> usize {
            self.retired
        }

        fn reclaim_retired_slot(&mut self) -> Option<SampleSlot> {
            self.retired_slots.pop_front()
        }

        fn latest_telemetry(&mut self) -> Option<Telemetry> {
            let mut latest = None;
            while let Some(next) = self.telemetry.pop_front() {
                latest = Some(next);
            }
            latest
        }

        fn poll_error(&mut self) -> Option<Self::RuntimeError> {
            self.runtime_errors.pop_front()
        }

        fn begin_capture(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureSendFailure> {
            self.capture.as_mut().unwrap().arm(buffer)
        }

        fn start_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
            self.capture.as_mut().unwrap().start(token)
        }

        fn stop_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
            self.capture.as_mut().unwrap().stop(token)
        }

        fn cancel_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
            self.capture.as_mut().unwrap().cancel(token)
        }

        fn capture_status(&mut self) -> Option<CaptureStatus> {
            None
        }

        fn capture_completion(&mut self) -> Option<CaptureOutcome> {
            self.capture_polls.set(self.capture_polls.get() + 1);
            self.capture.as_mut().unwrap().try_next_outcome()
        }
    }

    struct FakeInputSession {
        // Field order is the production input-session contract: stream/core is destroyed before
        // the controller that can still refer to its callback-owned ring state.
        core: TrackedCaptureCore,
        controller: TrackedCaptureController,
        sample_rate: u32,
        polls: Rc<Cell<usize>>,
        errors: VecDeque<ControlError>,
        poll_callback_on_begin: Rc<Cell<bool>>,
        poll_callback_on_status: Rc<Cell<bool>>,
    }

    impl InputSessionLike for FakeInputSession {
        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn begin_capture(&mut self, buffer: CaptureBuffer) -> Result<(), CaptureSendFailure> {
            self.controller.inner.arm(buffer)?;
            if self.poll_callback_on_begin.get() {
                self.core.inner.poll_commands();
            }
            Ok(())
        }

        fn start_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
            self.controller.inner.start(token)?;
            self.core.inner.poll_commands();
            self.core.inner.push_frame([0.5, -0.5]);
            Ok(())
        }

        fn stop_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
            self.controller.inner.stop(token)?;
            self.core.inner.poll_commands();
            Ok(())
        }

        fn cancel_capture(&mut self, token: u64) -> Result<(), CaptureSendFailure> {
            self.controller.inner.cancel(token)?;
            self.core.inner.poll_commands();
            Ok(())
        }

        fn capture_status(&mut self) -> Option<CaptureStatus> {
            None
        }

        fn capture_progress(&mut self) -> Option<CaptureProgressSnapshot> {
            self.controller.inner.progress()
        }

        fn capture_completion(&mut self) -> Option<CaptureOutcome> {
            self.polls.set(self.polls.get() + 1);
            self.controller.inner.try_next_outcome()
        }

        fn capture_state(&mut self) -> CaptureState {
            if self.poll_callback_on_status.get() {
                self.core.inner.poll_commands();
            }
            self.controller.inner.state()
        }

        fn poll_error(&mut self) -> Option<String> {
            self.errors.pop_front().map(|error| error.to_string())
        }
    }

    fn fake_input_session(
        controller: CaptureController,
        core: CaptureCore,
        events: Option<Arc<CaptureOwnershipEvents>>,
    ) -> FakeInputSession {
        FakeInputSession {
            core: TrackedCaptureCore {
                inner: core,
                _completion: events.as_ref().map(|events| OwnershipCompletionProbe {
                    events: Arc::clone(events),
                    kind: OwnershipCompletionKind::StreamCore,
                }),
            },
            controller: TrackedCaptureController {
                inner: controller,
                _completion: events.map(|events| OwnershipCompletionProbe {
                    events,
                    kind: OwnershipCompletionKind::Controller,
                }),
            },
            sample_rate: 44_100,
            polls: Rc::new(Cell::new(0)),
            errors: VecDeque::new(),
            poll_callback_on_begin: Rc::new(Cell::new(true)),
            poll_callback_on_status: Rc::new(Cell::new(false)),
        }
    }

    fn telemetry(rendered_frame: u64) -> Telemetry {
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
            pattern_slot: None,
            pattern_generation: None,
            pattern_playing: false,
            pattern_recording: false,
            pattern_origin: None,
            pattern_playhead: 0,
            pattern_loop_count: 0,
            pattern_overflows: 0,
            live_ack_overflows: 0,
        }
    }

    fn pattern_slot(slot: u8) -> PatternSlotId {
        PatternSlotId::new(slot).unwrap()
    }

    fn snapshot() -> Arc<PatternSnapshot> {
        let transport = Transport::new(
            48_000,
            Tempo::new(120.0).unwrap(),
            Meter::new(4, 4).unwrap(),
            1,
            Resolution::Sixteenth,
        )
        .unwrap();
        Arc::new(
            EditablePattern::new(pattern_slot(0), "Pattern", transport)
                .unwrap()
                .compile()
                .unwrap(),
        )
    }

    fn pattern_snapshot_slot() -> PatternSnapshotSlot {
        let (mut controller, _ports) = audio_channels();
        controller.install_pattern(snapshot()).unwrap()
    }

    fn reused_pattern_snapshot_slots() -> (PatternSnapshotSlot, PatternSnapshotSlot) {
        let (mut controller, mut ports) = audio_channels();
        let first_snapshot = snapshot();
        let first = controller
            .install_pattern(Arc::clone(&first_snapshot))
            .unwrap();
        drop(ports.immediate_commands.pop().unwrap());
        ports
            .pattern_retirements
            .push(PatternRetirement::new(first, first_snapshot))
            .unwrap();
        assert_eq!(controller.reclaim_retired_pattern(), Some(first));
        let second = controller.install_pattern(snapshot()).unwrap();
        (first, second)
    }

    #[test]
    fn tracked_live_and_pattern_methods_delegate_without_horizon_math() {
        let session = FakeSession::ready(48_000, 2);
        let mut port = SessionAudioPort::new(session);

        let id = port.trigger_live_tracked(PadId::first(), 1.0).unwrap();
        port.select_pattern(pattern_slot(3), PatternSwitch::NextBoundary)
            .unwrap();
        port.play_pattern().unwrap();

        assert_eq!(
            port.session().borrow().pattern_calls(),
            [
                PatternCall::TrackedTrigger(id, PadId::first()),
                PatternCall::Select(pattern_slot(3), PatternSwitch::NextBoundary),
                PatternCall::Play,
            ]
        );
    }

    #[test]
    fn retained_snapshot_outlives_session_and_drops_on_adapter_thread() {
        let installed = Rc::new(RefCell::new(Vec::new()));
        let session_drop = Rc::new(RefCell::new(Vec::new()));
        let probe = SnapshotOwnershipProbe {
            installed: Rc::clone(&installed),
            session_drop: Rc::clone(&session_drop),
        };
        let session = FakeSession::ready(48_000, 2)
            .with_snapshot_ownership_probe(probe)
            .with_pattern_snapshot_slot(pattern_snapshot_slot());
        let mut port = SessionAudioPort::new(session);
        let snapshot = snapshot();
        let weak = Arc::downgrade(&snapshot);

        port.install_pattern(Arc::clone(&snapshot)).unwrap();
        drop(snapshot);
        assert!(weak.upgrade().is_some());

        let ui_thread = std::thread::current().id();
        drop(port);

        assert_eq!(*session_drop.borrow(), [(ui_thread, true)]);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn stale_pattern_retirement_does_not_clear_a_reused_owner_slot() {
        let (stale, current) = reused_pattern_snapshot_slots();
        assert_eq!(stale.index(), current.index());
        assert_ne!(stale, current);
        let session = FakeSession::ready(48_000, 2)
            .with_pattern_snapshot_slot(current)
            .with_retired_pattern_slot(stale);
        let mut port = SessionAudioPort::new(session);
        let snapshot = snapshot();
        let weak = Arc::downgrade(&snapshot);

        port.install_pattern(Arc::clone(&snapshot)).unwrap();
        drop(snapshot);
        assert_eq!(port.reclaim_retired_patterns(), 0);
        assert!(port.session().borrow().retired_pattern_slots.is_empty());
        assert!(weak.upgrade().is_some());
    }

    #[test]
    fn session_adapter_uses_controller_horizon_and_maps_typed_errors() {
        let session = FakeSession::ready(48_000, 2)
            .with_horizon(2_000)
            .queue_full_on_trigger();
        let mut port = SessionAudioPort::new(session);
        assert_eq!(port.render_horizon(), 2_000);
        assert_eq!(
            port.trigger(PadId::first(), 2_064, 1.0),
            Err("audio command queue is full".into())
        );
    }

    #[test]
    fn maintenance_reclaims_and_drains_to_latest_telemetry() {
        let session = FakeSession::with_telemetry([telemetry(10), telemetry(20)]).with_retired(3);
        let mut port = SessionAudioPort::new(session);
        assert_eq!(port.reclaim_retired(), 3);
        assert_eq!(port.latest_telemetry().unwrap().rendered_frame, 20);
    }

    #[test]
    fn adapter_sample_owner_outlives_session_teardown_on_the_ui_thread() {
        let installed = Rc::new(RefCell::new(Vec::new()));
        let session_drop = Rc::new(RefCell::new(Vec::new()));
        let probe = OwnershipProbe {
            installed: Rc::clone(&installed),
            session_drop: Rc::clone(&session_drop),
        };
        let session = FakeSession::ready(48_000, 2).with_ownership_probe(probe);
        let mut port = SessionAudioPort::new(session);
        let sample = Arc::new(SampleBuffer::new(48_000, vec![0.25, 0.25]).unwrap());
        let weak = Arc::downgrade(&sample);
        port.install(
            PadId::first(),
            Arc::clone(&sample),
            PadSettings::default(),
            sampler_core::PadMixSettings::default(),
        )
        .unwrap();
        drop(sample);

        assert!(weak.upgrade().is_some());
        let ui_thread = std::thread::current().id();
        drop(port);

        assert_eq!(*session_drop.borrow(), [(ui_thread, true)]);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn adapter_retention_is_replaced_per_reused_sample_slot() {
        let session = FakeSession::ready(48_000, 2);
        let mut port = SessionAudioPort::new(session);
        let first = Arc::new(SampleBuffer::new(48_000, vec![0.1, 0.1]).unwrap());
        let first_weak = Arc::downgrade(&first);
        port.install(
            PadId::first(),
            Arc::clone(&first),
            PadSettings::default(),
            sampler_core::PadMixSettings::default(),
        )
        .unwrap();
        drop(first);
        assert!(first_weak.upgrade().is_some());

        let second = Arc::new(SampleBuffer::new(48_000, vec![0.2, 0.2]).unwrap());
        let second_weak = Arc::downgrade(&second);
        port.install(
            PadId::first(),
            Arc::clone(&second),
            PadSettings::default(),
            sampler_core::PadMixSettings::default(),
        )
        .unwrap();
        drop(second);

        assert!(first_weak.upgrade().is_none());
        assert!(second_weak.upgrade().is_some());
    }

    #[test]
    fn adapter_releases_a_retired_slot_owner_during_ui_maintenance() {
        let installed = Rc::new(RefCell::new(Vec::new()));
        let session_drop = Rc::new(RefCell::new(Vec::new()));
        let probe = OwnershipProbe {
            installed: Rc::clone(&installed),
            session_drop,
        };
        let slot = SampleSlot::new(0).unwrap();
        let session = FakeSession::ready(48_000, 2)
            .with_ownership_probe(probe)
            .with_retired_slot(slot);
        let mut port = SessionAudioPort::new(session);
        let sample = Arc::new(SampleBuffer::new(48_000, vec![0.25, 0.25]).unwrap());
        let weak = Arc::downgrade(&sample);
        port.install(
            PadId::first(),
            Arc::clone(&sample),
            PadSettings::default(),
            sampler_core::PadMixSettings::default(),
        )
        .unwrap();
        drop(sample);
        assert!(weak.upgrade().is_some());

        port.remove_sample(PadId::first()).unwrap();
        assert_eq!(port.session().borrow().removed_pads, vec![PadId::first()]);
        assert!(weak.upgrade().is_some());

        assert_eq!(port.reclaim_retired(), 1);

        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn input_capture_is_lazy_reused_and_exactly_routed() {
        let (output, output_core, _) = FakeSession::capture_ready(48_000);
        let (input_controller, input_core) = capture_channels(4, 1);
        let input_polls = Rc::new(Cell::new(0));
        let opens = Rc::new(Cell::new(0));
        let mut opened_input = fake_input_session(input_controller, input_core, None);
        opened_input.polls = Rc::clone(&input_polls);
        let mut input = Some(opened_input);
        let open_count = Rc::clone(&opens);
        let mut port = SessionAudioPort::new_with_input_opener(output, move || {
            open_count.set(open_count.get() + 1);
            Ok(Box::new(input.take().unwrap()) as Box<dyn InputSessionLike>)
        });

        assert_eq!(opens.get(), 0);
        assert_eq!(
            port.capture_source_rate(CaptureSource::Input).unwrap(),
            44_100
        );
        assert_eq!(
            port.capture_source_rate(CaptureSource::Input).unwrap(),
            44_100
        );
        assert_eq!(opens.get(), 1);

        port.begin_capture(
            CaptureBuffer::try_new(1, PadId::first(), CaptureSource::Input, 44_100, 2).unwrap(),
        )
        .unwrap();
        assert_eq!(output_core.state(), CaptureState::Idle);
        let status = port.capture_status(CaptureSource::Input).unwrap();
        assert_eq!(status.state, CaptureState::Armed);
        assert_eq!(status.token, 1);
        assert_eq!(status.max_frames, 2);
        port.start_capture(CaptureSource::Input, 1).unwrap();
        let status = port.capture_status(CaptureSource::Input).unwrap();
        assert_eq!(status.frames, 1);
        assert_eq!(status.peak, 0.5);
        port.stop_capture(CaptureSource::Input, 1).unwrap();

        let maintenance = port.poll_capture_maintenance();
        let CaptureOutcome::Completed(completion) =
            maintenance.completion(CaptureSource::Input).unwrap()
        else {
            panic!("input capture must complete")
        };
        assert_eq!(completion.source, CaptureSource::Input);
        assert_eq!(completion.sample_rate, 44_100);
        assert!(maintenance.completion(CaptureSource::Resample).is_none());
        assert_eq!(input_polls.get(), 1);
    }

    #[test]
    fn input_status_does_not_attribute_prior_progress_before_callback_polls_new_arm() {
        let (output, _output_core, _) = FakeSession::capture_ready(48_000);
        let (input_controller, input_core) = capture_channels(4, 2);
        let opened_input = fake_input_session(input_controller, input_core, None);
        let poll_on_begin = Rc::clone(&opened_input.poll_callback_on_begin);
        let poll_on_status = Rc::clone(&opened_input.poll_callback_on_status);
        let mut input = Some(opened_input);
        let mut port = SessionAudioPort::new_with_input_opener(output, move || {
            Ok(Box::new(input.take().unwrap()) as Box<dyn InputSessionLike>)
        });

        port.begin_capture(
            CaptureBuffer::try_new(81, PadId::first(), CaptureSource::Input, 44_100, 1).unwrap(),
        )
        .unwrap();
        port.start_capture(CaptureSource::Input, 81).unwrap();
        let maintenance = port.poll_capture_maintenance();
        assert!(maintenance.completion(CaptureSource::Input).is_some());

        poll_on_begin.set(false);
        port.begin_capture(
            CaptureBuffer::try_new(82, PadId::first(), CaptureSource::Input, 44_100, 2).unwrap(),
        )
        .unwrap();
        assert_eq!(
            port.capture_status(CaptureSource::Input),
            None,
            "the prior callback take must render unavailable during the Arm scheduling window",
        );

        poll_on_status.set(true);
        assert_eq!(
            port.capture_status(CaptureSource::Input),
            Some(CaptureStatus {
                token: 82,
                source: CaptureSource::Input,
                target: PadId::first(),
                state: CaptureState::Armed,
                frames: 0,
                max_frames: 2,
                peak: 0.0,
                hard_limit: false,
            }),
        );
    }

    #[test]
    fn maintenance_polls_each_source_completion_at_most_once_and_types_runtime_errors() {
        let (output, _output_core, output_polls) = FakeSession::capture_ready(48_000);
        let output = output.with_runtime_error(ControlError::ClosedSession);
        let (input_controller, input_core) = capture_channels(4, 1);
        let input_polls = Rc::new(Cell::new(0));
        let mut input = fake_input_session(input_controller, input_core, None);
        input.polls = Rc::clone(&input_polls);
        input.errors = VecDeque::from([ControlError::CommandQueueFull]);
        let mut input = Some(input);
        let mut port = SessionAudioPort::new_with_input_opener(output, move || {
            Ok(Box::new(input.take().unwrap()) as Box<dyn InputSessionLike>)
        });
        port.capture_source_rate(CaptureSource::Input).unwrap();

        let maintenance: CaptureMaintenance = port.poll_capture_maintenance();

        assert_eq!(output_polls.get(), 1);
        assert_eq!(input_polls.get(), 1);
        assert_eq!(
            maintenance.runtime_error(CaptureSource::Resample),
            Some(&CaptureError::OutputRuntime(
                "audio session is closed after a runtime failure".into()
            ))
        );
        assert_eq!(
            maintenance.runtime_error(CaptureSource::Input),
            Some(&CaptureError::InputRuntime(
                "audio command queue is full".into()
            ))
        );
    }

    #[test]
    fn controller_full_begin_error_returns_the_exact_capture_buffer() {
        let (mut output, _core, _) = FakeSession::capture_ready(48_000);
        for token in 1..=4 {
            output.capture.as_mut().unwrap().start(token).unwrap();
        }
        let mut port = SessionAudioPort::new(output);
        let buffer =
            CaptureBuffer::try_new(7, PadId::first(), CaptureSource::Resample, 48_000, 8).unwrap();
        let pointer = buffer.stereo().as_ptr();

        let failure: CaptureCommandFailure = port.begin_capture(buffer).unwrap_err();

        assert_eq!(
            failure.error(),
            &CaptureError::Command(CoreCaptureError::CommandFull)
        );
        let CaptureCommand::Arm(returned) = failure.into_command() else {
            panic!("begin failure must own the rejected arm command")
        };
        assert_eq!(returned.token(), 7);
        assert_eq!(returned.stereo().as_ptr(), pointer);
    }

    #[test]
    fn adapter_teardown_drops_stream_core_allocation_controller_then_output_on_app_thread() {
        let events = Arc::new(CaptureOwnershipEvents::new());
        let (output, _output_core, _) = FakeSession::capture_ready(48_000);
        let output = output.with_capture_ownership_events(Arc::clone(&events));
        let (input_controller, input_core) = capture_channels(4, 1);
        let input = fake_input_session(input_controller, input_core, Some(Arc::clone(&events)));
        let mut input = Some(input);
        let mut port = SessionAudioPort::new_with_input_opener(output, move || {
            Ok(Box::new(input.take().unwrap()) as Box<dyn InputSessionLike>)
        });
        port.capture_source_rate(CaptureSource::Input).unwrap();
        let buffer =
            CaptureBuffer::try_new(1, PadId::first(), CaptureSource::Input, 44_100, 8).unwrap();
        let allocation = buffer.stereo().as_ptr() as usize;
        port.begin_capture(buffer).unwrap();
        assert_eq!(
            port.capture_status(CaptureSource::Input).unwrap().state,
            CaptureState::Armed
        );

        TRACKED_ALLOCATION.store(allocation, Ordering::Release);
        OWNERSHIP_EVENTS.store(Arc::as_ptr(&events).cast_mut(), Ordering::Release);
        IS_APP_OWNER_THREAD.with(|marker| marker.set(true));

        drop(port);

        IS_APP_OWNER_THREAD.with(|marker| marker.set(false));
        OWNERSHIP_EVENTS.store(std::ptr::null_mut(), Ordering::Release);
        TRACKED_ALLOCATION.store(0, Ordering::Release);
        assert_eq!(
            (
                events.stream_core.load(Ordering::Acquire),
                events.allocation.load(Ordering::Acquire),
                events.controller.load(Ordering::Acquire),
                events.output.load(Ordering::Acquire),
            ),
            (2, 1, 3, 4)
        );
        assert!(!events.off_app_thread.load(Ordering::Acquire));
    }

    fn port_for_capture_source(
        source: CaptureSource,
    ) -> (SessionAudioPort<FakeSession>, CaptureCore) {
        let (output, output_core, _) = FakeSession::capture_ready(48_000);
        match source {
            CaptureSource::Resample => (SessionAudioPort::new(output), output_core),
            CaptureSource::Input => {
                let (controller, core) = capture_channels(4, 1);
                let input = fake_input_session(controller, core, None);
                let mut input = Some(input);
                (
                    SessionAudioPort::new_with_input_opener(output, move || {
                        Ok(Box::new(input.take().unwrap()) as Box<dyn InputSessionLike>)
                    }),
                    output_core,
                )
            }
        }
    }

    fn arm_source(port: &mut SessionAudioPort<FakeSession>, source: CaptureSource, token: u64) {
        let sample_rate = match source {
            CaptureSource::Resample => 48_000,
            CaptureSource::Input => 44_100,
        };
        port.begin_capture(
            CaptureBuffer::try_new(token, PadId::first(), source, sample_rate, 8).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn every_capture_command_synchronously_rejects_stale_tokens_for_both_sources() {
        for source in [CaptureSource::Resample, CaptureSource::Input] {
            for command_kind in ["start", "stop", "cancel"] {
                let (mut port, mut output_core) = port_for_capture_source(source);
                arm_source(&mut port, source, 41);
                if source == CaptureSource::Resample {
                    output_core.poll_commands();
                    assert_eq!(output_core.state(), CaptureState::Armed);
                }

                let failure = match command_kind {
                    "start" => port.start_capture(source, 42).unwrap_err(),
                    "stop" => port.stop_capture(source, 42).unwrap_err(),
                    "cancel" => port.cancel_capture(source, 42).unwrap_err(),
                    _ => unreachable!(),
                };

                assert_eq!(
                    failure.error(),
                    &CaptureError::CommandTokenMismatch {
                        expected: 41,
                        received: 42,
                    }
                );
                match (command_kind, failure.into_command()) {
                    ("start", CaptureCommand::Start { token: 42 })
                    | ("stop", CaptureCommand::Stop { token: 42 })
                    | ("cancel", CaptureCommand::Cancel { token: 42 }) => {}
                    (_, command) => panic!("wrong rejected command: {command:?}"),
                }
                if source == CaptureSource::Resample {
                    assert_eq!(output_core.state(), CaptureState::Armed);
                } else {
                    assert_eq!(
                        port.capture_status(CaptureSource::Input).unwrap().state,
                        CaptureState::Armed
                    );
                }
            }
        }
    }

    #[test]
    fn every_capture_command_returns_exact_command_on_active_source_mismatch() {
        for command_kind in ["start", "stop", "cancel"] {
            let (mut port, _) = port_for_capture_source(CaptureSource::Input);
            arm_source(&mut port, CaptureSource::Input, 41);

            let failure = match command_kind {
                "start" => port.start_capture(CaptureSource::Resample, 41).unwrap_err(),
                "stop" => port.stop_capture(CaptureSource::Resample, 41).unwrap_err(),
                "cancel" => port
                    .cancel_capture(CaptureSource::Resample, 41)
                    .unwrap_err(),
                _ => unreachable!(),
            };

            assert_eq!(
                failure.error(),
                &CaptureError::CommandSourceMismatch {
                    expected: CaptureSource::Input,
                    received: CaptureSource::Resample,
                }
            );
            match (command_kind, failure.into_command()) {
                ("start", CaptureCommand::Start { token: 41 })
                | ("stop", CaptureCommand::Stop { token: 41 })
                | ("cancel", CaptureCommand::Cancel { token: 41 }) => {}
                (_, command) => panic!("wrong rejected command: {command:?}"),
            }
            assert_eq!(
                port.capture_status(CaptureSource::Input).unwrap().state,
                CaptureState::Armed
            );
        }
    }

    #[test]
    fn arm_rejects_zero_and_active_identity_with_exact_buffer_ownership() {
        for source in [CaptureSource::Resample, CaptureSource::Input] {
            let sample_rate = match source {
                CaptureSource::Resample => 48_000,
                CaptureSource::Input => 44_100,
            };
            let (mut port, _output_core) = port_for_capture_source(source);
            let zero = CaptureBuffer::try_new(0, PadId::first(), source, sample_rate, 8).unwrap();
            let zero_pointer = zero.stereo().as_ptr();
            let zero_failure = port.begin_capture(zero).unwrap_err();
            assert_eq!(zero_failure.error(), &CaptureError::ZeroCommandToken);
            let CaptureCommand::Arm(zero_returned) = zero_failure.into_command() else {
                panic!("zero-token arm must return the buffer")
            };
            assert_eq!(zero_returned.stereo().as_ptr(), zero_pointer);

            arm_source(&mut port, source, 9);
            let overlapping_source = match source {
                CaptureSource::Resample => CaptureSource::Input,
                CaptureSource::Input => CaptureSource::Resample,
            };
            let overlapping_rate = match overlapping_source {
                CaptureSource::Resample => 48_000,
                CaptureSource::Input => 44_100,
            };
            let overlapping =
                CaptureBuffer::try_new(10, PadId::first(), overlapping_source, overlapping_rate, 8)
                    .unwrap();
            let overlapping_pointer = overlapping.stereo().as_ptr();
            let overlapping_failure = port.begin_capture(overlapping).unwrap_err();
            assert_eq!(
                overlapping_failure.error(),
                &CaptureError::ActiveCapture { source, token: 9 }
            );
            let CaptureCommand::Arm(overlapping_returned) = overlapping_failure.into_command()
            else {
                panic!("active arm must return the buffer")
            };
            assert_eq!(overlapping_returned.stereo().as_ptr(), overlapping_pointer);
        }
    }

    #[test]
    fn arm_rate_mismatch_is_command_typed_and_returns_exact_buffer() {
        for source in [CaptureSource::Resample, CaptureSource::Input] {
            let expected_rate = match source {
                CaptureSource::Resample => 48_000,
                CaptureSource::Input => 44_100,
            };
            let received_rate = expected_rate - 1;
            let (mut port, _output_core) = port_for_capture_source(source);
            let buffer =
                CaptureBuffer::try_new(3, PadId::first(), source, received_rate, 8).unwrap();
            let pointer = buffer.stereo().as_ptr();

            let failure = port.begin_capture(buffer).unwrap_err();

            assert_eq!(
                failure.error(),
                &CaptureError::CommandRateMismatch {
                    expected: expected_rate,
                    received: received_rate,
                }
            );
            let CaptureCommand::Arm(returned) = failure.into_command() else {
                panic!("rate-mismatched arm must return the exact buffer")
            };
            assert_eq!(returned.stereo().as_ptr(), pointer);
            assert!(port.capture_status(source).is_none());
        }
    }

    struct UnsupportedAudioPort;

    impl AudioPort for UnsupportedAudioPort {
        fn sample_rate(&self) -> u32 {
            48_000
        }

        fn channels(&self) -> u16 {
            2
        }

        fn render_horizon(&self) -> u64 {
            0
        }

        fn install(
            &mut self,
            _pad: PadId,
            _sample: Arc<SampleBuffer>,
            _settings: PadSettings,
            _mix: sampler_core::PadMixSettings,
        ) -> Result<SampleSlot, String> {
            Err("unsupported".into())
        }

        fn trigger(&mut self, _pad: PadId, _at: u64, _velocity: f32) -> Result<(), String> {
            Ok(())
        }

        fn release(&mut self, _pad: PadId, _at: u64) -> Result<(), String> {
            Ok(())
        }

        fn stop_pad(&mut self, _pad: PadId) -> Result<(), String> {
            Ok(())
        }

        fn stop_all(&mut self) -> Result<(), String> {
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

        fn capture_support(&self) -> CaptureSupport {
            CaptureSupport::Unsupported
        }
    }

    #[test]
    fn unsupported_audio_port_does_not_claim_partial_capture_support() {
        let mut port = UnsupportedAudioPort;
        assert_eq!(port.capture_support(), CaptureSupport::Unsupported);
        for source in [CaptureSource::Resample, CaptureSource::Input] {
            assert_eq!(
                port.capture_source_rate(source),
                Err(CaptureError::Unsupported)
            );
            assert!(port.capture_status(source).is_none());
            assert!(port.capture_completion(source).is_none());
            assert!(port.capture_runtime_error(source).is_none());
        }
        let buffer =
            CaptureBuffer::try_new(1, PadId::first(), CaptureSource::Resample, 48_000, 8).unwrap();
        let pointer = buffer.stereo().as_ptr();
        let failure = port.begin_capture(buffer).unwrap_err();
        assert_eq!(failure.error(), &CaptureError::Unsupported);
        let CaptureCommand::Arm(returned) = failure.into_command() else {
            panic!("unsupported arm must return the exact buffer")
        };
        assert_eq!(returned.stereo().as_ptr(), pointer);

        for command in ["start", "stop", "cancel"] {
            let failure = match command {
                "start" => port.start_capture(CaptureSource::Resample, 7).unwrap_err(),
                "stop" => port.stop_capture(CaptureSource::Resample, 7).unwrap_err(),
                "cancel" => port.cancel_capture(CaptureSource::Resample, 7).unwrap_err(),
                _ => unreachable!(),
            };
            assert_eq!(failure.error(), &CaptureError::Unsupported);
            match (command, failure.into_command()) {
                ("start", CaptureCommand::Start { token: 7 })
                | ("stop", CaptureCommand::Stop { token: 7 })
                | ("cancel", CaptureCommand::Cancel { token: 7 }) => {}
                (_, rejected) => panic!("wrong unsupported command: {rejected:?}"),
            }
        }

        let maintenance = port.poll_capture_maintenance();
        for source in [CaptureSource::Resample, CaptureSource::Input] {
            assert!(maintenance.completion(source).is_none());
            assert!(maintenance.runtime_error(source).is_none());
        }
    }
}
