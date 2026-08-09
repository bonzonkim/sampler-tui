use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use midir::{ConnectErrorKind, Ignore, MidiInput, MidiInputConnection};
use rtrb::{Consumer, Producer, RingBuffer};
use sampler_core::{MidiChannel, MidiNote};

pub const MIDI_INGRESS_CAPACITY: usize = 512;
pub const MAX_MIDI_DRAIN: usize = 128;
const MIDI_PORT_RESCAN_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiPortInfo {
    index: usize,
    backend_id: String,
    name: String,
}

impl MidiPortInfo {
    pub fn new(index: usize, backend_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            index,
            backend_id: backend_id.into(),
            name: name.into(),
        }
    }

    pub const fn index(&self) -> usize {
        self.index
    }

    pub fn backend_id(&self) -> &str {
        &self.backend_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiBackendPort {
    pub backend_id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MidiServiceError {
    #[error("could not initialize MIDI input: {0}")]
    BackendInit(String),
    #[error("could not enumerate MIDI input ports: {0}")]
    PortEnumeration(String),
    #[error("could not read MIDI port {backend_id} name: {message}")]
    PortName { backend_id: String, message: String },
    #[error("MIDI port index {0} is outside the current snapshot")]
    PortIndex(usize),
    #[error("MIDI port {0} is no longer in the backend snapshot")]
    StalePort(String),
    #[error("could not connect MIDI input: {0}")]
    Connect(String),
}

pub trait MidiConnection {
    fn close(self: Box<Self>);
}

pub trait MidiBackend {
    fn list_ports(&mut self) -> Result<Vec<MidiBackendPort>, MidiServiceError>;

    fn connect(
        &mut self,
        port: &MidiBackendPort,
        producer: MidiIngressProducer,
    ) -> Result<Box<dyn MidiConnection>, MidiServiceError>;
}

#[derive(Default)]
pub struct MidirBackend;

struct MidirConnection {
    connection: Option<MidiInputConnection<MidiIngressProducer>>,
}

impl MidiConnection for MidirConnection {
    fn close(mut self: Box<Self>) {
        if let Some(connection) = self.connection.take() {
            let _ = connection.close();
        }
    }
}

impl MidiBackend for MidirBackend {
    fn list_ports(&mut self) -> Result<Vec<MidiBackendPort>, MidiServiceError> {
        let mut input = MidiInput::new("sampler-tui MIDI discovery")
            .map_err(|error| MidiServiceError::BackendInit(error.to_string()))?;
        input.ignore(Ignore::All);
        input
            .ports()
            .into_iter()
            .map(|port| {
                let backend_id = port.id();
                let name = input
                    .port_name(&port)
                    .map_err(|error| MidiServiceError::PortName {
                        backend_id: backend_id.clone(),
                        message: error.to_string(),
                    })?;
                Ok(MidiBackendPort { backend_id, name })
            })
            .collect()
    }

    fn connect(
        &mut self,
        port: &MidiBackendPort,
        producer: MidiIngressProducer,
    ) -> Result<Box<dyn MidiConnection>, MidiServiceError> {
        let mut input = MidiInput::new("sampler-tui MIDI input")
            .map_err(|error| MidiServiceError::BackendInit(error.to_string()))?;
        input.ignore(Ignore::All);
        let backend_port = input
            .find_port_by_id(&port.backend_id)
            .ok_or_else(|| MidiServiceError::StalePort(port.backend_id.clone()))?;
        let connection_name = format!("sampler-tui MIDI input: {}", port.name);
        let connection = input
            .connect(
                &backend_port,
                &connection_name,
                |_timestamp, message, producer| producer.try_push_message(message),
                producer,
            )
            .map_err(|error| match error.kind() {
                ConnectErrorKind::InvalidPort => {
                    MidiServiceError::StalePort(port.backend_id.clone())
                }
                _ => MidiServiceError::Connect(error.to_string()),
            })?;
        Ok(Box::new(MidirConnection {
            connection: Some(connection),
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidiServiceEvent {
    PortDisappeared(MidiPortInfo),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidiServiceStatus {
    Disconnected,
    Connected(MidiPortInfo),
}

struct ConnectedMidi {
    port: MidiPortInfo,
    connection: Box<dyn MidiConnection>,
    consumer: MidiIngressConsumer,
}

pub(crate) struct PreparedMidiConnection {
    connected: Option<ConnectedMidi>,
}

impl Drop for PreparedMidiConnection {
    fn drop(&mut self) {
        if let Some(connected) = self.connected.take() {
            connected.connection.close();
        }
    }
}

pub struct MidiService {
    backend: Box<dyn MidiBackend>,
    backend_ports: Vec<MidiBackendPort>,
    ports: Vec<MidiPortInfo>,
    connected: Option<ConnectedMidi>,
    last_scan: Option<Instant>,
}

impl MidiService {
    pub fn new(backend: Box<dyn MidiBackend>) -> Self {
        Self {
            backend,
            backend_ports: Vec::new(),
            ports: Vec::new(),
            connected: None,
            last_scan: None,
        }
    }

    pub fn startup(&mut self, now: Instant) -> Result<(), MidiServiceError> {
        self.refresh_ports()?;
        self.last_scan = Some(now);
        if self.ports.len() == 1 {
            self.connect(0)?;
        }
        Ok(())
    }

    pub fn refresh_ports(&mut self) -> Result<(), MidiServiceError> {
        let backend_ports = self.backend.list_ports()?;
        let ports = backend_ports
            .iter()
            .enumerate()
            .map(|(index, port)| MidiPortInfo::new(index, &port.backend_id, &port.name))
            .collect();
        self.backend_ports = backend_ports;
        self.ports = ports;
        Ok(())
    }

    pub fn ports(&self) -> &[MidiPortInfo] {
        &self.ports
    }

    pub fn connected_port(&self) -> Option<&MidiPortInfo> {
        self.connected.as_ref().map(|connected| &connected.port)
    }

    pub fn status(&self) -> MidiServiceStatus {
        self.connected_port()
            .map_or(MidiServiceStatus::Disconnected, |port| {
                MidiServiceStatus::Connected(port.clone())
            })
    }

    pub fn connect(&mut self, index: usize) -> Result<(), MidiServiceError> {
        let prepared = self.prepare_connection(index)?;
        self.commit_connection(prepared);
        Ok(())
    }

    pub(crate) fn prepare_connection(
        &mut self,
        index: usize,
    ) -> Result<PreparedMidiConnection, MidiServiceError> {
        let port = self
            .backend_ports
            .get(index)
            .cloned()
            .ok_or(MidiServiceError::PortIndex(index))?;
        let info = self
            .ports
            .get(index)
            .cloned()
            .ok_or(MidiServiceError::PortIndex(index))?;
        let (producer, consumer) = midi_ingress();
        let connection = self.backend.connect(&port, producer)?;
        Ok(PreparedMidiConnection {
            connected: Some(ConnectedMidi {
                port: info,
                connection,
                consumer,
            }),
        })
    }

    pub(crate) fn commit_connection(&mut self, mut prepared: PreparedMidiConnection) {
        let connected = prepared
            .connected
            .take()
            .expect("a prepared MIDI connection can be committed exactly once");
        let previous = self.connected.replace(connected);
        if let Some(previous) = previous {
            previous.connection.close();
        }
    }

    pub fn disconnect(&mut self) -> bool {
        let Some(connected) = self.connected.take() else {
            return false;
        };
        connected.connection.close();
        true
    }

    pub fn maintain(&mut self, now: Instant) -> Result<Option<MidiServiceEvent>, MidiServiceError> {
        if self
            .last_scan
            .is_some_and(|last| now.saturating_duration_since(last) < MIDI_PORT_RESCAN_INTERVAL)
        {
            return Ok(None);
        }
        self.last_scan = Some(now);
        self.refresh_ports()?;
        let Some(connected) = self.connected_port().cloned() else {
            return Ok(None);
        };
        if let Some(current) = self
            .ports
            .iter()
            .find(|port| port.backend_id == connected.backend_id)
            .cloned()
        {
            self.connected
                .as_mut()
                .expect("connected state was observed")
                .port = current;
            return Ok(None);
        }
        self.disconnect();
        Ok(Some(MidiServiceEvent::PortDisappeared(connected)))
    }

    pub fn drain_events(&mut self, output: &mut [MidiEvent]) -> usize {
        self.connected
            .as_mut()
            .map_or(0, |connected| connected.consumer.drain_into(output))
    }

    pub fn take_lost_count(&self) -> usize {
        self.connected
            .as_ref()
            .map_or(0, |connected| connected.consumer.take_lost_count())
    }

    pub(crate) fn queued_event_count(&self) -> usize {
        self.connected
            .as_ref()
            .map_or(0, |connected| connected.consumer.consumer.slots())
    }
}

impl Drop for MidiService {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiEvent {
    NoteOn {
        channel: MidiChannel,
        note: MidiNote,
        velocity: u8,
    },
    NoteOff {
        channel: MidiChannel,
        note: MidiNote,
    },
}

pub fn parse_midi_message(message: &[u8]) -> Option<MidiEvent> {
    let &[status, raw_note, velocity] = message else {
        return None;
    };
    if raw_note > 127 || velocity > 127 {
        return None;
    }

    let channel = MidiChannel::new((status & 0x0f) + 1).ok()?;
    let note = MidiNote::new(raw_note).ok()?;
    match status & 0xf0 {
        0x80 => Some(MidiEvent::NoteOff { channel, note }),
        0x90 if velocity == 0 => Some(MidiEvent::NoteOff { channel, note }),
        0x90 => Some(MidiEvent::NoteOn {
            channel,
            note,
            velocity,
        }),
        _ => None,
    }
}

pub struct MidiIngressProducer {
    producer: Producer<MidiEvent>,
    lost: Arc<AtomicUsize>,
}

impl MidiIngressProducer {
    pub fn try_push_message(&mut self, message: &[u8]) {
        let Some(event) = parse_midi_message(message) else {
            return;
        };
        if self.producer.push(event).is_err() {
            increment_lost(&self.lost);
        }
    }
}

pub struct MidiIngressConsumer {
    consumer: Consumer<MidiEvent>,
    lost: Arc<AtomicUsize>,
}

impl MidiIngressConsumer {
    pub fn drain_into(&mut self, output: &mut [MidiEvent]) -> usize {
        let mut drained = 0;
        for slot in output.iter_mut().take(MAX_MIDI_DRAIN) {
            let Ok(event) = self.consumer.pop() else {
                break;
            };
            *slot = event;
            drained += 1;
        }
        drained
    }

    pub fn lost_count(&self) -> usize {
        self.lost.load(Ordering::Relaxed)
    }

    pub fn take_lost_count(&self) -> usize {
        self.lost.swap(0, Ordering::AcqRel)
    }
}

pub fn midi_ingress() -> (MidiIngressProducer, MidiIngressConsumer) {
    let (producer, consumer) = RingBuffer::new(MIDI_INGRESS_CAPACITY);
    let lost = Arc::new(AtomicUsize::new(0));
    (
        MidiIngressProducer {
            producer,
            lost: Arc::clone(&lost),
        },
        MidiIngressConsumer { consumer, lost },
    )
}

fn increment_lost(lost: &AtomicUsize) {
    let _ = lost.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
        Some(count.saturating_add(1))
    });
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{SyncSender, sync_channel};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use sampler_core::{MidiChannel, MidiNote};

    use super::{
        MAX_MIDI_DRAIN, MIDI_INGRESS_CAPACITY, MidiBackend, MidiBackendPort, MidiConnection,
        MidiEvent, MidiPortInfo, MidiService, MidiServiceError, MidiServiceEvent,
        MidiServiceStatus, MidirBackend, increment_lost, midi_ingress, parse_midi_message,
    };

    #[derive(Default)]
    struct FakeState {
        listings: VecDeque<Result<Vec<MidiBackendPort>, MidiServiceError>>,
        connections: VecDeque<Result<(), MidiServiceError>>,
        messages_on_connect: VecDeque<Vec<Vec<u8>>>,
        log: Vec<String>,
    }

    struct FakeBackend {
        state: Arc<Mutex<FakeState>>,
    }

    impl FakeBackend {
        fn new(state: Arc<Mutex<FakeState>>) -> Self {
            Self { state }
        }
    }

    impl Drop for FakeBackend {
        fn drop(&mut self) {
            self.state
                .lock()
                .unwrap()
                .log
                .push("backend-drop".to_owned());
        }
    }

    struct FakeConnection {
        id: String,
        state: Arc<Mutex<FakeState>>,
        producer: Option<super::MidiIngressProducer>,
    }

    impl MidiConnection for FakeConnection {
        fn close(mut self: Box<Self>) {
            drop(self.producer.take());
            self.state
                .lock()
                .unwrap()
                .log
                .push(format!("ingress-drop:{}", self.id));
            self.state
                .lock()
                .unwrap()
                .log
                .push(format!("close:{}", self.id));
        }
    }

    impl Drop for FakeConnection {
        fn drop(&mut self) {
            if self.producer.take().is_some() {
                self.state
                    .lock()
                    .unwrap()
                    .log
                    .push(format!("drop:{}", self.id));
            }
        }
    }

    impl MidiBackend for FakeBackend {
        fn list_ports(&mut self) -> Result<Vec<MidiBackendPort>, MidiServiceError> {
            let mut state = self.state.lock().unwrap();
            state.log.push("list".to_owned());
            state.listings.pop_front().unwrap_or_else(|| Ok(Vec::new()))
        }

        fn connect(
            &mut self,
            port: &MidiBackendPort,
            mut producer: super::MidiIngressProducer,
        ) -> Result<Box<dyn MidiConnection>, MidiServiceError> {
            let mut state = self.state.lock().unwrap();
            state.log.push(format!("connect:{}", port.backend_id));
            state.connections.pop_front().unwrap_or(Ok(()))?;
            for message in state.messages_on_connect.pop_front().unwrap_or_default() {
                producer.try_push_message(&message);
            }
            drop(state);
            Ok(Box::new(FakeConnection {
                id: port.backend_id.clone(),
                state: Arc::clone(&self.state),
                producer: Some(producer),
            }))
        }
    }

    fn backend_port(id: &str, name: &str) -> MidiBackendPort {
        MidiBackendPort {
            backend_id: id.to_owned(),
            name: name.to_owned(),
        }
    }

    fn fake_service(
        listings: impl IntoIterator<Item = Result<Vec<MidiBackendPort>, MidiServiceError>>,
        connections: impl IntoIterator<Item = Result<(), MidiServiceError>>,
    ) -> (MidiService, Arc<Mutex<FakeState>>) {
        let state = Arc::new(Mutex::new(FakeState {
            listings: listings.into_iter().collect(),
            connections: connections.into_iter().collect(),
            messages_on_connect: VecDeque::new(),
            log: Vec::new(),
        }));
        let service = MidiService::new(Box::new(FakeBackend::new(Arc::clone(&state))));
        (service, state)
    }

    type LifecycleTrace = Arc<Mutex<Vec<&'static str>>>;

    struct CallbackDataSentinel {
        trace: LifecycleTrace,
    }

    impl CallbackDataSentinel {
        fn invoke(&mut self, producer: &mut super::MidiIngressProducer) {
            self.trace.lock().unwrap().push("callback");
            producer.try_push_message(&[0x90, 64, 96]);
        }
    }

    impl Drop for CallbackDataSentinel {
        fn drop(&mut self) {
            self.trace.lock().unwrap().push("callback-data-drop");
        }
    }

    struct AppAudioDependentSentinel {
        trace: LifecycleTrace,
    }

    impl AppAudioDependentSentinel {
        fn new(trace: LifecycleTrace) -> Self {
            Self { trace }
        }
    }

    impl Drop for AppAudioDependentSentinel {
        fn drop(&mut self) {
            self.trace.lock().unwrap().push("dependent-drop");
        }
    }

    struct ThreadedConnection {
        trace: LifecycleTrace,
        stop: Option<SyncSender<()>>,
        worker: Option<JoinHandle<(super::MidiIngressProducer, CallbackDataSentinel)>>,
    }

    impl ThreadedConnection {
        fn quiesce(&mut self) {
            let Some(stop) = self.stop.take() else {
                return;
            };
            self.trace.lock().unwrap().push("close-request");
            stop.send(()).unwrap();
            let (producer, callback_data) = self.worker.take().unwrap().join().unwrap();
            self.trace.lock().unwrap().push("callback-thread-joined");
            drop(producer);
            drop(callback_data);
        }
    }

    impl MidiConnection for ThreadedConnection {
        fn close(mut self: Box<Self>) {
            self.quiesce();
            self.trace.lock().unwrap().push("connection-close");
        }
    }

    impl Drop for ThreadedConnection {
        fn drop(&mut self) {
            self.quiesce();
        }
    }

    struct ThreadedBackend {
        trace: LifecycleTrace,
    }

    impl ThreadedBackend {
        fn new(trace: LifecycleTrace) -> Self {
            Self { trace }
        }
    }

    impl MidiBackend for ThreadedBackend {
        fn list_ports(&mut self) -> Result<Vec<MidiBackendPort>, MidiServiceError> {
            self.trace.lock().unwrap().push("backend-list");
            Ok(vec![backend_port("threaded", "Threaded")])
        }

        fn connect(
            &mut self,
            _port: &MidiBackendPort,
            producer: super::MidiIngressProducer,
        ) -> Result<Box<dyn MidiConnection>, MidiServiceError> {
            self.trace.lock().unwrap().push("connect");
            let trace = Arc::clone(&self.trace);
            let (ready_tx, ready_rx) = sync_channel(0);
            let (stop_tx, stop_rx) = sync_channel(0);
            let worker = thread::spawn(move || {
                let mut producer = producer;
                let mut callback_data = CallbackDataSentinel {
                    trace: Arc::clone(&trace),
                };
                callback_data.invoke(&mut producer);
                ready_tx.send(()).unwrap();
                stop_rx.recv().unwrap();
                trace.lock().unwrap().push("callback-thread-stop");
                (producer, callback_data)
            });
            ready_rx.recv().unwrap();
            Ok(Box::new(ThreadedConnection {
                trace: Arc::clone(&self.trace),
                stop: Some(stop_tx),
                worker: Some(worker),
            }))
        }
    }

    impl Drop for ThreadedBackend {
        fn drop(&mut self) {
            self.trace.lock().unwrap().push("backend-drop");
        }
    }

    fn note(value: u8) -> MidiNote {
        MidiNote::new(value).unwrap()
    }

    fn channel(value: u8) -> MidiChannel {
        MidiChannel::new(value).unwrap()
    }

    #[test]
    fn parses_note_on_and_off_status_for_all_sixteen_channels() {
        for raw_channel in 0_u8..16 {
            let numbered = channel(raw_channel + 1);
            assert_eq!(
                parse_midi_message(&[0x90 | raw_channel, 0, 127]),
                Some(MidiEvent::NoteOn {
                    channel: numbered,
                    note: note(0),
                    velocity: 127,
                }),
                "Note On channel {}",
                raw_channel + 1
            );
            assert_eq!(
                parse_midi_message(&[0x80 | raw_channel, 127, 64]),
                Some(MidiEvent::NoteOff {
                    channel: numbered,
                    note: note(127),
                }),
                "Note Off channel {}",
                raw_channel + 1
            );
        }
    }

    #[test]
    fn note_on_velocity_zero_is_normalized_to_note_off_and_boundaries_are_exact() {
        assert_eq!(
            parse_midi_message(&[0x90, 0, 1]),
            Some(MidiEvent::NoteOn {
                channel: channel(1),
                note: note(0),
                velocity: 1,
            })
        );
        assert_eq!(
            parse_midi_message(&[0x9f, 127, 127]),
            Some(MidiEvent::NoteOn {
                channel: channel(16),
                note: note(127),
                velocity: 127,
            })
        );
        assert_eq!(
            parse_midi_message(&[0x95, 42, 0]),
            Some(MidiEvent::NoteOff {
                channel: channel(6),
                note: note(42),
            })
        );
    }

    #[test]
    fn rejects_malformed_running_status_system_and_non_note_messages() {
        let rejected: &[&[u8]] = &[
            &[],
            &[0x90],
            &[0x90, 60],
            &[0x90, 60, 100, 0],
            &[60, 100],
            &[0x90, 128, 1],
            &[0x90, 60, 128],
            &[0x80, 60, 128],
            &[0xa0, 60, 100],
            &[0xb0, 1, 127],
            &[0xc0, 1],
            &[0xd0, 1],
            &[0xe0, 0, 64],
            &[0xf0, 1, 0xf7],
            &[0xf1, 0, 0],
            &[0xf8],
            &[0xfe],
            &[0xff],
        ];

        for message in rejected {
            assert_eq!(parse_midi_message(message), None, "message {message:?}");
        }
    }

    #[test]
    fn ingress_is_fifo_with_exact_capacity_and_counts_overflow() {
        let (mut producer, mut consumer) = midi_ingress();
        for index in 0..MIDI_INGRESS_CAPACITY {
            producer.try_push_message(&[0x90, (index % 128) as u8, 100]);
        }
        assert_eq!(consumer.lost_count(), 0);

        producer.try_push_message(&[0x90, 99, 100]);
        assert_eq!(consumer.lost_count(), 1);

        let sentinel = MidiEvent::NoteOff {
            channel: channel(16),
            note: note(127),
        };
        let mut output = [sentinel; MAX_MIDI_DRAIN];
        let drained = consumer.drain_into(&mut output);
        assert_eq!(drained, MAX_MIDI_DRAIN);
        for (index, event) in output.into_iter().enumerate() {
            assert_eq!(
                event,
                MidiEvent::NoteOn {
                    channel: channel(1),
                    note: note(index as u8),
                    velocity: 100,
                }
            );
        }
    }

    #[test]
    fn drain_never_exceeds_one_hundred_twenty_eight_events_per_call() {
        let (mut producer, mut consumer) = midi_ingress();
        for index in 0..200_u8 {
            producer.try_push_message(&[0x90, index % 128, 1]);
        }

        let sentinel = MidiEvent::NoteOff {
            channel: channel(16),
            note: note(127),
        };
        let mut output = [sentinel; 256];
        assert_eq!(consumer.drain_into(&mut output), 128);
        assert_eq!(consumer.drain_into(&mut output), 72);
        assert_eq!(consumer.drain_into(&mut output), 0);
    }

    #[test]
    fn lost_counter_saturates_instead_of_wrapping() {
        let lost = AtomicUsize::new(usize::MAX);
        increment_lost(&lost);
        assert_eq!(lost.load(Ordering::Relaxed), usize::MAX);
    }

    #[test]
    fn taking_lost_count_consumes_each_interval_exactly_once() {
        let (mut producer, consumer) = midi_ingress();
        for _ in 0..MIDI_INGRESS_CAPACITY {
            producer.try_push_message(&[0x90, 60, 100]);
        }
        producer.try_push_message(&[0x90, 60, 100]);
        producer.try_push_message(&[0x90, 60, 100]);

        assert_eq!(consumer.take_lost_count(), 2);
        assert_eq!(consumer.take_lost_count(), 0);
        producer.try_push_message(&[0x90, 60, 100]);
        assert_eq!(consumer.take_lost_count(), 1);
        assert_eq!(consumer.take_lost_count(), 0);
    }

    #[test]
    fn saturation_is_consumed_once_and_a_later_overflow_starts_a_fresh_interval() {
        let (mut producer, consumer) = midi_ingress();
        for _ in 0..MIDI_INGRESS_CAPACITY {
            producer.try_push_message(&[0x90, 60, 100]);
        }
        consumer.lost.store(usize::MAX, Ordering::Relaxed);

        assert_eq!(consumer.take_lost_count(), usize::MAX);
        assert_eq!(consumer.take_lost_count(), 0);
        producer.try_push_message(&[0x90, 60, 100]);
        assert_eq!(consumer.take_lost_count(), 1);
    }

    #[test]
    fn producer_overflow_is_forced_onto_both_sides_of_the_taken_interval_boundary() {
        const BEFORE_BOUNDARY: usize = 20_000;
        const AFTER_BOUNDARY: usize = 30_000;
        const ATTEMPTED_OVERFLOWS: usize = 50_000;

        let (mut producer, consumer) = midi_ingress();
        for _ in 0..MIDI_INGRESS_CAPACITY {
            producer.try_push_message(&[0x90, 60, 100]);
        }
        let boundary = Arc::new(Barrier::new(2));
        let producer_boundary = Arc::clone(&boundary);
        let worker = thread::spawn(move || {
            for _ in 0..BEFORE_BOUNDARY {
                producer.try_push_message(&[0x90, 60, 100]);
            }
            producer_boundary.wait();
            producer_boundary.wait();
            for _ in 0..AFTER_BOUNDARY {
                producer.try_push_message(&[0x90, 60, 100]);
            }
        });

        boundary.wait();
        let before_boundary = consumer.take_lost_count();
        boundary.wait();
        worker.join().unwrap();
        let after_boundary = consumer.take_lost_count();

        assert!(before_boundary > 0);
        assert!(after_boundary > 0);
        assert_eq!(before_boundary, BEFORE_BOUNDARY);
        assert_eq!(after_boundary, AFTER_BOUNDARY);
        assert_eq!(before_boundary + after_boundary, ATTEMPTED_OVERFLOWS);
        assert_eq!(consumer.take_lost_count(), 0);
    }

    #[test]
    fn startup_policy_connects_only_one_discovered_port() {
        let now = Instant::now();
        for (ports, expected) in [
            (Vec::new(), None),
            (vec![backend_port("solo", "Solo")], Some("solo")),
            (
                vec![backend_port("left", "Left"), backend_port("right", "Right")],
                None,
            ),
        ] {
            let (mut service, _) = fake_service([Ok(ports)], []);
            service.startup(now).unwrap();
            assert_eq!(
                service.connected_port().map(MidiPortInfo::backend_id),
                expected
            );
        }
    }

    #[test]
    fn refresh_publishes_stable_snapshot_indices_and_preserves_it_on_failure() {
        let first = vec![backend_port("a:1", "Alpha"), backend_port("b:2", "Beta")];
        let (mut service, _) = fake_service(
            [
                Ok(first),
                Err(MidiServiceError::BackendInit("init failed".to_owned())),
                Err(MidiServiceError::PortName {
                    backend_id: "broken".to_owned(),
                    message: "name failed".to_owned(),
                }),
            ],
            [],
        );
        service.refresh_ports().unwrap();
        service.connect(1).unwrap();
        assert_eq!(
            service.ports(),
            &[
                MidiPortInfo::new(0, "a:1", "Alpha"),
                MidiPortInfo::new(1, "b:2", "Beta")
            ]
        );

        assert_eq!(
            service.refresh_ports(),
            Err(MidiServiceError::BackendInit("init failed".to_owned()))
        );
        assert_eq!(service.ports()[1], MidiPortInfo::new(1, "b:2", "Beta"));
        assert_eq!(service.connected_port().unwrap().backend_id(), "b:2");
        assert!(matches!(
            service.refresh_ports(),
            Err(MidiServiceError::PortName { .. })
        ));
        assert_eq!(service.ports()[0], MidiPortInfo::new(0, "a:1", "Alpha"));
        assert_eq!(service.connected_port().unwrap().backend_id(), "b:2");
    }

    #[test]
    fn explicit_replacement_connects_candidate_before_closing_prior_connection() {
        let ports = vec![backend_port("old", "Old"), backend_port("new", "New")];
        let (mut service, state) = fake_service([Ok(ports)], [Ok(()), Ok(())]);
        service.refresh_ports().unwrap();
        service.connect(0).unwrap();
        service.connect(1).unwrap();

        assert_eq!(service.connected_port().unwrap().backend_id(), "new");
        assert_eq!(
            state.lock().unwrap().log,
            [
                "list",
                "connect:old",
                "connect:new",
                "ingress-drop:old",
                "close:old"
            ]
        );
    }

    #[test]
    fn invalid_or_failed_replacement_preserves_a_healthy_connection() {
        let ports = vec![backend_port("old", "Old"), backend_port("new", "New")];
        let failures = [
            MidiServiceError::StalePort("new".to_owned()),
            MidiServiceError::Connect("permission denied".to_owned()),
        ];
        let (mut service, state) = fake_service(
            [Ok(ports)],
            [Ok(()), Err(failures[0].clone()), Err(failures[1].clone())],
        );
        service.refresh_ports().unwrap();
        service.connect(0).unwrap();

        assert_eq!(service.connect(99), Err(MidiServiceError::PortIndex(99)));
        assert_eq!(service.connect(1), Err(failures[0].clone()));
        assert_eq!(service.connect(1), Err(failures[1].clone()));
        assert_eq!(service.connected_port().unwrap().backend_id(), "old");
        assert!(
            !state
                .lock()
                .unwrap()
                .log
                .iter()
                .any(|entry| entry == "close:old")
        );
    }

    #[test]
    fn explicit_disconnect_closes_once_and_clears_event_ingress() {
        let (mut service, state) = fake_service([Ok(vec![backend_port("one", "One")])], [Ok(())]);
        service.refresh_ports().unwrap();
        assert_eq!(service.status(), MidiServiceStatus::Disconnected);
        service.connect(0).unwrap();
        assert_eq!(
            service.status(),
            MidiServiceStatus::Connected(MidiPortInfo::new(0, "one", "One"))
        );
        assert!(service.disconnect());
        assert!(!service.disconnect());
        assert_eq!(service.status(), MidiServiceStatus::Disconnected);
        assert_eq!(service.connected_port(), None);
        assert_eq!(
            state
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|entry| entry.as_str() == "close:one")
                .count(),
            1
        );
    }

    #[test]
    fn maintenance_uses_backend_identity_and_emits_disappearance_once_per_connection() {
        let start = Instant::now();
        let (mut service, state) = fake_service(
            [
                Ok(vec![backend_port("stable-id", "Original name")]),
                Ok(vec![
                    backend_port("other", "Other"),
                    backend_port("stable-id", "Renamed"),
                ]),
                Ok(vec![backend_port("other-id", "Original name")]),
            ],
            [Ok(())],
        );
        service.startup(start).unwrap();
        assert_eq!(service.connected_port().unwrap().name(), "Original name");

        assert_eq!(
            service
                .maintain(start + Duration::from_millis(999))
                .unwrap(),
            None
        );
        assert_eq!(
            state
                .lock()
                .unwrap()
                .log
                .iter()
                .filter(|x| *x == "list")
                .count(),
            1
        );
        assert_eq!(
            service.maintain(start + Duration::from_secs(1)).unwrap(),
            None
        );
        assert_eq!(
            service.connected_port(),
            Some(&MidiPortInfo::new(1, "stable-id", "Renamed"))
        );

        assert_eq!(
            service.maintain(start + Duration::from_secs(2)).unwrap(),
            Some(MidiServiceEvent::PortDisappeared(MidiPortInfo::new(
                1,
                "stable-id",
                "Renamed"
            )))
        );
        assert_eq!(service.connected_port(), None);
        assert_eq!(
            service.maintain(start + Duration::from_secs(3)).unwrap(),
            None
        );
    }

    #[test]
    fn failed_rescan_preserves_connection_and_retries_after_one_second() {
        let start = Instant::now();
        let (mut service, _) = fake_service(
            [
                Ok(vec![backend_port("stable", "Stable")]),
                Err(MidiServiceError::BackendInit("temporary".to_owned())),
                Ok(vec![backend_port("stable", "Stable")]),
            ],
            [Ok(())],
        );
        service.startup(start).unwrap();
        assert_eq!(
            service.maintain(start + Duration::from_secs(1)),
            Err(MidiServiceError::BackendInit("temporary".to_owned()))
        );
        assert_eq!(service.connected_port().unwrap().backend_id(), "stable");
        assert_eq!(
            service.maintain(start + Duration::from_secs(2)).unwrap(),
            None
        );
        assert_eq!(service.connected_port().unwrap().backend_id(), "stable");
    }

    #[test]
    fn service_drop_closes_callback_ingress_before_backend_teardown() {
        let (mut service, state) =
            fake_service([Ok(vec![backend_port("owned", "Owned")])], [Ok(())]);
        service.startup(Instant::now()).unwrap();
        drop(service);
        assert_eq!(
            state.lock().unwrap().log,
            [
                "list",
                "connect:owned",
                "ingress-drop:owned",
                "close:owned",
                "backend-drop"
            ]
        );
    }

    #[test]
    fn backend_callback_data_feeds_the_owned_service_ingress() {
        let (mut service, state) =
            fake_service([Ok(vec![backend_port("events", "Events")])], [Ok(())]);
        state.lock().unwrap().messages_on_connect.push_back(vec![
            vec![0x90, 60, 100],
            vec![0xb0, 1, 127],
            vec![0x80, 60, 0],
        ]);
        service.startup(Instant::now()).unwrap();

        let sentinel = MidiEvent::NoteOff {
            channel: channel(16),
            note: note(127),
        };
        let mut events = [sentinel; 3];
        assert_eq!(service.drain_events(&mut events), 2);
        assert_eq!(
            events[..2],
            [
                MidiEvent::NoteOn {
                    channel: channel(1),
                    note: note(60),
                    velocity: 100,
                },
                MidiEvent::NoteOff {
                    channel: channel(1),
                    note: note(60),
                },
            ]
        );
    }

    #[test]
    fn threaded_callback_quiesces_before_callback_data_and_dependent_state_drop() {
        let trace = Arc::new(Mutex::new(Vec::new()));
        let dependent = AppAudioDependentSentinel::new(Arc::clone(&trace));
        let mut service = MidiService::new(Box::new(ThreadedBackend::new(Arc::clone(&trace))));
        service.startup(Instant::now()).unwrap();

        let sentinel = MidiEvent::NoteOff {
            channel: channel(16),
            note: note(127),
        };
        let mut events = [sentinel; 1];
        assert_eq!(service.drain_events(&mut events), 1);
        assert_eq!(
            events[0],
            MidiEvent::NoteOn {
                channel: channel(1),
                note: note(64),
                velocity: 96,
            }
        );

        drop(service);
        drop(dependent);
        let trace = trace.lock().unwrap();
        assert_eq!(
            trace.as_slice(),
            [
                "backend-list",
                "connect",
                "callback",
                "close-request",
                "callback-thread-stop",
                "callback-thread-joined",
                "callback-data-drop",
                "connection-close",
                "backend-drop",
                "dependent-drop",
            ]
        );
        let stopped = trace
            .iter()
            .position(|event| *event == "callback-thread-stop")
            .unwrap();
        assert!(!trace[stopped + 1..].contains(&"callback"));
    }

    #[test]
    fn real_backend_discovery_smoke_does_not_require_a_physical_port() {
        let mut service = MidiService::new(Box::new(MidirBackend));
        match service.refresh_ports() {
            Ok(()) => {
                for (index, port) in service.ports().iter().enumerate() {
                    assert_eq!(port.index(), index);
                    assert!(!port.backend_id().is_empty());
                }
            }
            Err(MidiServiceError::BackendInit(_)) | Err(MidiServiceError::PortEnumeration(_)) => {}
            Err(error) => panic!("unexpected real MIDI discovery error: {error}"),
        }
    }
}
