use std::cell::RefCell;
use std::fmt::Display;
use std::sync::Arc;

use sampler_audio::{
    AudioSession, ControlError, DeviceError, Frame, SampleBuffer, SampleSlot, Telemetry,
};
use sampler_core::{PadId, PadSettings};

pub trait AudioPort {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn render_horizon(&self) -> Frame;
    fn install(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, String>;
    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String>;
    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String>;
    fn stop_pad(&mut self, pad: PadId) -> Result<(), String>;
    fn stop_all(&mut self) -> Result<(), String>;
    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String>;
    fn reclaim_retired(&mut self) -> usize;
    fn latest_telemetry(&mut self) -> Option<Telemetry>;
    fn poll_runtime_error(&mut self) -> Option<String>;
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
    ) -> Result<SampleSlot, Self::CommandError>;
    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), Self::CommandError>;
    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), Self::CommandError>;
    fn stop_pad(&mut self, pad: PadId) -> Result<(), Self::CommandError>;
    fn stop_all(&mut self) -> Result<(), Self::CommandError>;
    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), Self::CommandError>;
    fn reclaim_retired(&mut self) -> usize;
    fn latest_telemetry(&mut self) -> Option<Telemetry>;
    fn poll_error(&mut self) -> Option<Self::RuntimeError>;
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
    ) -> Result<SampleSlot, Self::CommandError> {
        self.controller_mut().install(pad, sample, settings)
    }

    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), Self::CommandError> {
        self.controller_mut().trigger(pad, at, velocity)
    }

    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), Self::CommandError> {
        self.controller_mut().release(pad, at)
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

    fn reclaim_retired(&mut self) -> usize {
        self.controller_mut().reclaim_retired()
    }

    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.controller_mut().latest_telemetry()
    }

    fn poll_error(&mut self) -> Option<Self::RuntimeError> {
        AudioSession::poll_error(self)
    }
}

pub struct SessionAudioPort<S = AudioSession> {
    session: RefCell<S>,
}

impl<S> SessionAudioPort<S> {
    fn new(session: S) -> Self {
        Self {
            session: RefCell::new(session),
        }
    }
}

impl<S> AudioPort for SessionAudioPort<S>
where
    S: SessionLike,
{
    fn sample_rate(&self) -> u32 {
        self.session.borrow().sample_rate()
    }

    fn channels(&self) -> u16 {
        self.session.borrow().channels()
    }

    fn render_horizon(&self) -> Frame {
        self.session.borrow_mut().render_horizon()
    }

    fn install(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, String> {
        self.session
            .get_mut()
            .install(pad, sample, settings)
            .map_err(|error| error.to_string())
    }

    fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String> {
        self.session
            .get_mut()
            .trigger(pad, at, velocity)
            .map_err(|error| error.to_string())
    }

    fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String> {
        self.session
            .get_mut()
            .release(pad, at)
            .map_err(|error| error.to_string())
    }

    fn stop_pad(&mut self, pad: PadId) -> Result<(), String> {
        self.session
            .get_mut()
            .stop_pad(pad)
            .map_err(|error| error.to_string())
    }

    fn stop_all(&mut self) -> Result<(), String> {
        self.session
            .get_mut()
            .stop_all()
            .map_err(|error| error.to_string())
    }

    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), String> {
        self.session
            .get_mut()
            .update_pad(pad, settings)
            .map_err(|error| error.to_string())
    }

    fn reclaim_retired(&mut self) -> usize {
        self.session.get_mut().reclaim_retired()
    }

    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.session.get_mut().latest_telemetry()
    }

    fn poll_runtime_error(&mut self) -> Option<String> {
        self.session
            .get_mut()
            .poll_error()
            .map(|error| error.to_string())
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
    use std::collections::VecDeque;
    use std::sync::Arc;

    use sampler_audio::{ControlError, SampleBuffer, SampleSlot, Telemetry};
    use sampler_core::{PadId, PadSettings};

    use super::{AudioPort, SessionAudioPort, SessionLike};

    struct FakeSession {
        sample_rate: u32,
        channels: u16,
        horizon: u64,
        trigger_error: Option<ControlError>,
        telemetry: VecDeque<Telemetry>,
        retired: usize,
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
            _sample: Arc<SampleBuffer>,
            _settings: PadSettings,
        ) -> Result<SampleSlot, Self::CommandError> {
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

        fn latest_telemetry(&mut self) -> Option<Telemetry> {
            let mut latest = None;
            while let Some(next) = self.telemetry.pop_front() {
                latest = Some(next);
            }
            latest
        }

        fn poll_error(&mut self) -> Option<Self::RuntimeError> {
            None
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
        }
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
}
