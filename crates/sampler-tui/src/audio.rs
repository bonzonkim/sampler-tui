use std::array;
use std::cell::RefCell;
use std::fmt::Display;
use std::sync::Arc;

use sampler_audio::{
    AudioSession, ControlError, DeviceError, Frame, SAMPLE_SLOT_COUNT, SampleBuffer, SampleSlot,
    Telemetry,
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
    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, String> {
        self.install(pad, sample, settings)
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
    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, Self::CommandError> {
        self.install(pad, sample, settings)
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
    fn stop_pad(&mut self, pad: PadId) -> Result<(), Self::CommandError>;
    fn stop_all(&mut self) -> Result<(), Self::CommandError>;
    fn update_pad(&mut self, pad: PadId, settings: PadSettings) -> Result<(), Self::CommandError>;
    fn reclaim_retired_slot(&mut self) -> Option<SampleSlot> {
        None
    }
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

    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, Self::CommandError> {
        self.controller_mut()
            .install_recovery(pad, sample, settings)
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

    fn reclaim_retired_slot(&mut self) -> Option<SampleSlot> {
        self.controller_mut().reclaim_retired_slot()
    }

    fn latest_telemetry(&mut self) -> Option<Telemetry> {
        self.controller_mut().latest_telemetry()
    }

    fn poll_error(&mut self) -> Option<Self::RuntimeError> {
        AudioSession::poll_error(self)
    }
}

pub struct SessionAudioPort<S = AudioSession> {
    session: Option<RefCell<S>>,
    retained_samples: [Option<Arc<SampleBuffer>>; SAMPLE_SLOT_COUNT],
}

impl<S> SessionAudioPort<S> {
    fn new(session: S) -> Self {
        Self {
            session: Some(RefCell::new(session)),
            retained_samples: array::from_fn(|_| None),
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
}

impl<S> Drop for SessionAudioPort<S> {
    fn drop(&mut self) {
        drop(self.session.take());
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
    ) -> Result<SampleSlot, String> {
        let retained = Arc::clone(&sample);
        let slot = self
            .session_mut()
            .install(pad, sample, settings)
            .map_err(|error| error.to_string())?;
        self.retained_samples[slot.index()] = Some(retained);
        Ok(slot)
    }

    fn install_recovery(
        &mut self,
        pad: PadId,
        sample: Arc<SampleBuffer>,
        settings: PadSettings,
    ) -> Result<SampleSlot, String> {
        let retained = Arc::clone(&sample);
        let slot = self
            .session_mut()
            .install_recovery(pad, sample, settings)
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
}

pub fn open_default_audio() -> Result<Box<dyn AudioPort>, String> {
    AudioSession::open_default()
        .map(SessionAudioPort::new)
        .map(|port| Box::new(port) as Box<dyn AudioPort>)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::{Arc, Weak};
    use std::thread::ThreadId;

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
        retired_slots: VecDeque<SampleSlot>,
        ownership: Option<OwnershipProbe>,
    }

    #[derive(Clone)]
    struct OwnershipProbe {
        installed: Rc<RefCell<Vec<Weak<SampleBuffer>>>>,
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
        port.install(PadId::first(), Arc::clone(&sample), PadSettings::default())
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
        port.install(PadId::first(), Arc::clone(&first), PadSettings::default())
            .unwrap();
        drop(first);
        assert!(first_weak.upgrade().is_some());

        let second = Arc::new(SampleBuffer::new(48_000, vec![0.2, 0.2]).unwrap());
        let second_weak = Arc::downgrade(&second);
        port.install(PadId::first(), Arc::clone(&second), PadSettings::default())
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
        port.install(PadId::first(), Arc::clone(&sample), PadSettings::default())
            .unwrap();
        drop(sample);
        assert!(weak.upgrade().is_some());

        assert_eq!(port.reclaim_retired(), 1);

        assert!(weak.upgrade().is_none());
    }
}
