#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Arc};

    use sampler_audio::{
        Frame, LiveAck, LiveAckKind, LiveCommandId, PatternSnapshotSlot, PatternSwitch,
        SampleBuffer, SampleSlot, Telemetry, TransportStamp,
    };
    use sampler_core::{PadId, PadSettings, PatternSlotId, PatternSnapshot};

    use crate::AudioPort;

    use super::{MAX_ACKS_PER_MAINTENANCE, PatternStatus, PatternWorkspace};

    #[derive(Default)]
    struct FakeAudio {
        acks: VecDeque<LiveAck>,
        installs: usize,
        backpressured: bool,
    }

    impl AudioPort for FakeAudio {
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
            _pad: PadId,
            _sample: Arc<SampleBuffer>,
            _settings: PadSettings,
        ) -> Result<SampleSlot, String> {
            Err("unused".into())
        }
        fn trigger(&mut self, _pad: PadId, _at: Frame, _velocity: f32) -> Result<(), String> {
            Ok(())
        }
        fn release(&mut self, _pad: PadId, _at: Frame) -> Result<(), String> {
            Ok(())
        }
        fn install_pattern(
            &mut self,
            _snapshot: Arc<PatternSnapshot>,
        ) -> Result<PatternSnapshotSlot, String> {
            self.installs += 1;
            if self.backpressured {
                Err("command queue full".into())
            } else {
                Err("no test snapshot slot".into())
            }
        }
        fn select_pattern(
            &mut self,
            _slot: PatternSlotId,
            _switch: PatternSwitch,
        ) -> Result<(), String> {
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
        fn drain_live_acks(&mut self, output: &mut [LiveAck]) -> usize {
            let mut count = 0;
            while count < output.len() {
                let Some(ack) = self.acks.pop_front() else {
                    break;
                };
                output[count] = ack;
                count += 1;
            }
            count
        }
    }

    fn telemetry() -> Telemetry {
        Telemetry {
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
        }
    }

    fn pad() -> PadId {
        PadId::new(sampler_core::BankId::new(0).unwrap(), 0).unwrap()
    }

    fn key(index: usize) -> usize {
        index
    }

    fn command(value: u64) -> LiveCommandId {
        LiveCommandId::new(value).unwrap()
    }

    fn slot() -> PatternSlotId {
        PatternSlotId::new(0).unwrap()
    }

    fn origin(value: u64) -> TransportStamp {
        TransportStamp {
            slot: slot(),
            generation: 0,
            origin: value,
            loop_frames: 100,
        }
    }

    fn recording_workspace() -> PatternWorkspace {
        let mut workspace = PatternWorkspace::new(100);
        workspace.start_recording(origin(1_000)).unwrap();
        workspace
    }

    fn trigger_ack(id: u64, frame: u64) -> LiveAck {
        LiveAck {
            id: command(id),
            pad: pad(),
            kind: LiveAckKind::Trigger { velocity: 1.0 },
            frame,
            transport: Some(origin(1_000)),
        }
    }

    fn release_ack(id: u64, frame: u64) -> LiveAck {
        LiveAck {
            id: command(id),
            pad: pad(),
            kind: LiveAckKind::Release,
            frame,
            transport: Some(origin(1_000)),
        }
    }

    #[test]
    fn step_toggle_uses_the_swung_grid_and_velocity_is_bounded() {
        let mut workspace = PatternWorkspace::new(48_000);
        workspace.set_swing(0.60).unwrap();
        workspace.move_cursor_to(pad(), 1);
        workspace.toggle_step().unwrap();
        assert_eq!(workspace.selected_event().unwrap().frame, 7_200);
        for _ in 0..30 {
            workspace.adjust_velocity(-0.05).unwrap();
        }
        assert_eq!(workspace.selected_event().unwrap().velocity, 0.0);
    }

    #[test]
    fn trigger_and_release_acks_record_exact_wrapped_duration() {
        let mut workspace = recording_workspace();
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);
        workspace.apply_ack(trigger_ack(7, 1_008));
        workspace.note_live_release(key(0), command(8));
        workspace.apply_ack(release_ack(8, 1_013));
        let event = workspace.selected_pattern().events()[0];
        assert_eq!((event.frame, event.duration), (8, Some(5)));
    }

    #[test]
    fn confirmed_clear_has_one_bounded_undo_checkpoint() {
        let mut workspace = PatternWorkspace::new(48_000);
        for step in 0..4 {
            workspace.move_cursor_to(pad(), step);
            workspace.toggle_step().unwrap();
        }
        workspace.clear_selected().unwrap();
        assert!(workspace.selected_pattern().events().is_empty());
        workspace.undo_clear().unwrap();
        assert_eq!(workspace.selected_pattern().events().len(), 4);
        assert!(workspace.undo_clear().is_err());
    }

    #[test]
    fn stale_transport_generation_ack_is_ignored() {
        let mut workspace = recording_workspace();
        let mut ack = trigger_ack(1, 1_004);
        ack.transport.as_mut().unwrap().generation = 999;
        workspace.apply_ack(ack);
        assert!(workspace.selected_pattern().events().is_empty());
    }

    #[test]
    fn maintenance_bounds_ack_drain_and_retries_the_same_pending_snapshot() {
        let mut workspace = PatternWorkspace::new(48_000);
        let mut audio = FakeAudio {
            backpressured: true,
            ..FakeAudio::default()
        };
        audio
            .acks
            .extend((0..MAX_ACKS_PER_MAINTENANCE + 1).map(|_| LiveAck::EMPTY));

        let first = workspace.maintain(&mut audio, telemetry());
        assert_eq!(first.drained_acks, MAX_ACKS_PER_MAINTENANCE);
        assert_eq!(first.compiled_slot, Some(slot()));
        assert!(workspace.has_pending_snapshot(slot()));
        assert_eq!(
            first.status,
            Some(PatternStatus::SnapshotBackpressured { slot: slot() })
        );

        let second = workspace.maintain(&mut audio, telemetry());
        assert_eq!(second.compiled_slot, Some(PatternSlotId::new(1).unwrap()));
        assert!(workspace.has_pending_snapshot(slot()));
        assert_eq!(audio.installs, 2);
    }

    #[test]
    fn maintenance_compiles_the_newest_dirty_generation_before_pristine_slots() {
        let mut workspace = PatternWorkspace::new(48_000);
        let edited_slot = PatternSlotId::new(3).unwrap();
        workspace.select_slot(edited_slot);
        workspace.toggle_step().unwrap();
        let mut audio = FakeAudio {
            backpressured: true,
            ..FakeAudio::default()
        };

        let maintenance = workspace.maintain(&mut audio, telemetry());

        assert_eq!(maintenance.compiled_slot, Some(edited_slot));
    }
}

use std::{array, sync::Arc};

use sampler_audio::{LiveAck, LiveAckKind, LiveCommandId, Telemetry, TransportStamp};
use sampler_core::{
    BankId, EditablePattern, EventId, Meter, PATTERN_SLOT_COUNT, PadId, PatternCompileError,
    PatternEditError, PatternEvent, PatternSlotId, PatternSnapshot, Resolution, Tempo, Transport,
};

use crate::AudioPort;

pub const MAX_RECORDING_KEYS: usize = 16;
pub const MAX_ACKS_PER_MAINTENANCE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceView {
    Perform,
    Pattern,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternCursor {
    pad: PadId,
    step: u32,
    bar: u16,
}

impl PatternCursor {
    pub fn pad(self) -> PadId {
        self.pad
    }

    pub fn step(self) -> u32 {
        self.step
    }

    pub fn bar(self) -> u16 {
        self.bar
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternStatus {
    UpdatePending { slot: PatternSlotId },
    SnapshotBackpressured { slot: PatternSlotId },
    SnapshotCompileFailed { slot: PatternSlotId, error: String },
    AudioCommandFailed { slot: PatternSlotId, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternMaintenance {
    pub reclaimed_snapshots: usize,
    pub drained_acks: usize,
    pub compiled_slot: Option<PatternSlotId>,
    pub submitted_slot: Option<PatternSlotId>,
    pub status: Option<PatternStatus>,
}

impl PatternMaintenance {
    fn empty() -> Self {
        Self {
            reclaimed_snapshots: 0,
            drained_acks: 0,
            compiled_slot: None,
            submitted_slot: None,
            status: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RecordingIntent {
    stamp: TransportStamp,
}

#[derive(Debug, Clone, Copy)]
struct HeldRecordingKey {
    pad: PadId,
    velocity: f32,
    trigger_id: Option<LiveCommandId>,
    release_id: Option<LiveCommandId>,
    event_id: Option<EventId>,
    trigger_frame: Option<u64>,
}

#[derive(Debug)]
struct PendingSnapshot {
    generation: u64,
    snapshot: Arc<PatternSnapshot>,
}

/// UI-owned editable pattern state. Every callback-facing value is immutable and submitted by
/// [`Self::maintain`], keeping editing and acknowledgement bookkeeping off the audio thread.
#[derive(Debug)]
pub struct PatternWorkspace {
    patterns: Box<[EditablePattern; PATTERN_SLOT_COUNT]>,
    selected_slot: PatternSlotId,
    cursor: PatternCursor,
    selected_event: Option<EventId>,
    view: WorkspaceView,
    playing: bool,
    recording: Option<RecordingIntent>,
    held_keys: [Option<HeldRecordingKey>; MAX_RECORDING_KEYS],
    dirty_generations: [Option<u64>; PATTERN_SLOT_COUNT],
    pending_snapshots: [Option<PendingSnapshot>; PATTERN_SLOT_COUNT],
    reinstall_pending: [bool; PATTERN_SLOT_COUNT],
    installed_generations: [Option<u64>; PATTERN_SLOT_COUNT],
    last_status: Option<PatternStatus>,
}

impl PatternWorkspace {
    pub fn new(sample_rate: u32) -> Self {
        let tempo = Tempo::new(120.0).expect("default tempo is valid");
        let meter = Meter::new(4, 4).expect("default meter is valid");
        let slot = PatternSlotId::new(0).expect("first pattern slot is valid");
        let pad = PadId::new(BankId::new(0).expect("first bank is valid"), 0)
            .expect("first pad is valid");
        let patterns = array::from_fn(|index| {
            let slot = PatternSlotId::new(u8::try_from(index).expect("slot index fits in u8"))
                .expect("pattern slot index is valid");
            let transport = Transport::new(sample_rate, tempo, meter, 1, Resolution::Sixteenth)
                .expect("audio sample rate must be non-zero");
            EditablePattern::new(slot, format!("Pattern {:02}", index + 1), transport)
                .expect("default pattern is valid")
        });
        Self {
            patterns: Box::new(patterns),
            selected_slot: slot,
            cursor: PatternCursor {
                pad,
                step: 0,
                bar: 0,
            },
            selected_event: None,
            view: WorkspaceView::Perform,
            playing: false,
            recording: None,
            held_keys: [None; MAX_RECORDING_KEYS],
            dirty_generations: array::from_fn(|_| Some(0)),
            pending_snapshots: array::from_fn(|_| None),
            reinstall_pending: [true; PATTERN_SLOT_COUNT],
            installed_generations: [None; PATTERN_SLOT_COUNT],
            last_status: None,
        }
    }

    pub fn view(&self) -> WorkspaceView {
        self.view
    }

    pub fn set_view(&mut self, view: WorkspaceView) {
        self.view = view;
    }

    pub fn toggle_view(&mut self) {
        self.view = match self.view {
            WorkspaceView::Perform => WorkspaceView::Pattern,
            WorkspaceView::Pattern => WorkspaceView::Perform,
        };
    }

    pub fn selected_slot(&self) -> PatternSlotId {
        self.selected_slot
    }

    pub fn select_slot(&mut self, slot: PatternSlotId) {
        self.selected_slot = slot;
        self.selected_event = None;
        self.clamp_cursor();
    }

    pub fn selected_pattern(&self) -> &EditablePattern {
        &self.patterns[self.slot_index()]
    }

    pub fn pattern(&self, slot: PatternSlotId) -> &EditablePattern {
        &self.patterns[usize::from(slot.get())]
    }

    pub fn cursor(&self) -> PatternCursor {
        self.cursor
    }

    pub fn move_cursor_to(&mut self, pad: PadId, step: u32) {
        self.cursor.pad = pad;
        self.cursor.step = step;
        self.clamp_cursor();
        self.refresh_selected_event();
    }

    pub fn move_cursor_steps(&mut self, delta: i32) {
        let count = self.selected_pattern().transport().step_count();
        let step = i64::from(self.cursor.step)
            .saturating_add(i64::from(delta))
            .clamp(0, i64::from(count.saturating_sub(1)));
        self.cursor.step = u32::try_from(step).expect("clamped step fits in u32");
        self.cursor.bar = self.cursor_bar();
        self.refresh_selected_event();
    }

    pub fn move_cursor_bar(&mut self, delta: i32) {
        let bars = self.selected_pattern().transport().bars();
        let bar = i32::from(self.cursor.bar)
            .saturating_add(delta)
            .clamp(0, i32::from(bars.saturating_sub(1)));
        self.cursor.bar = u16::try_from(bar).expect("clamped bar fits in u16");
        self.refresh_selected_event();
    }

    pub fn selected_event(&self) -> Option<&PatternEvent> {
        self.selected_event
            .and_then(|event_id| self.selected_pattern().event(event_id))
    }

    pub fn toggle_step(&mut self) -> Result<(), PatternEditError> {
        let raw_frame = self
            .selected_pattern()
            .transport()
            .step_frame(self.cursor.step);
        let index = self.slot_index();
        let event = self.patterns[index].toggle_at(self.cursor.pad, raw_frame, 1.0)?;
        self.selected_event = event;
        self.mark_dirty(index);
        Ok(())
    }

    pub fn delete_step(&mut self) -> Result<(), PatternEditError> {
        let Some(event_id) = self.selected_event else {
            return Ok(());
        };
        let index = self.slot_index();
        self.patterns[index].remove(event_id)?;
        self.selected_event = None;
        self.mark_dirty(index);
        Ok(())
    }

    pub fn adjust_velocity(&mut self, delta: f32) -> Result<(), PatternEditError> {
        if !delta.is_finite() {
            return Err(PatternEditError::InvalidVelocity);
        }
        let Some(event_id) = self.selected_event else {
            return Ok(());
        };
        let velocity = self
            .selected_pattern()
            .event(event_id)
            .expect("selected event belongs to the selected pattern")
            .velocity;
        let index = self.slot_index();
        self.patterns[index].set_velocity(event_id, (velocity + delta).clamp(0.0, 1.0))?;
        self.mark_dirty(index);
        Ok(())
    }

    pub fn set_tempo(&mut self, tempo: Tempo) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].set_tempo(tempo)?;
        self.mark_dirty(index);
        self.clamp_cursor();
        self.refresh_selected_event();
        Ok(())
    }

    pub fn set_bars(&mut self, bars: u16) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].set_bars(bars)?;
        self.mark_dirty(index);
        self.clamp_cursor();
        self.refresh_selected_event();
        Ok(())
    }

    pub fn set_resolution(&mut self, resolution: Resolution) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].set_resolution(resolution)?;
        self.mark_dirty(index);
        self.clamp_cursor();
        self.refresh_selected_event();
        Ok(())
    }

    pub fn set_swing(&mut self, swing: f64) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].set_swing(swing)?;
        self.mark_dirty(index);
        self.refresh_selected_event();
        Ok(())
    }

    pub fn set_quantize(&mut self, strength: f32) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].set_quantize_strength(strength)?;
        self.mark_dirty(index);
        self.refresh_selected_event();
        Ok(())
    }

    pub fn clear_selected(&mut self) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].clear()?;
        self.selected_event = None;
        self.stop_recording();
        self.mark_dirty(index);
        Ok(())
    }

    pub fn undo_clear(&mut self) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].undo_clear()?;
        self.mark_dirty(index);
        self.refresh_selected_event();
        Ok(())
    }

    pub fn start_recording(&mut self, stamp: TransportStamp) -> Result<(), PatternEditError> {
        if stamp.slot != self.selected_slot || stamp.loop_frames == 0 {
            return Err(PatternEditError::InvalidSlot);
        }
        self.recording = Some(RecordingIntent { stamp });
        self.held_keys.fill(None);
        Ok(())
    }

    pub fn stop_recording(&mut self) {
        self.recording = None;
        self.held_keys.fill(None);
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    pub fn record_capture(&self) -> Option<(PatternSlotId, u64)> {
        self.recording
            .map(|intent| (intent.stamp.slot, intent.stamp.generation))
    }

    pub fn note_live_trigger(
        &mut self,
        key: usize,
        command: LiveCommandId,
        pad: PadId,
        velocity: f32,
    ) {
        let Some(entry) = self.held_keys.get_mut(key) else {
            return;
        };
        *entry = Some(HeldRecordingKey {
            pad,
            velocity: velocity.clamp(0.0, 1.0),
            trigger_id: Some(command),
            release_id: None,
            event_id: None,
            trigger_frame: None,
        });
    }

    pub fn note_live_release(&mut self, key: usize, command: LiveCommandId) {
        if let Some(Some(entry)) = self.held_keys.get_mut(key) {
            entry.release_id = Some(command);
        }
    }

    pub fn pending_trigger_id(&self, key: usize) -> Option<LiveCommandId> {
        self.held_keys
            .get(key)
            .and_then(|entry| entry.as_ref())
            .and_then(|entry| entry.trigger_id)
    }

    pub fn apply_ack(&mut self, ack: LiveAck) {
        let Some(intent) = self.recording else {
            return;
        };
        let Some(stamp) = ack.transport else {
            return;
        };
        if stamp.slot != intent.stamp.slot
            || stamp.generation != intent.stamp.generation
            || stamp.origin != intent.stamp.origin
            || stamp.loop_frames != intent.stamp.loop_frames
        {
            return;
        }

        let Some(key) = self.held_keys.iter().position(|entry| {
            entry.is_some_and(|entry| {
                entry.trigger_id == Some(ack.id) || entry.release_id == Some(ack.id)
            })
        }) else {
            return;
        };
        let mut entry = self.held_keys[key].expect("the matching held key exists");
        match ack.kind {
            LiveAckKind::Trigger { velocity } if entry.trigger_id == Some(ack.id) => {
                if ack.pad != entry.pad {
                    return;
                }
                let frame = ack.frame.wrapping_sub(stamp.origin) % stamp.loop_frames;
                let index = usize::from(stamp.slot.get());
                let velocity = if velocity.is_finite() {
                    velocity.clamp(0.0, 1.0)
                } else {
                    entry.velocity
                };
                if let Ok(event_id) =
                    self.patterns[index].insert_new(entry.pad, frame, velocity, None)
                {
                    entry.event_id = Some(event_id);
                    entry.trigger_frame = Some(frame);
                    self.held_keys[key] = Some(entry);
                    self.selected_event = Some(event_id);
                    self.mark_dirty(index);
                }
            }
            LiveAckKind::Release if entry.release_id == Some(ack.id) => {
                let (Some(event_id), Some(trigger_frame)) = (entry.event_id, entry.trigger_frame)
                else {
                    return;
                };
                let release_frame = ack.frame.wrapping_sub(stamp.origin) % stamp.loop_frames;
                let duration = release_frame
                    .wrapping_add(stamp.loop_frames)
                    .wrapping_sub(trigger_frame)
                    % stamp.loop_frames;
                if duration == 0 {
                    self.held_keys[key] = None;
                    return;
                }
                let index = usize::from(stamp.slot.get());
                let Some(event) = self.patterns[index].event(event_id).copied() else {
                    self.held_keys[key] = None;
                    return;
                };
                let _ = self.patterns[index].remove(event_id);
                if self.patterns[index]
                    .insert(
                        PatternEvent::new(
                            event_id,
                            event.pad,
                            event.frame,
                            event.velocity,
                            Some(duration),
                        )
                        .expect("recorded duration is valid"),
                    )
                    .is_ok()
                {
                    self.selected_event = Some(event_id);
                    self.mark_dirty(index);
                }
                self.held_keys[key] = None;
            }
            _ => {}
        }
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn last_status(&self) -> Option<&PatternStatus> {
        self.last_status.as_ref()
    }

    pub fn has_pending_snapshot(&self, slot: PatternSlotId) -> bool {
        self.pending_snapshots[usize::from(slot.get())].is_some()
    }

    pub fn needs_reinstall(&self, slot: PatternSlotId) -> bool {
        self.reinstall_pending[usize::from(slot.get())]
    }

    pub fn rebuild_sample_rate(&mut self, sample_rate: u32) -> Result<(), PatternEditError> {
        for index in 0..PATTERN_SLOT_COUNT {
            self.patterns[index].rebuild_sample_rate(sample_rate)?;
            self.mark_dirty(index);
            self.reinstall_pending[index] = true;
        }
        self.clamp_cursor();
        self.refresh_selected_event();
        Ok(())
    }

    pub fn maintain(
        &mut self,
        audio: &mut dyn AudioPort,
        telemetry: Telemetry,
    ) -> PatternMaintenance {
        let mut result = PatternMaintenance::empty();
        result.reclaimed_snapshots = audio.reclaim_retired_patterns();

        let mut acks = [LiveAck::EMPTY; MAX_ACKS_PER_MAINTENANCE];
        result.drained_acks = audio.drain_live_acks(&mut acks).min(acks.len());
        for ack in acks.into_iter().take(result.drained_acks) {
            self.apply_ack(ack);
        }

        self.playing = telemetry.pattern_playing;
        if !telemetry.pattern_recording && self.recording.is_some() {
            self.stop_recording();
        }

        if let Some((index, generation)) = self.next_dirty_slot() {
            let slot = self.patterns[index].slot();
            match self.patterns[index].compile() {
                Ok(snapshot) => {
                    if self.patterns[index].generation() == generation {
                        self.pending_snapshots[index] = Some(PendingSnapshot {
                            generation,
                            snapshot: Arc::new(snapshot),
                        });
                        self.dirty_generations[index] = None;
                        result.compiled_slot = Some(slot);
                    }
                }
                Err(error) => {
                    let status = PatternStatus::SnapshotCompileFailed {
                        slot,
                        error: compile_error_text(error),
                    };
                    self.last_status = Some(status.clone());
                    result.status = Some(status);
                    return result;
                }
            }
        }

        if let Some(index) = self.next_pending_slot() {
            let slot = self.patterns[index].slot();
            let pending = self.pending_snapshots[index]
                .as_ref()
                .expect("selected pending slot holds a snapshot");
            if self.patterns[index].generation() != pending.generation {
                self.pending_snapshots[index] = None;
                self.dirty_generations[index] = Some(self.patterns[index].generation());
                let status = PatternStatus::UpdatePending { slot };
                self.last_status = Some(status.clone());
                result.status = Some(status);
                return result;
            }
            match audio.install_pattern(Arc::clone(&pending.snapshot)) {
                Ok(_) => {
                    self.installed_generations[index] = Some(pending.generation);
                    self.pending_snapshots[index] = None;
                    self.reinstall_pending[index] = false;
                    result.submitted_slot = Some(slot);
                    self.last_status = None;
                }
                Err(error) => {
                    let status = if error.contains("queue") || error.contains("full") {
                        PatternStatus::SnapshotBackpressured { slot }
                    } else {
                        PatternStatus::AudioCommandFailed { slot, error }
                    };
                    self.last_status = Some(status.clone());
                    result.status = Some(status);
                }
            }
        } else if let Some((index, _)) = self.next_dirty_slot() {
            let status = PatternStatus::UpdatePending {
                slot: self.patterns[index].slot(),
            };
            self.last_status = Some(status.clone());
            result.status = Some(status);
        }
        result
    }

    fn slot_index(&self) -> usize {
        usize::from(self.selected_slot.get())
    }

    fn mark_dirty(&mut self, index: usize) {
        let generation = self.patterns[index].generation();
        self.dirty_generations[index] = Some(generation);
        self.reinstall_pending[index] = true;
        if self.pending_snapshots[index]
            .as_ref()
            .is_some_and(|pending| pending.generation != generation)
        {
            self.pending_snapshots[index] = None;
        }
    }

    fn next_dirty_slot(&self) -> Option<(usize, u64)> {
        self.dirty_generations
            .iter()
            .enumerate()
            .filter_map(|(index, generation)| generation.map(|generation| (index, generation)))
            .fold(None, |best, candidate| match best {
                Some((_, generation)) if generation >= candidate.1 => best,
                _ => Some(candidate),
            })
    }

    fn next_pending_slot(&self) -> Option<usize> {
        self.pending_snapshots.iter().position(Option::is_some)
    }

    fn clamp_cursor(&mut self) {
        let transport = self.selected_pattern().transport();
        self.cursor.step = self
            .cursor
            .step
            .min(transport.step_count().saturating_sub(1));
        self.cursor.bar = self.cursor_bar().min(transport.bars().saturating_sub(1));
    }

    fn cursor_bar(&self) -> u16 {
        let transport = self.selected_pattern().transport();
        let steps_per_bar = (transport.step_count() / u32::from(transport.bars())).max(1);
        u16::try_from(self.cursor.step / steps_per_bar).unwrap_or(u16::MAX)
    }

    fn refresh_selected_event(&mut self) {
        let raw_frame = self
            .selected_pattern()
            .transport()
            .step_frame(self.cursor.step);
        self.selected_event = self.selected_pattern().events().iter().find_map(|event| {
            (event.pad == self.cursor.pad && event.frame == raw_frame).then_some(event.id)
        });
    }
}

fn compile_error_text(error: PatternCompileError) -> String {
    error.to_string()
}
