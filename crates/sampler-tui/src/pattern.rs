#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Arc};

    use sampler_audio::{
        AudioController, AudioEngine, Frame, LiveAck, LiveAckKind, LiveCommandId,
        PatternSnapshotSlot, PatternSwitch, SampleBuffer, SampleSlot, Telemetry, TransportStamp,
        audio_channels_with_test_capacities,
    };
    use sampler_core::{
        PATTERN_SLOT_COUNT, PadId, PadSettings, PatternSlotId, PatternSnapshot, Tempo,
    };

    use crate::{AudioPort, CaptureSupport};

    use super::{
        MAX_ACKS_PER_MAINTENANCE, PatternCaptureState, PatternStatus, PatternWorkspace,
        RecordingIntent, RecordingState,
    };

    #[derive(Default)]
    struct FakeAudio {
        acks: VecDeque<LiveAck>,
        installs: usize,
        backpressured: bool,
        capture_error: bool,
        capture_attempts: usize,
    }

    impl AudioPort for FakeAudio {
        fn capture_support(&self) -> CaptureSupport {
            CaptureSupport::Unsupported
        }

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
            _mix: sampler_core::PadMixSettings,
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
        fn set_record_capture(
            &mut self,
            _capture: Option<(PatternSlotId, u64)>,
        ) -> Result<(), String> {
            self.capture_attempts += 1;
            if self.capture_error {
                Err("command queue full".into())
            } else {
                Ok(())
            }
        }
    }

    struct OneSlotAudio {
        controller: AudioController,
        engine: AudioEngine,
    }

    impl OneSlotAudio {
        fn new() -> Self {
            let (controller, ports) = audio_channels_with_test_capacities(1, 256, 64);
            Self {
                controller,
                engine: AudioEngine::new(100, ports).unwrap(),
            }
        }

        fn callback(&mut self) {
            self.engine.render_frames(0, |_| {});
        }
    }

    impl AudioPort for OneSlotAudio {
        fn capture_support(&self) -> CaptureSupport {
            CaptureSupport::Unsupported
        }

        fn sample_rate(&self) -> u32 {
            100
        }
        fn channels(&self) -> u16 {
            2
        }
        fn render_horizon(&self) -> Frame {
            self.controller.render_horizon()
        }
        fn install(
            &mut self,
            _pad: PadId,
            _sample: Arc<SampleBuffer>,
            _settings: PadSettings,
            _mix: sampler_core::PadMixSettings,
        ) -> Result<SampleSlot, String> {
            Err("unused".into())
        }
        fn trigger(&mut self, _pad: PadId, _at: Frame, _velocity: f32) -> Result<(), String> {
            Err("unused".into())
        }
        fn release(&mut self, _pad: PadId, _at: Frame) -> Result<(), String> {
            Err("unused".into())
        }
        fn install_pattern(
            &mut self,
            snapshot: Arc<PatternSnapshot>,
        ) -> Result<PatternSnapshotSlot, String> {
            self.controller
                .install_pattern(snapshot)
                .map_err(|error| error.to_string())
        }
        fn select_pattern(
            &mut self,
            _slot: PatternSlotId,
            _switch: PatternSwitch,
        ) -> Result<(), String> {
            Err("unused".into())
        }
        fn set_record_capture(
            &mut self,
            capture: Option<(PatternSlotId, u64)>,
        ) -> Result<(), String> {
            self.controller
                .set_record_capture(capture)
                .map_err(|error| error.to_string())
        }
        fn stop_pad(&mut self, _pad: PadId) -> Result<(), String> {
            Err("unused".into())
        }
        fn stop_all(&mut self) -> Result<(), String> {
            Err("unused".into())
        }
        fn update_pad(&mut self, _pad: PadId, _settings: PadSettings) -> Result<(), String> {
            Err("unused".into())
        }
        fn update_pad_mix(
            &mut self,
            _pad: PadId,
            _settings: sampler_core::PadMixSettings,
        ) -> Result<(), String> {
            Err("unused".into())
        }
        fn update_master_mix(
            &mut self,
            _settings: sampler_core::MasterMixSettings,
        ) -> Result<(), String> {
            Err("unused".into())
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

    fn recording_telemetry(stamp: TransportStamp) -> Telemetry {
        let mut telemetry = telemetry();
        telemetry.pattern_slot = Some(stamp.slot);
        telemetry.pattern_generation = Some(stamp.generation);
        telemetry.pattern_origin = Some(stamp.origin);
        telemetry.pattern_recording = true;
        telemetry
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

    #[test]
    fn pending_capture_accepts_its_first_exact_ack_before_telemetry_establishes_origin() {
        let mut workspace = PatternWorkspace::new(100);
        workspace.start_recording(origin(0)).unwrap();
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);

        workspace.apply_ack(trigger_ack(7, 1_008));

        assert_eq!(workspace.selected_pattern().events().len(), 1);
        assert_eq!(workspace.selected_pattern().events()[0].frame, 8);
        assert_eq!(workspace.pending_trigger_id(key(0)), Some(command(7)));
    }

    #[test]
    fn pending_capture_confirms_exact_origin_when_ack_and_telemetry_arrive_together() {
        let mut workspace = PatternWorkspace::new(100);
        workspace.start_recording(origin(0)).unwrap();
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);
        let mut audio = FakeAudio::default();
        audio.acks.push_back(trigger_ack(7, 1_008));

        workspace.maintain(&mut audio, recording_telemetry(origin(1_000)));

        assert_eq!(workspace.selected_pattern().events().len(), 1);
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Confirmed)
        );
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
    fn toggle_removes_an_event_in_the_same_swung_display_cell() {
        let mut workspace = PatternWorkspace::new(48_000);
        workspace.move_cursor_to(pad(), 1);
        workspace.toggle_step().unwrap();
        workspace.set_swing(0.60).unwrap();
        workspace.set_quantize(1.0).unwrap();

        workspace.toggle_step().unwrap();

        assert!(workspace.selected_pattern().events().is_empty());
    }

    #[test]
    fn delete_removes_the_selected_event_in_a_partially_quantized_display_cell() {
        let mut workspace = PatternWorkspace::new(48_000);
        workspace.move_cursor_to(pad(), 1);
        workspace.toggle_step().unwrap();
        workspace.set_swing(0.60).unwrap();
        workspace.set_quantize(0.5).unwrap();

        workspace.delete_step().unwrap();

        assert!(workspace.selected_pattern().events().is_empty());
    }

    #[test]
    fn an_edit_is_pending_until_its_current_generation_is_admitted() {
        let mut workspace = PatternWorkspace::new(48_000);
        workspace.move_cursor_to(pad(), 0);
        workspace.toggle_step().unwrap();

        assert!(workspace.updates_pending(slot()));
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
    fn midi_task5_retrigger_keeps_retiring_ack_pair_until_duration_is_committed() {
        let mut workspace = recording_workspace();
        workspace.note_live_trigger(key(16), command(7), pad(), 0.5);
        workspace.note_live_release(key(16), command(8));
        workspace.note_live_trigger(key(16), command(9), pad(), 0.75);

        assert!(workspace.apply_ack(trigger_ack(7, 1_008)));
        assert!(workspace.apply_ack(release_ack(8, 1_013)));
        assert!(workspace.apply_ack(trigger_ack(9, 1_020)));

        let events = workspace.selected_pattern().events();
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].frame, events[0].duration), (8, Some(5)));
        assert_eq!((events[1].frame, events[1].duration), (20, None));
    }

    #[test]
    fn live_ack_overflow_discards_lost_release_event_and_reuses_correlations() {
        let mut workspace = recording_workspace();
        workspace.note_live_trigger(key(16), command(7), pad(), 0.5);
        assert!(workspace.apply_ack(trigger_ack(7, 1_008)));
        workspace.note_live_release(key(16), command(8));
        workspace.note_live_trigger(key(16), command(9), pad(), 0.75);

        let mut overflow = recording_telemetry(origin(1_000));
        overflow.live_ack_overflows = 1;
        workspace.maintain(&mut FakeAudio::default(), overflow);

        assert!(workspace.selected_pattern().events().is_empty());
        assert!(
            workspace.export_project_patterns().unwrap()[0]
                .events
                .is_empty()
        );
        assert!(workspace.held_keys.iter().all(Option::is_none));
        assert!(workspace.retiring_keys.iter().all(Option::is_none));

        workspace.note_live_trigger(key(16), command(10), pad(), 1.0);
        assert!(workspace.apply_ack(trigger_ack(10, 1_020)));
        assert_eq!(workspace.selected_pattern().events().len(), 1);
    }

    #[test]
    fn repeated_live_ack_overflows_keep_retrigger_capacity_reusable() {
        let mut workspace = recording_workspace();
        let mut audio = FakeAudio::default();

        for cycle in 0..3_u64 {
            let trigger = 20 + cycle * 3;
            workspace.note_live_trigger(key(16), command(trigger), pad(), 1.0);
            assert!(workspace.apply_ack(trigger_ack(trigger, 1_010 + cycle)));
            workspace.note_live_release(key(16), command(trigger + 1));
            workspace.note_live_trigger(key(16), command(trigger + 2), pad(), 1.0);

            let mut overflow = recording_telemetry(origin(1_000));
            overflow.live_ack_overflows = cycle + 1;
            workspace.maintain(&mut audio, overflow);

            assert!(workspace.selected_pattern().events().is_empty());
            assert!(workspace.held_keys.iter().all(Option::is_none));
            assert!(workspace.retiring_keys.iter().all(Option::is_none));
        }
    }

    #[test]
    fn four_rapid_retriggers_before_any_ack_commit_in_engine_fifo_order() {
        let mut workspace = recording_workspace();
        for (trigger, release) in [(30, 31), (32, 33), (34, 35)] {
            workspace.note_live_trigger(key(16), command(trigger), pad(), 1.0);
            workspace.note_live_release(key(16), command(release));
        }
        workspace.note_live_trigger(key(16), command(36), pad(), 1.0);

        for (trigger, release, frame) in [(30, 31, 1_010), (32, 33, 1_020), (34, 35, 1_030)] {
            assert!(workspace.apply_ack(trigger_ack(trigger, frame)));
            assert!(workspace.apply_ack(release_ack(release, frame + 5)));
        }
        assert!(workspace.apply_ack(trigger_ack(36, 1_040)));

        let events = workspace.selected_pattern().events();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].duration, Some(5));
        assert_eq!(events[1].duration, Some(5));
        assert_eq!(events[2].duration, Some(5));
        assert_eq!(events[3].duration, None);
    }

    #[test]
    fn one_shot_ack_records_no_duration_and_drops_its_release_correlation() {
        let mut workspace = recording_workspace();
        workspace.note_live_trigger_with_duration(key(0), command(7), pad(), 1.0, false);
        workspace.apply_ack(trigger_ack(7, 1_008));

        assert_eq!(workspace.selected_pattern().events()[0].duration, None);
        assert_eq!(workspace.pending_trigger_id(key(0)), None);
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

    #[test]
    fn release_ack_for_an_exact_loop_hold_keeps_a_full_loop_duration() {
        let mut workspace = recording_workspace();
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);
        workspace.apply_ack(trigger_ack(7, 1_008));
        workspace.note_live_release(key(0), command(8));
        workspace.apply_ack(release_ack(8, 1_108));

        assert_eq!(workspace.selected_pattern().events()[0].duration, Some(100));
    }

    #[test]
    fn release_ack_longer_than_one_loop_is_capped_to_the_core_duration_limit() {
        let mut workspace = recording_workspace();
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);
        workspace.apply_ack(trigger_ack(7, 1_008));
        workspace.note_live_release(key(0), command(8));
        workspace.apply_ack(release_ack(8, 1_308));

        assert_eq!(workspace.selected_pattern().events()[0].duration, Some(100));
    }

    #[test]
    fn off_slot_record_ack_never_corrupts_current_selection_or_panics_edits() {
        let mut workspace = recording_workspace();
        let other_slot = PatternSlotId::new(1).unwrap();
        workspace.select_slot(other_slot);
        workspace.toggle_step().unwrap();
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);
        workspace.apply_ack(trigger_ack(7, 1_008));

        workspace.adjust_velocity(-0.25).unwrap();

        assert_eq!(workspace.selected_slot(), other_slot);
        assert_eq!(workspace.selected_event().unwrap().velocity, 0.75);
    }

    #[test]
    fn recording_waits_for_an_exact_telemetry_confirmation_and_disarms_causally() {
        let mut workspace = PatternWorkspace::new(100);
        let stamp = origin(1_000);
        workspace.start_recording(stamp).unwrap();
        let mut audio = FakeAudio::default();

        workspace.maintain(&mut audio, telemetry());
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Pending)
        );

        let mut stale = recording_telemetry(stamp);
        stale.pattern_generation = Some(99);
        workspace.maintain(&mut audio, stale);
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Pending)
        );

        workspace.maintain(&mut audio, recording_telemetry(stamp));
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Confirmed)
        );

        workspace.stop_recording();
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Disarming)
        );
        workspace.maintain(&mut audio, telemetry());
        assert_eq!(workspace.capture_state(), None);
    }

    #[test]
    fn confirmed_capture_clears_on_current_false_or_replacement_telemetry() {
        let stamp = origin(1_000);
        let mut workspace = PatternWorkspace::new(100);
        let mut audio = FakeAudio::default();
        workspace.start_recording(stamp).unwrap();
        workspace.maintain(&mut audio, recording_telemetry(stamp));
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Confirmed)
        );

        workspace.maintain(&mut audio, telemetry());
        assert_eq!(workspace.capture_state(), None);

        workspace.start_recording(stamp).unwrap();
        workspace.maintain(&mut audio, recording_telemetry(stamp));
        let mut replacement = recording_telemetry(stamp);
        replacement.pattern_origin = Some(stamp.origin + 100);
        workspace.maintain(&mut audio, replacement);
        assert_eq!(workspace.capture_state(), None);
    }

    #[test]
    fn stale_confirmed_generation_telemetry_preserves_rearming_capture_and_held_key() {
        let previous = origin(1_000);
        let target = TransportStamp {
            generation: 2,
            ..previous
        };
        let mut workspace = PatternWorkspace::new(100);
        let mut audio = FakeAudio::default();
        workspace.start_recording(previous).unwrap();
        workspace.maintain(&mut audio, recording_telemetry(previous));
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);
        workspace.recording = Some(RecordingState::Rearming {
            previous: RecordingIntent { stamp: previous },
            target: RecordingIntent { stamp: target },
            capture_command_pending: false,
        });

        workspace.maintain(&mut audio, recording_telemetry(previous));

        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Pending)
        );
        assert_eq!(workspace.pending_trigger_id(key(0)), Some(command(7)));
        workspace.maintain(&mut audio, recording_telemetry(target));
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Confirmed)
        );
        assert_eq!(workspace.record_capture(), Some((slot(), 2)));
    }

    #[test]
    fn different_live_slot_invalidates_rearming_and_cannot_confirm_when_switching_back() {
        let previous = origin(1_000);
        let target = TransportStamp {
            generation: 2,
            ..previous
        };
        let mut workspace = PatternWorkspace::new(100);
        let mut audio = FakeAudio::default();
        workspace.recording = Some(RecordingState::Rearming {
            previous: RecordingIntent { stamp: previous },
            target: RecordingIntent { stamp: target },
            capture_command_pending: false,
        });
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);
        workspace.dirty_patterns.fill(None);
        let mut switched = telemetry();
        switched.pattern_playing = true;
        switched.pattern_slot = Some(PatternSlotId::new(1).unwrap());

        workspace.maintain(&mut audio, switched);

        assert!(!workspace.is_recording());
        assert_eq!(workspace.record_capture(), None);
        assert_eq!(workspace.pending_trigger_id(key(0)), None);
        workspace.maintain(&mut audio, recording_telemetry(target));
        assert!(!workspace.is_recording());
        assert_eq!(workspace.capture_state(), None);
    }

    #[test]
    fn rearming_capture_retries_once_per_maintenance_after_admission_backpressure() {
        let previous = origin(1_000);
        let target = TransportStamp {
            generation: 2,
            ..previous
        };
        let mut workspace = PatternWorkspace::new(100);
        let mut audio = FakeAudio {
            capture_error: true,
            ..FakeAudio::default()
        };
        workspace.recording = Some(RecordingState::Rearming {
            previous: RecordingIntent { stamp: previous },
            target: RecordingIntent { stamp: target },
            capture_command_pending: true,
        });
        workspace.dirty_patterns.fill(None);

        workspace.maintain(&mut audio, recording_telemetry(previous));
        assert_eq!(audio.capture_attempts, 1);
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Pending)
        );
        audio.capture_error = false;
        workspace.maintain(&mut audio, recording_telemetry(previous));
        assert_eq!(audio.capture_attempts, 2);
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Pending)
        );
        workspace.maintain(&mut audio, recording_telemetry(target));
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Confirmed)
        );
    }

    #[test]
    fn one_slot_queue_retries_rearm_after_install_consumes_admission() {
        let previous = origin(1_000);
        let mut workspace = PatternWorkspace::new(100);
        let mut audio = OneSlotAudio::new();
        workspace.dirty_patterns.fill(None);
        workspace.start_recording(previous).unwrap();
        workspace.maintain(&mut audio, recording_telemetry(previous));
        workspace.move_cursor_to(pad(), 0);
        workspace.toggle_step().unwrap();

        workspace.maintain(&mut audio, recording_telemetry(previous));
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Pending)
        );
        assert_eq!(workspace.record_capture(), Some((slot(), 1)));

        audio.callback(); // drains InstallPattern, making one command slot available.
        workspace.maintain(&mut audio, recording_telemetry(previous));
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Pending)
        );
        audio.callback(); // drains the retry SetRecordCapture.
        let target = TransportStamp {
            generation: 1,
            ..previous
        };
        workspace.maintain(&mut audio, recording_telemetry(target));
        assert_eq!(
            workspace.capture_state(),
            Some(PatternCaptureState::Confirmed)
        );
    }

    #[test]
    fn invalidating_a_confirmed_capture_clears_all_held_correlations() {
        let stamp = origin(1_000);
        let mut workspace = PatternWorkspace::new(100);
        let mut audio = FakeAudio::default();
        workspace.start_recording(stamp).unwrap();
        workspace.maintain(&mut audio, recording_telemetry(stamp));
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);

        workspace.maintain(&mut audio, telemetry());

        assert_eq!(workspace.capture_state(), None);
        assert_eq!(workspace.pending_trigger_id(key(0)), None);

        workspace.start_recording(stamp).unwrap();
        workspace.maintain(&mut audio, recording_telemetry(stamp));
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);
        let mut replacement = recording_telemetry(stamp);
        replacement.pattern_origin = Some(stamp.origin + 100);
        workspace.maintain(&mut audio, replacement);

        assert_eq!(workspace.capture_state(), None);
        assert_eq!(workspace.pending_trigger_id(key(0)), None);
    }

    #[test]
    fn moving_bar_keeps_the_local_column_and_updates_absolute_step() {
        let mut workspace = PatternWorkspace::new(48_000);
        workspace.set_bars(2).unwrap();
        workspace.move_cursor_to(pad(), 5);

        workspace.move_cursor_bar(1);

        assert_eq!(
            (workspace.cursor().bar(), workspace.cursor().step()),
            (1, 21)
        );
    }

    #[test]
    fn latest_dirty_ticket_wins_even_when_an_older_slot_has_a_higher_generation() {
        let mut workspace = PatternWorkspace::new(48_000);
        let first = PatternSlotId::new(0).unwrap();
        let second = PatternSlotId::new(1).unwrap();
        workspace.select_slot(first);
        workspace.toggle_step().unwrap();
        workspace.toggle_step().unwrap();
        workspace.select_slot(second);
        workspace.toggle_step().unwrap();
        let mut audio = FakeAudio {
            backpressured: true,
            ..FakeAudio::default()
        };

        let maintenance = workspace.maintain(&mut audio, telemetry());

        assert_eq!(maintenance.compiled_slot, Some(second));
    }

    #[test]
    fn dirty_ticket_renormalizes_at_max_without_starving_an_older_slot() {
        let mut workspace = PatternWorkspace::new(48_000);
        workspace.dirty_patterns.fill(None);
        workspace.next_dirty_ticket = u64::MAX - 1;
        let first = PatternSlotId::new(0).unwrap();
        let second = PatternSlotId::new(1).unwrap();
        workspace.select_slot(first);
        workspace.toggle_step().unwrap();
        workspace.select_slot(second);
        workspace.toggle_step().unwrap();
        let mut audio = FakeAudio {
            backpressured: true,
            ..FakeAudio::default()
        };

        assert_eq!(
            workspace.maintain(&mut audio, telemetry()).compiled_slot,
            Some(second)
        );
        assert_eq!(
            workspace.maintain(&mut audio, telemetry()).compiled_slot,
            Some(first)
        );
    }

    #[test]
    fn rejected_matching_trigger_ack_clears_only_that_held_correlation() {
        let mut workspace = recording_workspace();
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);
        workspace.note_live_trigger(key(1), command(8), pad(), 1.0);
        let mut stale = trigger_ack(7, 1_008);
        stale.transport.as_mut().unwrap().generation = 99;

        workspace.apply_ack(stale);

        assert_eq!(workspace.pending_trigger_id(key(0)), None);
        assert_eq!(workspace.pending_trigger_id(key(1)), Some(command(8)));
    }

    #[test]
    fn full_pattern_rejects_a_matching_trigger_ack_without_leaking_its_key() {
        let mut workspace = PatternWorkspace::new(48_000);
        workspace.start_recording(origin(1_000)).unwrap();
        for frame in 0..sampler_core::MAX_PATTERN_EVENTS {
            workspace.patterns[0]
                .insert_new(pad(), u64::try_from(frame).unwrap(), 1.0, None)
                .unwrap();
        }
        workspace.note_live_trigger(key(0), command(7), pad(), 1.0);

        workspace.apply_ack(trigger_ack(7, 1_008));

        assert_eq!(workspace.pending_trigger_id(key(0)), None);
        assert_eq!(
            workspace.selected_pattern().events().len(),
            sampler_core::MAX_PATTERN_EVENTS
        );
    }

    #[test]
    fn exact_generation_lookup_does_not_substitute_a_newer_edit() {
        let mut workspace = PatternWorkspace::new(48_000);
        let slot = slot();
        assert!(workspace.pattern_for_generation(slot, 0).is_some());

        workspace.toggle_step().unwrap();

        assert!(workspace.pattern_for_generation(slot, 0).is_none());
        assert!(workspace.pattern_for_generation(slot, 1).is_some());
    }

    #[test]
    fn project_pattern_export_uses_all_sixteen_editable_slots() {
        let mut workspace = PatternWorkspace::new(48_000);
        workspace.toggle_step().unwrap();

        let exported = workspace.export_project_patterns().unwrap();

        assert_eq!(exported.len(), PATTERN_SLOT_COUNT);
        assert_eq!(exported[0].slot, PatternSlotId::new(0).unwrap());
        assert_eq!(exported[0].events.len(), 1);
        assert_eq!(exported[15].slot, PatternSlotId::new(15).unwrap());
    }

    #[test]
    fn project_pattern_replace_is_atomic_and_resets_transient_state() {
        let mut workspace = recording_workspace();
        workspace.toggle_step().unwrap();
        let before = workspace.export_project_patterns().unwrap();
        let mut invalid = before.clone();
        invalid[7].name.clear();

        assert!(workspace.replace_project_patterns(invalid).is_err());
        assert_eq!(workspace.export_project_patterns().unwrap(), before);

        let replacement = PatternWorkspace::new(44_100)
            .export_project_patterns()
            .unwrap();
        workspace
            .replace_project_patterns(replacement.clone())
            .unwrap();

        assert_eq!(workspace.export_project_patterns().unwrap(), replacement);
        assert!(!workspace.is_recording());
        for slot in 0..PATTERN_SLOT_COUNT {
            let slot = PatternSlotId::new(slot as u8).unwrap();
            assert!(!workspace.has_pending_snapshot(slot));
            assert!(workspace.needs_reinstall(slot));
            assert!(!workspace.is_slot_ready(slot));
        }
    }

    #[test]
    fn pattern_project_restore_preserves_all_slots_and_submits_in_slot_order() {
        let mut source = PatternWorkspace::new(44_100);
        for index in 0..PATTERN_SLOT_COUNT {
            let slot = PatternSlotId::new(index as u8).unwrap();
            source.select_slot(slot);
            source.cursor.step = index as u32;
            source
                .set_tempo(Tempo::new(90.0 + index as f64).unwrap())
                .unwrap();
            source.set_bars(1 + (index % 4) as u16).unwrap();
            source.set_swing(0.50 + index as f64 / 100.0).unwrap();
            source.set_quantize(index as f32 / 20.0).unwrap();
            source.toggle_step().unwrap();
        }
        let mut replacement = source.export_project_patterns().unwrap();
        for (index, pattern) in replacement.iter_mut().enumerate() {
            pattern.name = format!("Restored {:02}", index + 1);
        }

        let mut restored = PatternWorkspace::new(48_000);
        restored
            .replace_project_patterns(replacement.clone())
            .unwrap();

        let exported = restored.export_project_patterns().unwrap();
        assert_eq!(exported, replacement);
        for (index, pattern) in exported.iter().enumerate() {
            assert_eq!(pattern.slot, PatternSlotId::new(index as u8).unwrap());
            assert_eq!(pattern.sample_rate, 44_100);
            assert_eq!(pattern.events.len(), 1);
            assert_eq!(
                pattern.events[0].raw_frame,
                replacement[index].events[0].raw_frame
            );
            if index > 0 {
                assert!(pattern.events[0].raw_frame > 0);
            }
            assert_eq!(pattern.tempo, Tempo::new(90.0 + index as f64).unwrap());
        }

        let mut audio = OneSlotAudio::new();
        let mut submitted = Vec::new();
        for _ in 0..PATTERN_SLOT_COUNT {
            let maintenance = restored.maintain(&mut audio, telemetry());
            submitted.push(maintenance.submitted_slot.unwrap());
            audio.callback();
        }
        assert_eq!(
            submitted,
            (0..PATTERN_SLOT_COUNT)
                .map(|index| PatternSlotId::new(index as u8).unwrap())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn record_ack_mutations_are_counted_and_revision_budgeted() {
        let mut accepted = recording_workspace();
        let stamp = origin(1_000);
        accepted.note_live_trigger(0, command(91), pad(), 1.0);
        let mut audio = FakeAudio {
            acks: VecDeque::from([LiveAck {
                id: command(91),
                pad: pad(),
                kind: LiveAckKind::Trigger { velocity: 1.0 },
                frame: 1_120,
                transport: Some(stamp),
            }]),
            ..FakeAudio::default()
        };
        let maintenance =
            accepted.maintain_with_recording_budget(&mut audio, recording_telemetry(stamp), 1);
        assert_eq!(maintenance.committed_mutations, 1);
        assert_eq!(accepted.selected_pattern().events().len(), 1);

        let mut refused = recording_workspace();
        refused.note_live_trigger(0, command(92), pad(), 1.0);
        let mut audio = FakeAudio {
            acks: VecDeque::from([LiveAck {
                id: command(92),
                pad: pad(),
                kind: LiveAckKind::Trigger { velocity: 1.0 },
                frame: 1_120,
                transport: Some(stamp),
            }]),
            ..FakeAudio::default()
        };
        let maintenance =
            refused.maintain_with_recording_budget(&mut audio, recording_telemetry(stamp), 0);
        assert_eq!(maintenance.committed_mutations, 0);
        assert!(refused.selected_pattern().events().is_empty());
    }
}

use std::{array, sync::Arc};

use sampler_audio::{LiveAck, LiveAckKind, LiveCommandId, Telemetry, TransportStamp};
use sampler_core::{
    BankId, EditablePattern, EventId, Meter, PATTERN_SLOT_COUNT, PadId, PatternCompileError,
    PatternEditError, PatternEvent, PatternSlotId, PatternSnapshot, ProjectError, ProjectPattern,
    Resolution, Tempo, Transport,
};

use crate::AudioPort;

pub const MAX_RECORDING_KEYS: usize = 16;
const MAX_LIVE_RECORDING_KEYS: usize = MAX_RECORDING_KEYS + 16 * 128;
pub const MAX_ACKS_PER_MAINTENANCE: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectPatternWorkspaceError {
    SlotCount { found: usize },
    DuplicateSlot(PatternSlotId),
    Project(ProjectError),
}

impl std::fmt::Display for ProjectPatternWorkspaceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SlotCount { found } => write!(
                formatter,
                "project must contain exactly {PATTERN_SLOT_COUNT} patterns, found {found}"
            ),
            Self::DuplicateSlot(slot) => {
                write!(formatter, "duplicate project pattern slot {}", slot.get())
            }
            Self::Project(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ProjectPatternWorkspaceError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceView {
    Perform,
    Pattern,
    Sample,
    Mixer,
}

impl WorkspaceView {
    pub const fn next(self) -> Self {
        match self {
            Self::Perform => Self::Pattern,
            Self::Pattern => Self::Sample,
            Self::Sample => Self::Mixer,
            Self::Mixer => Self::Perform,
        }
    }

    pub const fn previous(self) -> Self {
        match self {
            Self::Perform => Self::Mixer,
            Self::Pattern => Self::Perform,
            Self::Sample => Self::Pattern,
            Self::Mixer => Self::Sample,
        }
    }
}

#[cfg(test)]
mod mixer_task8_view_tests {
    use super::WorkspaceView;

    #[test]
    fn workspace_cycles_through_four_views_in_both_directions() {
        assert_eq!(WorkspaceView::Perform.next(), WorkspaceView::Pattern);
        assert_eq!(WorkspaceView::Pattern.next(), WorkspaceView::Sample);
        assert_eq!(WorkspaceView::Sample.next(), WorkspaceView::Mixer);
        assert_eq!(WorkspaceView::Mixer.next(), WorkspaceView::Perform);

        assert_eq!(WorkspaceView::Perform.previous(), WorkspaceView::Mixer);
        assert_eq!(WorkspaceView::Mixer.previous(), WorkspaceView::Sample);
        assert_eq!(WorkspaceView::Sample.previous(), WorkspaceView::Pattern);
        assert_eq!(WorkspaceView::Pattern.previous(), WorkspaceView::Perform);
    }
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

/// The bounded sixteen-column cell that the Pattern view shows for one transport step.
/// Keeping this projection beside editing prevents the renderer and reducer from disagreeing
/// when swing, resolution, or partial quantization move an effective event frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DisplayCell {
    pub(crate) bar: u16,
    pub(crate) column: u32,
    pub(crate) start: u64,
    pub(crate) end: u64,
}

pub(crate) fn displayed_cell_for_step(transport: Transport, step: u32) -> DisplayCell {
    let bars = transport.bars().max(1);
    let steps_per_bar = (transport.step_count() / u32::from(bars)).max(1);
    let bar = u16::try_from(step / steps_per_bar)
        .unwrap_or(u16::MAX)
        .min(bars.saturating_sub(1));
    let first_step = u32::from(bar).saturating_mul(steps_per_bar);
    let bar_start = transport.step_frame(first_step);
    let bar_end = if bar.saturating_add(1) == bars {
        transport.loop_frames()
    } else {
        transport.step_frame(first_step.saturating_add(steps_per_bar))
    };
    let length = bar_end.saturating_sub(bar_start).max(1);
    let frame = transport.step_frame(step.min(transport.step_count().saturating_sub(1)));
    let local = frame
        .saturating_sub(bar_start)
        .min(length.saturating_sub(1));
    let column = u32::try_from(u128::from(local) * 16 / u128::from(length))
        .expect("sixteen-column projection fits in u32");
    let start = bar_start.saturating_add(
        u64::try_from((u128::from(column) * u128::from(length)).div_ceil(16))
            .expect("display cell start fits in u64"),
    );
    let end = bar_start.saturating_add(
        u64::try_from((u128::from(column + 1) * u128::from(length)).div_ceil(16))
            .expect("display cell end fits in u64"),
    );
    DisplayCell {
        bar,
        column,
        start,
        end: end.max(start.saturating_add(1)).min(bar_end),
    }
}

pub(crate) fn displayed_column_for_frame(
    transport: Transport,
    bar: u16,
    frame: u64,
) -> Option<u32> {
    let bars = transport.bars().max(1);
    let bar = bar.min(bars.saturating_sub(1));
    let steps_per_bar = (transport.step_count() / u32::from(bars)).max(1);
    let start = transport.step_frame(u32::from(bar).saturating_mul(steps_per_bar));
    let end = if bar.saturating_add(1) == bars {
        transport.loop_frames()
    } else {
        transport.step_frame(
            u32::from(bar)
                .saturating_add(1)
                .saturating_mul(steps_per_bar),
        )
    };
    let length = end.saturating_sub(start);
    let local = frame.checked_sub(start)?;
    (length != 0 && local < length).then(|| {
        u32::try_from(u128::from(local) * 16 / u128::from(length))
            .expect("sixteen-column projection fits in u32")
    })
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
    pub committed_mutations: usize,
    pub compiled_slot: Option<PatternSlotId>,
    pub submitted_slot: Option<PatternSlotId>,
    pub status: Option<PatternStatus>,
}

impl PatternMaintenance {
    fn empty() -> Self {
        Self {
            reclaimed_snapshots: 0,
            drained_acks: 0,
            committed_mutations: 0,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternCaptureState {
    Pending,
    Confirmed,
    Disarming,
}

#[derive(Debug, Clone, Copy)]
enum RecordingState {
    Pending(RecordingIntent),
    Confirmed(RecordingIntent),
    Rearming {
        previous: RecordingIntent,
        target: RecordingIntent,
        capture_command_pending: bool,
    },
    Disarming(RecordingIntent),
}

impl RecordingState {
    fn intent(self) -> RecordingIntent {
        match self {
            Self::Pending(intent) | Self::Confirmed(intent) | Self::Disarming(intent) => intent,
            Self::Rearming { target, .. } => target,
        }
    }

    fn capture_state(self) -> PatternCaptureState {
        match self {
            Self::Pending(_) | Self::Rearming { .. } => PatternCaptureState::Pending,
            Self::Confirmed(_) => PatternCaptureState::Confirmed,
            Self::Disarming(_) => PatternCaptureState::Disarming,
        }
    }

    fn accepts_acks(self) -> bool {
        !matches!(self, Self::Disarming(_))
    }
}

#[derive(Debug, Clone, Copy)]
struct HeldRecordingKey {
    pad: PadId,
    velocity: f32,
    trigger_id: Option<LiveCommandId>,
    release_id: Option<LiveCommandId>,
    event_id: Option<EventId>,
    event_slot: Option<PatternSlotId>,
    trigger_frame: Option<u64>,
    trigger_absolute_frame: Option<u64>,
    record_duration: bool,
}

#[derive(Debug, Clone, Copy)]
enum HeldRecordingLocation {
    Active(usize),
    Retiring(usize),
}

#[derive(Debug, Clone, Copy)]
struct SelectedEvent {
    slot: PatternSlotId,
    event_id: EventId,
}

#[derive(Debug, Clone, Copy)]
struct DirtyPattern {
    generation: u64,
    ticket: u64,
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
    project_patterns: Box<[ProjectPattern; PATTERN_SLOT_COUNT]>,
    selected_slot: PatternSlotId,
    cursor: PatternCursor,
    selected_event: Option<SelectedEvent>,
    view: WorkspaceView,
    playing: bool,
    recording: Option<RecordingState>,
    held_keys: Box<[Option<HeldRecordingKey>; MAX_LIVE_RECORDING_KEYS]>,
    retiring_keys: Box<[Option<HeldRecordingKey>; MAX_LIVE_RECORDING_KEYS]>,
    observed_live_ack_overflows: u64,
    dirty_patterns: [Option<DirtyPattern>; PATTERN_SLOT_COUNT],
    next_dirty_ticket: u64,
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
        let project_patterns = array::from_fn(|index| {
            ProjectPattern::from_editable(&patterns[index])
                .expect("default project pattern is valid")
        });
        Self {
            patterns: Box::new(patterns),
            project_patterns: Box::new(project_patterns),
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
            held_keys: Box::new([None; MAX_LIVE_RECORDING_KEYS]),
            retiring_keys: Box::new([None; MAX_LIVE_RECORDING_KEYS]),
            observed_live_ack_overflows: 0,
            dirty_patterns: array::from_fn(|_| {
                Some(DirtyPattern {
                    generation: 0,
                    ticket: 0,
                })
            }),
            next_dirty_ticket: 0,
            pending_snapshots: array::from_fn(|_| None),
            reinstall_pending: [true; PATTERN_SLOT_COUNT],
            installed_generations: [None; PATTERN_SLOT_COUNT],
            last_status: None,
        }
    }

    pub fn export_project_patterns(
        &self,
    ) -> Result<Vec<ProjectPattern>, ProjectPatternWorkspaceError> {
        Ok(self.project_patterns.to_vec())
    }

    pub fn replace_project_patterns(
        &mut self,
        patterns: Vec<ProjectPattern>,
    ) -> Result<(), ProjectPatternWorkspaceError> {
        if patterns.len() != PATTERN_SLOT_COUNT {
            return Err(ProjectPatternWorkspaceError::SlotCount {
                found: patterns.len(),
            });
        }
        let mut replacement: [Option<EditablePattern>; PATTERN_SLOT_COUNT] =
            array::from_fn(|_| None);
        let mut project_replacement: [Option<ProjectPattern>; PATTERN_SLOT_COUNT] =
            array::from_fn(|_| None);
        for pattern in patterns {
            let index = usize::from(pattern.slot().get());
            if replacement[index].is_some() {
                return Err(ProjectPatternWorkspaceError::DuplicateSlot(pattern.slot()));
            }
            let editable = pattern.to_editable().map_err(|error| {
                ProjectPatternWorkspaceError::Project(ProjectError::InvalidPattern(error))
            })?;
            replacement[index] = Some(editable);
            project_replacement[index] = Some(pattern);
        }
        let Some(patterns) = replacement
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .and_then(|patterns| patterns.try_into().ok())
            .map(Box::new)
        else {
            return Err(ProjectPatternWorkspaceError::SlotCount {
                found: PATTERN_SLOT_COUNT,
            });
        };
        let project_patterns = project_replacement
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .and_then(|patterns| patterns.try_into().ok())
            .map(Box::new)
            .expect("sixteen unique bounded slots fill the project pattern array");

        self.patterns = patterns;
        self.project_patterns = project_patterns;
        self.selected_slot = PatternSlotId::new(0).expect("first pattern slot is valid");
        self.cursor = PatternCursor {
            pad: PadId::new(BankId::new(0).expect("first bank is valid"), 0)
                .expect("first pad is valid"),
            step: 0,
            bar: 0,
        };
        self.selected_event = None;
        self.playing = false;
        self.recording = None;
        self.held_keys.fill(None);
        self.retiring_keys.fill(None);
        self.dirty_patterns = array::from_fn(|index| {
            Some(DirtyPattern {
                generation: self.patterns[index].generation(),
                ticket: 0,
            })
        });
        self.next_dirty_ticket = 0;
        self.pending_snapshots = array::from_fn(|_| None);
        self.reinstall_pending.fill(true);
        self.installed_generations.fill(None);
        self.last_status = None;
        Ok(())
    }

    pub fn view(&self) -> WorkspaceView {
        self.view
    }

    pub fn set_view(&mut self, view: WorkspaceView) {
        self.view = view;
    }

    pub fn toggle_view(&mut self) {
        self.view = self.view.next();
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

    /// Returns a pattern only when the caller's callback-visible generation still describes the
    /// editable slot. UI telemetry must not be rendered against a newer selected edit.
    pub fn pattern_for_generation(
        &self,
        slot: PatternSlotId,
        generation: u64,
    ) -> Option<&EditablePattern> {
        let pattern = self.pattern(slot);
        (pattern.generation() == generation).then_some(pattern)
    }

    pub fn sample_rates(&self) -> [u32; PATTERN_SLOT_COUNT] {
        array::from_fn(|index| self.patterns[index].transport().sample_rate())
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
        let transport = self.selected_pattern().transport();
        let bars = transport.bars();
        let steps_per_bar = (transport.step_count() / u32::from(bars)).max(1);
        let local_step = self.cursor.step % steps_per_bar;
        let bar = i32::from(self.cursor.bar)
            .saturating_add(delta)
            .clamp(0, i32::from(bars.saturating_sub(1)));
        self.cursor.bar = u16::try_from(bar).expect("clamped bar fits in u16");
        self.cursor.step = u32::from(self.cursor.bar)
            .saturating_mul(steps_per_bar)
            .saturating_add(local_step)
            .min(transport.step_count().saturating_sub(1));
        self.refresh_selected_event();
    }

    pub fn selected_event(&self) -> Option<&PatternEvent> {
        self.selected_event.and_then(|selected| {
            (selected.slot == self.selected_slot)
                .then(|| self.selected_pattern().event(selected.event_id))
                .flatten()
        })
    }

    pub fn toggle_step(&mut self) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        let selected = self.event_in_cursor_cell();
        if let Some(event_id) = selected {
            self.patterns[index].remove(event_id)?;
            self.selected_event = None;
        } else {
            let raw_frame = self.patterns[index]
                .transport()
                .step_frame(self.cursor.step);
            let event_id =
                self.patterns[index].insert_new(self.cursor.pad, raw_frame, 1.0, None)?;
            self.selected_event = Some(SelectedEvent {
                slot: self.selected_slot,
                event_id,
            });
        }
        self.commit_project_pattern(index);
        Ok(())
    }

    pub fn delete_step(&mut self) -> Result<(), PatternEditError> {
        let Some(event_id) = self.event_in_cursor_cell() else {
            return Ok(());
        };
        let index = self.slot_index();
        self.patterns[index].remove(event_id)?;
        self.selected_event = None;
        self.commit_project_pattern(index);
        Ok(())
    }

    pub fn adjust_velocity(&mut self, delta: f32) -> Result<(), PatternEditError> {
        if !delta.is_finite() {
            return Err(PatternEditError::InvalidVelocity);
        }
        let Some(event_id) = self.selected_event_id() else {
            return Ok(());
        };
        let Some(event) = self.selected_pattern().event(event_id) else {
            self.selected_event = None;
            return Ok(());
        };
        let velocity = event.velocity;
        let index = self.slot_index();
        self.patterns[index].set_velocity(event_id, (velocity + delta).clamp(0.0, 1.0))?;
        self.commit_project_pattern(index);
        Ok(())
    }

    pub fn set_tempo(&mut self, tempo: Tempo) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].set_tempo(tempo)?;
        self.commit_project_pattern(index);
        self.clamp_cursor();
        self.refresh_selected_event();
        Ok(())
    }

    pub fn set_bars(&mut self, bars: u16) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].set_bars(bars)?;
        self.commit_project_pattern(index);
        self.clamp_cursor();
        self.refresh_selected_event();
        Ok(())
    }

    pub fn set_resolution(&mut self, resolution: Resolution) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].set_resolution(resolution)?;
        self.commit_project_pattern(index);
        self.clamp_cursor();
        self.refresh_selected_event();
        Ok(())
    }

    pub fn set_swing(&mut self, swing: f64) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].set_swing(swing)?;
        self.commit_project_pattern(index);
        self.refresh_selected_event();
        Ok(())
    }

    pub fn set_quantize(&mut self, strength: f32) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].set_quantize_strength(strength)?;
        self.commit_project_pattern(index);
        self.refresh_selected_event();
        Ok(())
    }

    pub fn clear_selected(&mut self) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].clear()?;
        self.selected_event = None;
        self.stop_recording();
        self.commit_project_pattern(index);
        Ok(())
    }

    pub fn undo_clear(&mut self) -> Result<(), PatternEditError> {
        let index = self.slot_index();
        self.patterns[index].undo_clear()?;
        self.commit_project_pattern(index);
        self.refresh_selected_event();
        Ok(())
    }

    pub fn start_recording(&mut self, stamp: TransportStamp) -> Result<(), PatternEditError> {
        if stamp.slot != self.selected_slot || stamp.loop_frames == 0 {
            return Err(PatternEditError::InvalidSlot);
        }
        self.recording = Some(RecordingState::Pending(RecordingIntent { stamp }));
        self.held_keys.fill(None);
        self.retiring_keys.fill(None);
        Ok(())
    }

    pub fn stop_recording(&mut self) {
        self.recording = self
            .recording
            .map(|state| RecordingState::Disarming(state.intent()));
        self.held_keys.fill(None);
        self.retiring_keys.fill(None);
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some_and(RecordingState::accepts_acks)
    }

    pub fn capture_state(&self) -> Option<PatternCaptureState> {
        self.recording.map(RecordingState::capture_state)
    }

    pub fn record_capture(&self) -> Option<(PatternSlotId, u64)> {
        self.recording
            .filter(|state| state.accepts_acks())
            .map(|state| {
                let stamp = state.intent().stamp;
                (stamp.slot, stamp.generation)
            })
    }

    pub fn note_live_trigger(
        &mut self,
        key: usize,
        command: LiveCommandId,
        pad: PadId,
        velocity: f32,
    ) {
        self.note_live_trigger_with_duration(key, command, pad, velocity, true);
    }

    pub fn note_live_trigger_with_duration(
        &mut self,
        key: usize,
        command: LiveCommandId,
        pad: PadId,
        velocity: f32,
        record_duration: bool,
    ) {
        let Some(previous) = self.held_keys.get_mut(key).map(Option::take) else {
            return;
        };
        if let Some(previous) = previous {
            let Some(retiring) = self.retiring_keys.iter_mut().find(|entry| entry.is_none()) else {
                self.held_keys[key] = Some(previous);
                return;
            };
            *retiring = Some(previous);
        }
        self.held_keys[key] = Some(HeldRecordingKey {
            pad,
            velocity: velocity.clamp(0.0, 1.0),
            trigger_id: Some(command),
            release_id: None,
            event_id: None,
            event_slot: None,
            trigger_frame: None,
            trigger_absolute_frame: None,
            record_duration,
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

    pub fn apply_ack(&mut self, ack: LiveAck) -> bool {
        let Some(location) = self.matching_held_key(ack) else {
            return false;
        };
        let Some(entry) = self.recording_key(location) else {
            return false;
        };
        let matching_trigger =
            matches!(ack.kind, LiveAckKind::Trigger { .. }) && entry.trigger_id == Some(ack.id);
        let matching_release =
            matches!(ack.kind, LiveAckKind::Release) && entry.release_id == Some(ack.id);
        if !matching_trigger && !matching_release {
            return false;
        }

        let Some(state) = self.recording else {
            self.set_recording_key(location, None);
            return false;
        };
        let intent = state.intent();
        let Some(mut stamp) = ack.transport else {
            self.set_recording_key(location, None);
            return false;
        };
        let accepted = match state {
            RecordingState::Pending(intent) => {
                stamp.slot == intent.stamp.slot
                    && stamp.generation == intent.stamp.generation
                    && stamp.loop_frames != 0
            }
            RecordingState::Confirmed(intent) => stamp == intent.stamp,
            RecordingState::Rearming {
                previous, target, ..
            } => stamp == previous.stamp || stamp == target.stamp,
            RecordingState::Disarming(_) => false,
        };
        if !state.accepts_acks() || !accepted {
            self.set_recording_key(location, None);
            return false;
        }
        if matches!(state, RecordingState::Pending(_)) && stamp != intent.stamp {
            self.recording = Some(RecordingState::Pending(RecordingIntent { stamp }));
        } else if !matches!(state, RecordingState::Rearming { .. }) {
            stamp = intent.stamp;
        }

        match ack.kind {
            LiveAckKind::Trigger { velocity } => {
                if ack.pad != entry.pad {
                    self.set_recording_key(location, None);
                    return false;
                }
                let frame = ack.frame.wrapping_sub(stamp.origin) % stamp.loop_frames;
                let index = usize::from(stamp.slot.get());
                let velocity = if velocity.is_finite() {
                    velocity.clamp(0.0, 1.0)
                } else {
                    entry.velocity
                };
                match self.patterns[index].insert_new(entry.pad, frame, velocity, None) {
                    Ok(event_id) => {
                        let mut entry = entry;
                        entry.event_id = Some(event_id);
                        entry.event_slot = Some(stamp.slot);
                        entry.trigger_frame = Some(frame);
                        entry.trigger_absolute_frame = Some(ack.frame);
                        self.set_recording_key(location, entry.record_duration.then_some(entry));
                        if stamp.slot == self.selected_slot {
                            self.selected_event = Some(SelectedEvent {
                                slot: stamp.slot,
                                event_id,
                            });
                        }
                        self.commit_project_pattern(index);
                        true
                    }
                    Err(_) => {
                        self.set_recording_key(location, None);
                        false
                    }
                }
            }
            LiveAckKind::Release => {
                let (Some(event_id), Some(trigger_absolute_frame)) =
                    (entry.event_id, entry.trigger_absolute_frame)
                else {
                    self.set_recording_key(location, None);
                    return false;
                };
                let elapsed = ack.frame.saturating_sub(trigger_absolute_frame);
                let duration = elapsed.min(stamp.loop_frames);
                let index = usize::from(stamp.slot.get());
                let committed = duration != 0
                    && self.patterns[index]
                        .set_duration(event_id, Some(duration))
                        .is_ok();
                if committed {
                    if stamp.slot == self.selected_slot {
                        self.selected_event = Some(SelectedEvent {
                            slot: stamp.slot,
                            event_id,
                        });
                    }
                    self.commit_project_pattern(index);
                }
                self.set_recording_key(location, None);
                committed
            }
        }
    }

    fn matching_held_key(&self, ack: LiveAck) -> Option<HeldRecordingLocation> {
        let matches = |entry: &Option<HeldRecordingKey>| {
            entry.is_some_and(|entry| match ack.kind {
                LiveAckKind::Trigger { .. } => entry.trigger_id == Some(ack.id),
                LiveAckKind::Release => entry.release_id == Some(ack.id),
            })
        };
        self.held_keys
            .iter()
            .position(matches)
            .map(HeldRecordingLocation::Active)
            .or_else(|| {
                self.retiring_keys
                    .iter()
                    .position(matches)
                    .map(HeldRecordingLocation::Retiring)
            })
    }

    fn recording_key(&self, location: HeldRecordingLocation) -> Option<HeldRecordingKey> {
        match location {
            HeldRecordingLocation::Active(index) => self.held_keys[index],
            HeldRecordingLocation::Retiring(index) => self.retiring_keys[index],
        }
    }

    fn set_recording_key(
        &mut self,
        location: HeldRecordingLocation,
        entry: Option<HeldRecordingKey>,
    ) {
        match location {
            HeldRecordingLocation::Active(index) => self.held_keys[index] = entry,
            HeldRecordingLocation::Retiring(index) => self.retiring_keys[index] = entry,
        }
    }

    fn reconcile_record_capture(&mut self, telemetry: Telemetry) {
        let Some(state) = self.recording else {
            return;
        };
        let stamp = state.intent().stamp;
        if let RecordingState::Pending(intent) = state
            && telemetry.pattern_recording
            && telemetry.pattern_slot == Some(intent.stamp.slot)
            && telemetry.pattern_generation == Some(intent.stamp.generation)
            && let Some(origin) = telemetry.pattern_origin
            && origin != intent.stamp.origin
        {
            let index = usize::from(intent.stamp.slot.get());
            self.recording = Some(RecordingState::Pending(RecordingIntent {
                stamp: TransportStamp {
                    origin,
                    loop_frames: self.patterns[index].transport().loop_frames(),
                    ..intent.stamp
                },
            }));
            return;
        }
        let matches_stamp = |candidate: TransportStamp| {
            telemetry.pattern_recording
                && telemetry.pattern_slot == Some(candidate.slot)
                && telemetry.pattern_generation == Some(candidate.generation)
                && telemetry.pattern_origin == Some(candidate.origin)
        };
        let matches_capture = matches_stamp(stamp);
        let next = match state {
            RecordingState::Pending(intent) if matches_capture => {
                Some(RecordingState::Confirmed(intent))
            }
            RecordingState::Confirmed(_) if !matches_capture => None,
            RecordingState::Rearming { target, .. }
                if telemetry
                    .pattern_slot
                    .is_some_and(|live_slot| live_slot != target.stamp.slot) =>
            {
                None
            }
            RecordingState::Rearming {
                previous: _,
                target,
                ..
            } if matches_stamp(target.stamp) => Some(RecordingState::Confirmed(target)),
            RecordingState::Rearming { .. } => Some(state),
            RecordingState::Disarming(_) if !matches_capture => None,
            _ => Some(state),
        };
        if next.is_none() {
            self.held_keys.fill(None);
            self.retiring_keys.fill(None);
        }
        self.recording = next;
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

    /// Includes an uncompiled dirty edit and a submitted-but-not-current snapshot, so status
    /// never briefly claims an edited slot is current between UI maintenance passes.
    pub fn updates_pending(&self, slot: PatternSlotId) -> bool {
        !self.is_slot_ready(slot)
    }

    pub fn needs_reinstall(&self, slot: PatternSlotId) -> bool {
        self.reinstall_pending[usize::from(slot.get())]
    }

    /// True only after the current editable generation has been admitted to the audio controller.
    pub fn is_slot_ready(&self, slot: PatternSlotId) -> bool {
        let index = usize::from(slot.get());
        self.installed_generations[index] == Some(self.patterns[index].generation())
            && !self.reinstall_pending[index]
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
        self.maintain_with_recording_budget(audio, telemetry, usize::MAX)
    }

    pub(crate) fn maintain_with_recording_budget(
        &mut self,
        audio: &mut dyn AudioPort,
        telemetry: Telemetry,
        recording_mutation_budget: usize,
    ) -> PatternMaintenance {
        let mut result = PatternMaintenance::empty();
        result.reclaimed_snapshots = audio.reclaim_retired_patterns();
        self.recover_from_live_ack_overflow(telemetry.live_ack_overflows);

        let mut acks = [LiveAck::EMPTY; MAX_ACKS_PER_MAINTENANCE];
        result.drained_acks = audio.drain_live_acks(&mut acks).min(acks.len());
        for ack in acks.into_iter().take(result.drained_acks) {
            if result.committed_mutations < recording_mutation_budget {
                result.committed_mutations += usize::from(self.apply_ack(ack));
            } else if let Some(location) = self.matching_held_key(ack) {
                self.set_recording_key(location, None);
            }
        }

        self.playing = telemetry.pattern_playing;
        self.reconcile_record_capture(telemetry);

        if let Some((index, dirty)) = self.next_dirty_slot() {
            let slot = self.patterns[index].slot();
            match self.patterns[index].compile() {
                Ok(snapshot) => {
                    if self.patterns[index].generation() == dirty.generation {
                        self.pending_snapshots[index] = Some(PendingSnapshot {
                            generation: dirty.generation,
                            snapshot: Arc::new(snapshot),
                        });
                        self.dirty_patterns[index] = None;
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
            let pending_generation = pending.generation;
            if self.patterns[index].generation() != pending_generation {
                self.pending_snapshots[index] = None;
                self.mark_dirty(index);
                let status = PatternStatus::UpdatePending { slot };
                self.last_status = Some(status.clone());
                result.status = Some(status);
                return result;
            }
            match audio.install_pattern(Arc::clone(&pending.snapshot)) {
                Ok(_) => {
                    self.installed_generations[index] = Some(pending_generation);
                    self.pending_snapshots[index] = None;
                    self.reinstall_pending[index] = false;
                    // The callback replaces an active slot immediately. Re-arm capture with
                    // that exact admitted identity before the next tracked live command can be
                    // acknowledged against it; origin stays causal across a replacement.
                    if self.rebind_recording_to_admitted_generation(slot, pending_generation)
                        && let Err(error) = self.submit_record_capture_rearm(audio)
                    {
                        let status = PatternStatus::AudioCommandFailed { slot, error };
                        self.last_status = Some(status.clone());
                        result.status = Some(status);
                    }
                    result.submitted_slot = Some(slot);
                    if result.status.is_none() {
                        self.last_status = None;
                    }
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
        } else if self.rearm_capture_command_pending()
            && let Err(error) = self.submit_record_capture_rearm(audio)
        {
            let slot = self
                .recording
                .expect("rearm requires recording")
                .intent()
                .stamp
                .slot;
            let status = PatternStatus::AudioCommandFailed { slot, error };
            self.last_status = Some(status.clone());
            result.status = Some(status);
        } else if let Some((index, _)) = self.next_dirty_slot() {
            let status = PatternStatus::UpdatePending {
                slot: self.patterns[index].slot(),
            };
            self.last_status = Some(status.clone());
            result.status = Some(status);
        }
        result
    }

    fn recover_from_live_ack_overflow(&mut self, live_ack_overflows: u64) {
        if live_ack_overflows <= self.observed_live_ack_overflows {
            self.observed_live_ack_overflows = live_ack_overflows;
            return;
        }
        self.observed_live_ack_overflows = live_ack_overflows;

        let mut removals = self
            .held_keys
            .iter()
            .chain(self.retiring_keys.iter())
            .flatten()
            .filter_map(|entry| Some((entry.event_slot?, entry.event_id?)))
            .collect::<Vec<_>>();
        removals.sort_unstable_by_key(|(slot, event_id)| (slot.get(), event_id.0));
        removals.dedup();

        let mut removal_counts = [0_u64; PATTERN_SLOT_COUNT];
        removals.retain(|(slot, event_id)| {
            let index = usize::from(slot.get());
            let unfinalized = self.patterns[index]
                .event(*event_id)
                .is_some_and(|event| event.duration.is_none());
            if unfinalized {
                removal_counts[index] += 1;
            }
            unfinalized
        });
        debug_assert!(removal_counts.iter().enumerate().all(|(index, count)| {
            self.patterns[index]
                .generation()
                .checked_add(*count)
                .is_some()
        }));

        self.held_keys.fill(None);
        self.retiring_keys.fill(None);

        let mut changed = [false; PATTERN_SLOT_COUNT];
        let mut refresh_selection = false;
        for (slot, event_id) in removals {
            let index = usize::from(slot.get());
            self.patterns[index]
                .remove(event_id)
                .expect("a correlated unfinalized event remains internally removable");
            if self
                .selected_event
                .is_some_and(|selected| selected.slot == slot && selected.event_id == event_id)
            {
                self.selected_event = None;
                refresh_selection = true;
            }
            changed[index] = true;
        }
        for (index, changed) in changed.into_iter().enumerate() {
            if changed {
                self.commit_project_pattern(index);
            }
        }
        if refresh_selection {
            self.refresh_selected_event();
        }
    }

    fn slot_index(&self) -> usize {
        usize::from(self.selected_slot.get())
    }

    fn selected_event_id(&self) -> Option<EventId> {
        self.selected_event
            .filter(|selected| selected.slot == self.selected_slot)
            .map(|selected| selected.event_id)
    }

    fn mark_dirty(&mut self, index: usize) {
        let generation = self.patterns[index].generation();
        if self.next_dirty_ticket == u64::MAX {
            self.renormalize_dirty_tickets();
        }
        self.next_dirty_ticket += 1;
        self.dirty_patterns[index] = Some(DirtyPattern {
            generation,
            ticket: self.next_dirty_ticket,
        });
        self.reinstall_pending[index] = true;
        if self.pending_snapshots[index]
            .as_ref()
            .is_some_and(|pending| pending.generation != generation)
        {
            self.pending_snapshots[index] = None;
        }
    }

    fn commit_project_pattern(&mut self, index: usize) {
        self.project_patterns[index] = ProjectPattern::from_editable(&self.patterns[index])
            .expect("a validated editable pattern remains persistable after a committed edit");
        self.mark_dirty(index);
    }

    fn renormalize_dirty_tickets(&mut self) {
        let mut indexes: [usize; PATTERN_SLOT_COUNT] = array::from_fn(|index| index);
        indexes.sort_by_key(|index| {
            self.dirty_patterns[*index]
                .map(|dirty| (0_u8, dirty.ticket, *index))
                .unwrap_or((1_u8, 0, *index))
        });
        let mut ticket = 0_u64;
        for index in indexes {
            if let Some(dirty) = self.dirty_patterns[index].as_mut() {
                ticket += 1;
                dirty.ticket = ticket;
            }
        }
        self.next_dirty_ticket = ticket;
    }

    fn next_dirty_slot(&self) -> Option<(usize, DirtyPattern)> {
        self.dirty_patterns
            .iter()
            .enumerate()
            .filter_map(|(index, dirty)| dirty.map(|dirty| (index, dirty)))
            .fold(None, |best, candidate| match best {
                Some((_, dirty)) if dirty.ticket >= candidate.1.ticket => best,
                _ => Some(candidate),
            })
    }

    fn next_pending_slot(&self) -> Option<usize> {
        self.pending_snapshots.iter().position(Option::is_some)
    }

    fn rearm_capture_command_pending(&self) -> bool {
        self.recording.is_some_and(|state| {
            matches!(
                state,
                RecordingState::Rearming {
                    capture_command_pending: true,
                    ..
                }
            )
        })
    }

    fn submit_record_capture_rearm(&mut self, audio: &mut dyn AudioPort) -> Result<(), String> {
        let capture = self.record_capture();
        audio.set_record_capture(capture)?;
        if let Some(RecordingState::Rearming {
            capture_command_pending,
            ..
        }) = self.recording.as_mut()
        {
            *capture_command_pending = false;
        }
        Ok(())
    }

    fn rebind_recording_to_admitted_generation(
        &mut self,
        slot: PatternSlotId,
        generation: u64,
    ) -> bool {
        let Some(state) = self.recording else {
            return false;
        };
        if !state.accepts_acks() || state.intent().stamp.slot != slot {
            return false;
        }
        let intent = state.intent();
        if intent.stamp.generation == generation {
            return false;
        }
        let stamp = TransportStamp {
            generation,
            loop_frames: self.patterns[usize::from(slot.get())]
                .transport()
                .loop_frames(),
            ..intent.stamp
        };
        let target = RecordingIntent { stamp };
        self.recording = Some(match state {
            RecordingState::Pending(previous) => RecordingState::Rearming {
                previous,
                target,
                capture_command_pending: true,
            },
            RecordingState::Confirmed(previous) => RecordingState::Rearming {
                previous,
                target,
                capture_command_pending: true,
            },
            RecordingState::Rearming { previous, .. } => RecordingState::Rearming {
                previous,
                target,
                capture_command_pending: true,
            },
            RecordingState::Disarming(intent) => RecordingState::Disarming(intent),
        });
        true
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
        self.selected_event = self.event_in_cursor_cell().map(|event_id| SelectedEvent {
            slot: self.selected_slot,
            event_id,
        });
    }

    fn event_in_cursor_cell(&self) -> Option<EventId> {
        let cell = displayed_cell_for_step(self.selected_pattern().transport(), self.cursor.step);
        self.selected_pattern()
            .events()
            .iter()
            .filter(|event| {
                event.pad == self.cursor.pad && event.frame >= cell.start && event.frame < cell.end
            })
            .map(|event| event.id)
            .min()
    }
}

fn compile_error_text(error: PatternCompileError) -> String {
    error.to_string()
}
