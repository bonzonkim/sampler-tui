use std::ops::Range;

use sampler_core::{PadId, PadSettings, PlaybackMode, SAMPLE_PHASE_SCALE, SampleEditRecipe};

use crate::SampleEditStatus;

/// The trim endpoint affected by marker movement and zoom anchoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleMarker {
    Start,
    End,
}

/// A fixed, integer-only visible part of the source phase domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleViewport {
    pub start: u64,
    pub end: u64,
}

impl SampleViewport {
    fn full() -> Self {
        Self {
            start: 0,
            end: SAMPLE_PHASE_SCALE,
        }
    }

    fn new(start: u64, end: u64) -> Self {
        let start = start.min(SAMPLE_PHASE_SCALE.saturating_sub(1));
        let end = end.clamp(start.saturating_add(1), SAMPLE_PHASE_SCALE);
        Self { start, end }
    }

    fn width(self) -> u64 {
        self.end.saturating_sub(self.start).max(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffscreenDirection {
    Left,
    Right,
}

/// Column projection used by the renderer. Marker columns are always valid for widths >= 1;
/// direction says when the real marker lies outside the visible half-open range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleProjection {
    pub visible: Range<u64>,
    pub start_column: u16,
    pub end_column: u16,
    pub start_offscreen: Option<OffscreenDirection>,
    pub end_offscreen: Option<OffscreenDirection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleEditorError {
    WorkerBusy,
    WorkerClosed,
    StaleResult,
    AudioQueueFull,
    InstallFailed,
    DeviceUnavailable,
    GenerationExhausted,
    SelectedPadReplaced,
    UnsupportedSourceLength,
    InvalidSourceRate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleEditorStatus {
    Empty,
    Clean,
    Dirty,
    Pending,
    Error(SampleEditorError),
    ApplyConfirmation,
    DiscardConfirmation,
    UndoAvailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEditorIntent {
    ReturnToPerform,
    ConfirmDiscard {
        pad: PadId,
    },
    Apply {
        pad: PadId,
        recipe: SampleEditRecipe,
    },
    Undo {
        pad: PadId,
    },
}

/// Read-only App projection. It deliberately carries no worker generation or audio ownership;
/// the App owns those exact identities and this reducer only presents their state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampleEditorContext {
    pub pad: PadId,
    pub committed: Option<SampleEditRecipe>,
    pub base_frames: Option<usize>,
    pub base_rate: Option<u32>,
    pub settings: PadSettings,
    pub edit_status: SampleEditStatus,
    pub device_available: bool,
}

/// Bounded UI-only editing state for exactly one selected pad.  It never owns PCM or worker
/// requests: `App` remains the single source of truth for edit generations and audio admission.
#[derive(Debug, Clone)]
pub struct SampleEditor {
    pad: PadId,
    committed: Option<SampleEditRecipe>,
    draft: SampleEditRecipe,
    committed_settings: PadSettings,
    settings: PadSettings,
    base_frames: Option<usize>,
    base_rate: Option<u32>,
    marker: SampleMarker,
    zoom: u8,
    viewport: SampleViewport,
    status: SampleEditorStatus,
    undo_available: bool,
}

impl SampleEditor {
    pub const MAX_ZOOM: u8 = 24;

    pub fn open_loaded(
        pad: PadId,
        committed: SampleEditRecipe,
        base_frames: usize,
        base_rate: u32,
        settings: PadSettings,
    ) -> Self {
        Self::open(SampleEditorContext {
            pad,
            committed: Some(committed),
            base_frames: Some(base_frames),
            base_rate: Some(base_rate),
            settings,
            edit_status: SampleEditStatus::Idle,
            device_available: true,
        })
    }

    pub fn open_empty(pad: PadId, settings: PadSettings) -> Self {
        Self::open(SampleEditorContext {
            pad,
            committed: None,
            base_frames: None,
            base_rate: None,
            settings,
            edit_status: SampleEditStatus::Idle,
            device_available: true,
        })
    }

    pub fn open(context: SampleEditorContext) -> Self {
        let mut editor = Self {
            pad: context.pad,
            committed: None,
            draft: SampleEditRecipe::identity(),
            committed_settings: context.settings,
            settings: context.settings,
            base_frames: None,
            base_rate: None,
            marker: SampleMarker::Start,
            zoom: 0,
            viewport: SampleViewport::full(),
            status: SampleEditorStatus::Empty,
            undo_available: false,
        };
        editor.sync_context(context);
        editor
    }

    /// Applies only read-only status from App. A replacement while dirty is surfaced rather than
    /// overwriting the draft; the caller must explicitly discard before opening the new pad.
    pub fn sync_context(&mut self, context: SampleEditorContext) {
        let recipe_dirty = self.recipe_dirty();
        let settings_dirty = self.settings_dirty();
        if context.pad != self.pad && (recipe_dirty || settings_dirty) {
            self.status = SampleEditorStatus::Error(SampleEditorError::SelectedPadReplaced);
            return;
        }
        let recipe_conflict = context.pad == self.pad
            && recipe_dirty
            && context
                .committed
                .is_some_and(|recipe| Some(recipe) != self.committed);
        self.undo_available = matches!(context.edit_status, SampleEditStatus::UndoAvailable);
        self.pad = context.pad;
        match (context.committed, context.base_frames, context.base_rate) {
            (Some(recipe), Some(frames), Some(rate))
                if recipe.validate().is_ok()
                    && frames != 0
                    && rate != 0
                    && source_frames_supported(frames) =>
            {
                self.committed = Some(recipe);
                self.base_frames = Some(frames);
                self.base_rate = Some(rate);
                self.committed_settings = context.settings;
                if !recipe_dirty {
                    self.draft = recipe;
                }
                if !settings_dirty {
                    self.settings = context.settings;
                }
            }
            _ => {
                if recipe_dirty || settings_dirty {
                    self.status = SampleEditorStatus::Error(SampleEditorError::SelectedPadReplaced);
                    return;
                }
                self.committed = None;
                self.draft = SampleEditRecipe::identity();
                self.base_frames = None;
                self.base_rate = None;
                self.committed_settings = context.settings;
                self.settings = context.settings;
                self.status = if context
                    .base_frames
                    .is_some_and(|frames| !source_frames_supported(frames))
                {
                    SampleEditorStatus::Error(SampleEditorError::UnsupportedSourceLength)
                } else if context.base_rate == Some(0) {
                    SampleEditorStatus::Error(SampleEditorError::InvalidSourceRate)
                } else {
                    SampleEditorStatus::Empty
                };
                return;
            }
        }
        if !context.device_available {
            self.observe_error(SampleEditorError::DeviceUnavailable);
            return;
        }
        if recipe_conflict {
            self.observe_error(SampleEditorError::SelectedPadReplaced);
            return;
        }
        match context.edit_status {
            SampleEditStatus::AwaitingWorker
            | SampleEditStatus::Rendering
            | SampleEditStatus::ReadyToInstall => self.observe_pending(),
            SampleEditStatus::Failed => self.observe_error(SampleEditorError::InstallFailed),
            SampleEditStatus::GenerationExhausted => {
                self.observe_error(SampleEditorError::GenerationExhausted)
            }
            SampleEditStatus::UndoAvailable => self.status = SampleEditorStatus::UndoAvailable,
            SampleEditStatus::Idle => self.note_draft_change(),
        }
    }

    pub fn pad(&self) -> PadId {
        self.pad
    }
    pub fn committed(&self) -> Option<SampleEditRecipe> {
        self.committed
    }
    pub fn draft(&self) -> SampleEditRecipe {
        self.draft
    }
    pub fn settings(&self) -> PadSettings {
        self.settings
    }
    pub fn base_frames(&self) -> Option<usize> {
        self.base_frames
    }
    pub fn base_rate(&self) -> Option<u32> {
        self.base_rate
    }
    pub fn marker(&self) -> SampleMarker {
        self.marker
    }
    pub fn viewport(&self) -> SampleViewport {
        self.viewport
    }
    pub fn zoom_level(&self) -> u8 {
        self.zoom
    }
    pub fn status(&self) -> SampleEditorStatus {
        self.status.clone()
    }

    pub fn set_marker(&mut self, marker: SampleMarker) {
        self.marker = marker;
    }

    pub fn set_viewport(&mut self, start: u64, end: u64) {
        self.viewport = SampleViewport::new(start, end);
    }

    /// Fine movement is exactly one source frame. Shift movement is one hundredth of the visible
    /// window, rounded up to one source frame, all in checked Q32 integer arithmetic.
    pub fn move_marker(&mut self, direction: i8, coarse: bool) {
        let Some(frames) = self.base_frames else {
            return;
        };
        let current = self.marker_frame(frames);
        let step = if coarse {
            self.coarse_frames(frames)
        } else {
            1
        };
        let movement = i128::from(direction).saturating_mul(step as i128);
        let candidate = if movement.is_negative() {
            current.saturating_sub(movement.unsigned_abs().min(usize::MAX as u128) as usize)
        } else {
            current.saturating_add(movement.min(usize::MAX as i128) as usize)
        };
        let frame = match self.marker {
            SampleMarker::Start => candidate.min(frames.saturating_sub(1)),
            SampleMarker::End => candidate.clamp(1, frames),
        };
        self.set_marker_frame(frames, frame);
        self.note_draft_change();
    }

    pub fn zoom_in(&mut self) {
        self.zoom_by(1);
    }
    pub fn zoom_out(&mut self) {
        self.zoom_by(-1);
    }

    pub fn toggle_normalize(&mut self) {
        if self.committed.is_none() {
            return;
        }
        self.draft.normalize = !self.draft.normalize;
        self.note_draft_change();
    }

    pub fn toggle_reverse(&mut self) {
        if self.committed.is_none() {
            return;
        }
        self.draft.reversed = !self.draft.reversed;
        self.note_draft_change();
    }

    pub fn adjust_pitch(&mut self, semitones: i8) {
        self.settings.pitch_semitones =
            (self.settings.pitch_semitones + f32::from(semitones)).clamp(-24.0, 24.0);
        self.note_draft_change();
    }

    pub fn set_mode(&mut self, mode: PlaybackMode) {
        self.settings.mode = mode;
        self.note_draft_change();
    }

    pub fn escape(&mut self) -> SampleEditorIntent {
        if self.is_dirty() {
            self.status = SampleEditorStatus::DiscardConfirmation;
            SampleEditorIntent::ConfirmDiscard { pad: self.pad }
        } else {
            SampleEditorIntent::ReturnToPerform
        }
    }

    pub fn request_apply(&mut self) -> Option<SampleEditorIntent> {
        self.committed?;
        if !self.is_dirty() {
            return None;
        }
        self.status = SampleEditorStatus::ApplyConfirmation;
        Some(SampleEditorIntent::Apply {
            pad: self.pad,
            recipe: self.draft,
        })
    }

    pub fn request_undo(&mut self) -> Option<SampleEditorIntent> {
        if !self.undo_available {
            return None;
        }
        self.status = SampleEditorStatus::Pending;
        Some(SampleEditorIntent::Undo { pad: self.pad })
    }

    pub fn confirm_discard(&mut self) {
        if let Some(committed) = self.committed {
            self.draft = committed;
            self.settings = self.committed_settings;
            self.set_clean_status();
        }
    }

    pub fn observe_pending(&mut self) {
        self.status = SampleEditorStatus::Pending;
    }
    pub fn observe_error(&mut self, error: SampleEditorError) {
        self.status = SampleEditorStatus::Error(error);
    }

    pub fn observe_apply_succeeded(&mut self) {
        if self.committed.is_none() {
            return;
        }
        self.committed = Some(self.draft);
        self.committed_settings = self.settings;
        self.set_clean_status();
    }

    pub fn observe_apply_failed(&mut self, error: SampleEditorError) {
        self.observe_error(error);
    }

    pub fn observe_undo_succeeded(&mut self) {
        self.set_clean_status();
    }

    pub fn observe_undo_failed(&mut self, error: SampleEditorError) {
        self.observe_error(error);
    }

    /// A replacement notification cannot erase an uncommitted draft; Task 5 owns the discard
    /// overlay and decides whether to call `confirm_discard` before rebuilding this editor.
    pub fn observe_selected_pad_replaced(&mut self, context: SampleEditorContext) {
        if self.is_dirty() {
            self.status = SampleEditorStatus::Error(SampleEditorError::SelectedPadReplaced);
            return;
        }
        self.sync_context(context);
    }

    pub fn project(&self, width: u16) -> SampleProjection {
        let width = width.max(1);
        let project_marker = |phase: u64| {
            let offscreen = if phase < self.viewport.start {
                Some(OffscreenDirection::Left)
            } else if phase >= self.viewport.end {
                Some(OffscreenDirection::Right)
            } else {
                None
            };
            let phase = phase.clamp(self.viewport.start, self.viewport.end.saturating_sub(1));
            let local = phase.saturating_sub(self.viewport.start);
            let column = (u128::from(local) * u128::from(width)
                / u128::from(self.viewport.width()))
            .min(u128::from(width - 1));
            (
                u16::try_from(column).expect("bounded terminal column"),
                offscreen,
            )
        };
        let (start_column, start_offscreen) = project_marker(self.draft.start_phase);
        let (end_column, end_offscreen) = project_marker(self.draft.end_phase.saturating_sub(1));
        SampleProjection {
            visible: self.viewport.start..self.viewport.end,
            start_column,
            end_column,
            start_offscreen,
            end_offscreen,
        }
    }

    fn is_dirty(&self) -> bool {
        self.recipe_dirty() || self.settings_dirty()
    }

    fn recipe_dirty(&self) -> bool {
        self.committed
            .is_some_and(|committed| committed != self.draft)
    }

    fn settings_dirty(&self) -> bool {
        self.committed
            .is_some_and(|_| self.committed_settings != self.settings)
    }

    fn note_draft_change(&mut self) {
        if self.committed.is_some() {
            if self.is_dirty() {
                self.status = SampleEditorStatus::Dirty;
            } else {
                self.set_clean_status();
            }
        }
    }

    fn set_clean_status(&mut self) {
        self.status = if self.undo_available {
            SampleEditorStatus::UndoAvailable
        } else {
            SampleEditorStatus::Clean
        };
    }

    fn marker_frame(&self, frames: usize) -> usize {
        let phase = match self.marker {
            SampleMarker::Start => self.draft.start_phase,
            SampleMarker::End => self.draft.end_phase,
        };
        phase_to_frame(phase, frames, matches!(self.marker, SampleMarker::End))
    }

    fn set_marker_frame(&mut self, frames: usize, frame: usize) {
        let phase = frame_to_phase(frame, frames, matches!(self.marker, SampleMarker::End));
        match self.marker {
            SampleMarker::Start => {
                self.draft.start_phase = phase.min(self.draft.end_phase.saturating_sub(1))
            }
            SampleMarker::End => {
                self.draft.end_phase = phase.max(self.draft.start_phase.saturating_add(1))
            }
        }
    }

    fn coarse_frames(&self, frames: usize) -> usize {
        let scale = u128::from(SAMPLE_PHASE_SCALE);
        let source_frames = frames as u128;
        let start = u128::from(self.viewport.start)
            .checked_mul(source_frames)
            .expect("bounded Q32 source span multiplication")
            / scale;
        let end = u128::from(self.viewport.end)
            .checked_mul(source_frames)
            .expect("bounded Q32 source span multiplication")
            .div_ceil(scale);
        let span = end
            .checked_sub(start)
            .expect("half-open viewport is monotonic");
        usize::try_from(span.div_ceil(100).max(1)).unwrap_or(usize::MAX)
    }

    fn zoom_by(&mut self, delta: i8) {
        if self.committed.is_none() {
            return;
        }
        let zoom = if delta.is_negative() {
            self.zoom.saturating_sub(delta.unsigned_abs())
        } else {
            self.zoom.saturating_add(delta as u8).min(Self::MAX_ZOOM)
        };
        self.zoom = zoom;
        let width = (SAMPLE_PHASE_SCALE >> self.zoom).max(1);
        let anchor = match self.marker {
            SampleMarker::Start => self.draft.start_phase,
            SampleMarker::End => self.draft.end_phase.saturating_sub(1),
        };
        let start = anchor
            .saturating_sub(width / 2)
            .min(SAMPLE_PHASE_SCALE.saturating_sub(width));
        self.viewport = SampleViewport::new(start, start.saturating_add(width));
    }
}

fn phase_to_frame(phase: u64, frames: usize, ceil: bool) -> usize {
    let numerator = u128::from(phase).saturating_mul(frames as u128);
    let value = if ceil {
        numerator.div_ceil(u128::from(SAMPLE_PHASE_SCALE))
    } else {
        numerator / u128::from(SAMPLE_PHASE_SCALE)
    };
    usize::try_from(value.min(frames as u128)).expect("phase frame is bounded by usize input")
}

fn frame_to_phase(frame: usize, frames: usize, ceil: bool) -> u64 {
    let numerator = (frame as u128).saturating_mul(u128::from(SAMPLE_PHASE_SCALE));
    // Core maps a start with floor and an exclusive end with ceil. The inverse therefore uses
    // ceil for starts and floor for ends, preserving the requested source frame exactly.
    let value = if ceil {
        numerator / frames as u128
    } else {
        numerator.div_ceil(frames as u128)
    };
    u64::try_from(value.min(u128::from(SAMPLE_PHASE_SCALE))).expect("phase scale fits u64")
}

fn source_frames_supported(frames: usize) -> bool {
    (frames as u128) <= u128::from(SAMPLE_PHASE_SCALE)
}

#[cfg(test)]
mod tests {
    use sampler_core::{BankId, PadId, PadSettings, PlaybackMode, SampleEditRecipe};

    use super::{
        SampleEditor, SampleEditorContext, SampleEditorError, SampleEditorIntent,
        SampleEditorStatus, SampleMarker,
    };
    use crate::WorkspaceView;

    fn pad(index: u8) -> PadId {
        PadId::new(BankId::new(0).unwrap(), index).unwrap()
    }

    fn loaded(frames: usize) -> SampleEditor {
        SampleEditor::open_loaded(
            pad(0),
            SampleEditRecipe::identity(),
            frames,
            48_000,
            PadSettings::default(),
        )
    }

    fn replacement_context(
        recipe: SampleEditRecipe,
        frames: usize,
        rate: u32,
    ) -> SampleEditorContext {
        SampleEditorContext {
            pad: pad(0),
            committed: Some(recipe),
            base_frames: Some(frames),
            base_rate: Some(rate),
            settings: PadSettings::default(),
            edit_status: crate::SampleEditStatus::Idle,
            device_available: true,
        }
    }

    #[test]
    fn workspace_view_cycles_through_sample_in_both_directions() {
        assert_eq!(WorkspaceView::Perform.next(), WorkspaceView::Pattern);
        assert_eq!(WorkspaceView::Pattern.next(), WorkspaceView::Sample);
        assert_eq!(WorkspaceView::Sample.next(), WorkspaceView::Perform);
        assert_eq!(WorkspaceView::Perform.previous(), WorkspaceView::Sample);
        assert_eq!(WorkspaceView::Sample.previous(), WorkspaceView::Pattern);
    }

    #[test]
    fn opening_a_loaded_pad_uses_its_committed_recipe_and_empty_is_explicit() {
        let loaded = loaded(100);
        assert_eq!(loaded.status(), SampleEditorStatus::Clean);
        assert_eq!(loaded.draft(), SampleEditRecipe::identity());
        let empty = SampleEditor::open_empty(pad(1), PadSettings::default());
        assert_eq!(empty.status(), SampleEditorStatus::Empty);
        assert_eq!(empty.base_frames(), None);
    }

    #[test]
    fn loaded_constructor_uses_the_same_context_validation_as_open() {
        for (frames, rate, expected) in [
            (
                100,
                0,
                SampleEditorStatus::Error(SampleEditorError::InvalidSourceRate),
            ),
            (
                (sampler_core::SAMPLE_PHASE_SCALE as usize).saturating_add(1),
                48_000,
                SampleEditorStatus::Error(SampleEditorError::UnsupportedSourceLength),
            ),
            (100, 48_000, SampleEditorStatus::Clean),
        ] {
            let direct = SampleEditor::open_loaded(
                pad(0),
                SampleEditRecipe::identity(),
                frames,
                rate,
                PadSettings::default(),
            );
            let contextual = SampleEditor::open(replacement_context(
                SampleEditRecipe::identity(),
                frames,
                rate,
            ));
            assert_eq!(direct.status(), expected);
            assert_eq!(direct.status(), contextual.status());
            assert_eq!(direct.base_frames(), contextual.base_frames());
        }
    }

    #[test]
    fn fine_marker_movement_is_one_source_frame_even_for_one_frame_sources() {
        let mut editor = loaded(1);
        editor.move_marker(1, false);
        assert_eq!(editor.draft().start_phase, 0);
        editor.set_marker(SampleMarker::End);
        editor.move_marker(-1, false);
        assert_eq!(editor.draft().end_phase, sampler_core::SAMPLE_PHASE_SCALE);
    }

    #[test]
    fn marker_phase_conversion_round_trips_the_actual_core_frame_range() {
        for frames in [1, 100, 1_000, crate::MAX_PREPARED_FRAMES] {
            let mut editor = loaded(frames);
            editor.move_marker(1, false);
            let range = editor.draft().frame_range(frames).unwrap();
            assert_eq!(range.start, usize::from(frames > 1));

            editor.set_marker(SampleMarker::End);
            editor.move_marker(-1, false);
            let range = editor.draft().frame_range(frames).unwrap();
            assert_eq!(range.end, frames.saturating_sub(1).max(1));
        }
    }

    #[test]
    fn coarse_marker_movement_is_one_hundredth_of_visible_window_at_supported_boundaries() {
        for frames in [1, 100, 1_000, crate::MAX_PREPARED_FRAMES] {
            let mut editor = loaded(frames);
            editor.set_viewport(0, sampler_core::SAMPLE_PHASE_SCALE / 10);
            editor.move_marker(1, true);
            let visible = u128::from(sampler_core::SAMPLE_PHASE_SCALE / 10) * frames as u128
                / u128::from(sampler_core::SAMPLE_PHASE_SCALE);
            let expected = usize::try_from(visible.div_ceil(100).max(1))
                .unwrap()
                .min(frames.saturating_sub(1));
            assert_eq!(editor.draft().frame_range(frames).unwrap().start, expected);
        }
    }

    #[test]
    fn coarse_marker_uses_the_actual_half_open_visible_source_span() {
        let mut editor = loaded(1_000);
        editor.set_viewport(0, sampler_core::SAMPLE_PHASE_SCALE / 10 + 1);
        editor.move_marker(1, true);
        assert_eq!(editor.draft().frame_range(1_000).unwrap().start, 2);

        let mut nonzero = loaded(1_000);
        nonzero.set_viewport(
            sampler_core::SAMPLE_PHASE_SCALE / 2 + 1,
            sampler_core::SAMPLE_PHASE_SCALE / 2 + sampler_core::SAMPLE_PHASE_SCALE / 10 + 2,
        );
        nonzero.move_marker(1, true);
        assert_eq!(nonzero.draft().frame_range(1_000).unwrap().start, 2);

        for _ in 0..SampleEditor::MAX_ZOOM {
            nonzero.zoom_in();
        }
        nonzero.move_marker(1, true);
        assert!(nonzero.draft().frame_range(1_000).unwrap().start >= 3);
    }

    #[test]
    fn source_lengths_above_q32_precision_are_explicitly_unavailable() {
        let editor = loaded((sampler_core::SAMPLE_PHASE_SCALE as usize).saturating_add(1));
        assert_eq!(editor.base_frames(), None);
        assert_eq!(
            editor.status(),
            SampleEditorStatus::Error(SampleEditorError::UnsupportedSourceLength)
        );
    }

    #[test]
    fn replacement_reuses_context_validation_for_unsupported_then_recovered_sources() {
        let mut editor = loaded(100);
        editor.observe_selected_pad_replaced(replacement_context(
            SampleEditRecipe::identity(),
            (sampler_core::SAMPLE_PHASE_SCALE as usize).saturating_add(1),
            48_000,
        ));
        assert_eq!(editor.base_frames(), None);
        assert_eq!(
            editor.status(),
            SampleEditorStatus::Error(SampleEditorError::UnsupportedSourceLength)
        );
        editor.move_marker(1, false);
        assert_eq!(editor.draft(), SampleEditRecipe::identity());

        editor.observe_selected_pad_replaced(replacement_context(
            SampleEditRecipe::identity(),
            100,
            48_000,
        ));
        assert_eq!(editor.base_frames(), Some(100));
        assert_eq!(editor.status(), SampleEditorStatus::Clean);
    }

    #[test]
    fn replacement_requires_complete_context_without_synthesizing_a_sample_rate() {
        let mut editor = loaded(100);
        let unsupported = SampleEditorContext {
            pad: pad(0),
            committed: Some(SampleEditRecipe::identity()),
            base_frames: Some((sampler_core::SAMPLE_PHASE_SCALE as usize).saturating_add(1)),
            base_rate: Some(48_000),
            settings: PadSettings::default(),
            edit_status: crate::SampleEditStatus::Idle,
            device_available: true,
        };
        editor.observe_selected_pad_replaced(unsupported);
        assert_eq!(editor.base_rate(), None);

        editor.observe_selected_pad_replaced(SampleEditorContext {
            base_frames: Some(100),
            base_rate: Some(44_100),
            ..unsupported
        });
        assert_eq!(editor.base_rate(), Some(44_100));

        editor.observe_selected_pad_replaced(SampleEditorContext {
            base_frames: Some(100),
            base_rate: Some(0),
            ..unsupported
        });
        assert_eq!(
            editor.status(),
            SampleEditorStatus::Error(SampleEditorError::InvalidSourceRate)
        );

        let mut empty = SampleEditor::open_empty(pad(0), PadSettings::default());
        empty.observe_selected_pad_replaced(SampleEditorContext {
            base_frames: Some(100),
            base_rate: Some(48_000),
            ..unsupported
        });
        assert_eq!(empty.base_rate(), Some(48_000));
    }

    #[test]
    fn markers_clamp_to_a_nonempty_source_range_at_usize_max() {
        let mut editor = loaded(usize::MAX);
        editor.set_marker(SampleMarker::End);
        editor.move_marker(i8::MIN, false);
        assert!(
            editor.draft().frame_range(usize::MAX).unwrap().start
                < editor.draft().frame_range(usize::MAX).unwrap().end
        );
        editor.set_marker(SampleMarker::Start);
        editor.move_marker(i8::MAX, false);
        assert!(editor.draft().validate().is_ok());
    }

    #[test]
    fn zoom_is_bounded_and_keeps_the_active_marker_visible() {
        let mut editor = loaded(1_000);
        editor.set_marker(SampleMarker::End);
        editor.zoom_in();
        assert!(editor.viewport().start <= editor.draft().end_phase);
        assert!(editor.draft().end_phase <= editor.viewport().end);
        for _ in 0..64 {
            editor.zoom_in();
        }
        assert!(editor.zoom_level() <= SampleEditor::MAX_ZOOM);
    }

    #[test]
    fn recipe_toggles_and_settings_bounds_are_reversible() {
        let mut editor = loaded(100);
        editor.toggle_normalize();
        editor.toggle_reverse();
        editor.adjust_pitch(99);
        editor.set_mode(PlaybackMode::Loop);
        assert!(editor.draft().normalize && editor.draft().reversed);
        assert_eq!(editor.settings().pitch_semitones, 24.0);
        assert_eq!(editor.settings().mode, PlaybackMode::Loop);
        editor.toggle_normalize();
        editor.toggle_reverse();
        editor.adjust_pitch(-99);
        assert!(!editor.draft().normalize && !editor.draft().reversed);
        assert_eq!(editor.settings().pitch_semitones, -24.0);
    }

    #[test]
    fn clean_escape_returns_and_dirty_escape_requests_discard() {
        let mut editor = loaded(100);
        assert_eq!(editor.escape(), SampleEditorIntent::ReturnToPerform);
        editor.toggle_reverse();
        assert_eq!(
            editor.escape(),
            SampleEditorIntent::ConfirmDiscard { pad: pad(0) }
        );
        assert_eq!(editor.status(), SampleEditorStatus::DiscardConfirmation);
    }

    #[test]
    fn apply_is_an_intent_and_app_context_exposes_the_single_undo_checkpoint() {
        let mut editor = loaded(100);
        editor.toggle_reverse();
        let recipe = editor.draft();
        assert_eq!(
            editor.request_apply(),
            Some(SampleEditorIntent::Apply {
                pad: pad(0),
                recipe
            })
        );
        assert_eq!(editor.status(), SampleEditorStatus::ApplyConfirmation);
        editor.observe_apply_succeeded();
        assert_eq!(editor.status(), SampleEditorStatus::Clean);
        editor.sync_context(SampleEditorContext {
            pad: pad(0),
            committed: Some(recipe),
            base_frames: Some(100),
            base_rate: Some(48_000),
            settings: PadSettings::default(),
            edit_status: crate::SampleEditStatus::UndoAvailable,
            device_available: true,
        });
        assert_eq!(editor.status(), SampleEditorStatus::UndoAvailable);
        assert_eq!(
            editor.request_undo(),
            Some(SampleEditorIntent::Undo { pad: pad(0) })
        );
        editor.observe_undo_succeeded();
        assert_eq!(editor.status(), SampleEditorStatus::UndoAvailable);
    }

    #[test]
    fn pending_failed_and_replaced_states_preserve_the_draft() {
        let mut editor = loaded(100);
        editor.toggle_reverse();
        let draft = editor.draft();
        editor.observe_pending();
        assert_eq!(editor.status(), SampleEditorStatus::Pending);
        editor.observe_error(SampleEditorError::AudioQueueFull);
        assert_eq!(
            editor.status(),
            SampleEditorStatus::Error(SampleEditorError::AudioQueueFull)
        );
        editor.observe_selected_pad_replaced(replacement_context(
            SampleEditRecipe::identity(),
            200,
            48_000,
        ));
        assert_eq!(editor.draft(), draft);
        assert_eq!(
            editor.status(),
            SampleEditorStatus::Error(SampleEditorError::SelectedPadReplaced)
        );
    }

    #[test]
    fn app_context_maps_busy_failure_and_device_states_without_copying_pending_truth() {
        let context = SampleEditorContext {
            pad: pad(0),
            committed: Some(SampleEditRecipe::identity()),
            base_frames: Some(100),
            base_rate: Some(48_000),
            settings: PadSettings::default(),
            edit_status: crate::SampleEditStatus::AwaitingWorker,
            device_available: true,
        };
        let mut editor = SampleEditor::open(context);
        assert_eq!(editor.status(), SampleEditorStatus::Pending);
        editor.sync_context(SampleEditorContext {
            edit_status: crate::SampleEditStatus::Failed,
            ..context
        });
        assert_eq!(
            editor.status(),
            SampleEditorStatus::Error(SampleEditorError::InstallFailed)
        );
        editor.sync_context(SampleEditorContext {
            device_available: false,
            ..context
        });
        assert_eq!(
            editor.status(),
            SampleEditorStatus::Error(SampleEditorError::DeviceUnavailable)
        );
    }

    #[test]
    fn same_pad_rebase_keeps_dirty_recipe_across_rate_recovery_and_reports_external_conflicts() {
        let context = SampleEditorContext {
            pad: pad(0),
            committed: Some(SampleEditRecipe::identity()),
            base_frames: Some(1_000),
            base_rate: Some(48_000),
            settings: PadSettings::default(),
            edit_status: crate::SampleEditStatus::Idle,
            device_available: true,
        };
        let mut editor = SampleEditor::open(context);
        editor.move_marker(1, false);
        editor.toggle_reverse();
        let draft_start = editor.draft().start_phase;
        editor.sync_context(SampleEditorContext {
            base_frames: Some(900),
            base_rate: Some(44_100),
            ..context
        });
        assert!(editor.draft().reversed);
        assert_eq!(editor.draft().start_phase, draft_start);
        assert_eq!(editor.base_frames(), Some(900));
        assert_eq!(editor.base_rate(), Some(44_100));

        let replacement =
            SampleEditRecipe::new(10, sampler_core::SAMPLE_PHASE_SCALE, false, false).unwrap();
        editor.sync_context(SampleEditorContext {
            committed: Some(replacement),
            ..context
        });
        assert_eq!(editor.committed(), Some(replacement));
        assert!(editor.draft().reversed);
        assert_eq!(
            editor.status(),
            SampleEditorStatus::Error(SampleEditorError::SelectedPadReplaced)
        );
    }

    #[test]
    fn context_rebase_updates_clean_fields_and_preserves_only_dirty_settings_or_recipe_fields() {
        let context = SampleEditorContext {
            pad: pad(0),
            committed: Some(SampleEditRecipe::identity()),
            base_frames: Some(100),
            base_rate: Some(48_000),
            settings: PadSettings::default(),
            edit_status: crate::SampleEditStatus::Idle,
            device_available: true,
        };
        let mut clean = SampleEditor::open(context);
        let external = PadSettings {
            pitch_semitones: 7.0,
            mode: PlaybackMode::Gate,
            ..PadSettings::default()
        };
        clean.sync_context(SampleEditorContext {
            settings: external,
            ..context
        });
        assert_eq!(clean.settings(), external);

        let mut recipe_dirty = SampleEditor::open(context);
        recipe_dirty.toggle_reverse();
        recipe_dirty.sync_context(SampleEditorContext {
            settings: external,
            ..context
        });
        assert!(recipe_dirty.draft().reversed);
        assert_eq!(recipe_dirty.settings(), external);

        let mut settings_dirty = SampleEditor::open(context);
        settings_dirty.adjust_pitch(3);
        settings_dirty.sync_context(SampleEditorContext {
            settings: external,
            ..context
        });
        assert_eq!(settings_dirty.settings().pitch_semitones, 3.0);
        assert_eq!(
            settings_dirty.committed(),
            Some(SampleEditRecipe::identity())
        );
        settings_dirty.confirm_discard();
        assert_eq!(settings_dirty.settings(), external);
    }

    #[test]
    fn undo_is_reopened_and_enabled_only_from_app_context() {
        let context = SampleEditorContext {
            pad: pad(0),
            committed: Some(SampleEditRecipe::identity()),
            base_frames: Some(100),
            base_rate: Some(48_000),
            settings: PadSettings::default(),
            edit_status: crate::SampleEditStatus::UndoAvailable,
            device_available: true,
        };
        let mut editor = SampleEditor::open(context);
        assert_eq!(editor.status(), SampleEditorStatus::UndoAvailable);
        assert_eq!(
            editor.request_undo(),
            Some(SampleEditorIntent::Undo { pad: pad(0) })
        );
        editor.sync_context(SampleEditorContext {
            edit_status: crate::SampleEditStatus::Idle,
            ..context
        });
        assert_eq!(editor.status(), SampleEditorStatus::Clean);
        assert_eq!(editor.request_undo(), None);
    }

    #[test]
    fn every_clean_reducer_path_keeps_app_owned_undo_available() {
        let context = SampleEditorContext {
            pad: pad(0),
            committed: Some(SampleEditRecipe::identity()),
            base_frames: Some(100),
            base_rate: Some(48_000),
            settings: PadSettings::default(),
            edit_status: crate::SampleEditStatus::UndoAvailable,
            device_available: true,
        };
        let mut editor = SampleEditor::open(context);
        editor.toggle_reverse();
        editor.toggle_reverse();
        assert_eq!(editor.status(), SampleEditorStatus::UndoAvailable);
        editor.toggle_normalize();
        editor.toggle_normalize();
        assert_eq!(editor.status(), SampleEditorStatus::UndoAvailable);
        editor.adjust_pitch(1);
        editor.adjust_pitch(-1);
        editor.set_mode(PlaybackMode::Gate);
        editor.set_mode(PlaybackMode::OneShot);
        assert_eq!(editor.status(), SampleEditorStatus::UndoAvailable);
        assert_eq!(
            editor.request_undo(),
            Some(SampleEditorIntent::Undo { pad: pad(0) })
        );
    }

    #[test]
    fn apply_and_undo_failures_keep_the_draft_and_each_typed_error_retryable() {
        let mut editor = loaded(100);
        editor.toggle_reverse();
        let draft = editor.draft();
        let _ = editor.request_apply();
        for error in [
            SampleEditorError::WorkerBusy,
            SampleEditorError::WorkerClosed,
            SampleEditorError::StaleResult,
            SampleEditorError::AudioQueueFull,
            SampleEditorError::InstallFailed,
        ] {
            editor.observe_apply_failed(error.clone());
            assert_eq!(editor.status(), SampleEditorStatus::Error(error));
            assert_eq!(editor.draft(), draft);
        }
        editor.observe_apply_succeeded();
        editor.sync_context(SampleEditorContext {
            pad: pad(0),
            committed: Some(draft),
            base_frames: Some(100),
            base_rate: Some(48_000),
            settings: PadSettings::default(),
            edit_status: crate::SampleEditStatus::UndoAvailable,
            device_available: true,
        });
        let _ = editor.request_undo();
        editor.observe_undo_failed(SampleEditorError::AudioQueueFull);
        assert_eq!(editor.draft(), draft);
        assert_eq!(
            editor.status(),
            SampleEditorStatus::Error(SampleEditorError::AudioQueueFull)
        );
        assert!(editor.request_undo().is_some());
    }

    #[test]
    fn projection_is_monotonic_for_every_terminal_width_and_marks_offscreen_direction() {
        let mut editor = loaded(100);
        editor.set_viewport(
            sampler_core::SAMPLE_PHASE_SCALE / 4,
            sampler_core::SAMPLE_PHASE_SCALE / 2,
        );
        for width in 1..=u16::MAX {
            let projection = editor.project(width);
            assert!(projection.start_column < width);
            assert!(projection.end_column < width);
            assert!(projection.visible.start < projection.visible.end);
        }
        assert_eq!(
            editor.project(10).start_offscreen,
            Some(super::OffscreenDirection::Left)
        );
    }
}
