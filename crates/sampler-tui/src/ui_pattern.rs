use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use sampler_core::{EditablePattern, PadId, PatternEvent, PatternSlotId, Resolution, Transport};

use crate::App;

const PAD_KEYS: [char; 16] = [
    '1', '2', '3', '4', 'Q', 'W', 'E', 'R', 'A', 'S', 'D', 'F', 'Z', 'X', 'C', 'V',
];
const STEPS_PER_BAR: u32 = 16;

#[derive(Clone, Copy)]
struct GridProjection {
    transport: Transport,
    bar: u16,
    steps_per_bar: u32,
    start_step: u32,
    bounds: BarBounds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BarBounds {
    pub(crate) start: u64,
    pub(crate) end: u64,
}

pub(crate) fn transport_bar_bounds(transport: Transport, bar: u16) -> BarBounds {
    let bars = transport.bars().max(1);
    let bar = bar.min(bars.saturating_sub(1));
    let steps_per_bar = (transport.step_count() / u32::from(bars)).max(1);
    let start_step = u32::from(bar) * steps_per_bar;
    let next_step = start_step.saturating_add(steps_per_bar);
    BarBounds {
        start: transport.step_frame(start_step),
        end: if bar.saturating_add(1) == bars {
            transport.loop_frames()
        } else {
            transport.step_frame(next_step)
        },
    }
}

pub(crate) fn transport_bar_index(transport: Transport, playhead: u64) -> u64 {
    let loop_frames = transport.loop_frames();
    if loop_frames == 0 || transport.bars() == 0 {
        return 1;
    }
    let playhead = playhead % loop_frames;
    for bar in 0..transport.bars() {
        let bounds = transport_bar_bounds(transport, bar);
        if playhead >= bounds.start && playhead < bounds.end {
            return u64::from(bar) + 1;
        }
    }
    u64::from(transport.bars())
}

impl GridProjection {
    fn new(transport: Transport, bar: u16) -> Self {
        let bars = transport.bars().max(1);
        let bar = bar.min(bars.saturating_sub(1));
        let steps_per_bar = (transport.step_count() / u32::from(bars)).max(1);
        let bounds = transport_bar_bounds(transport, bar);
        Self {
            transport,
            bar,
            steps_per_bar,
            start_step: u32::from(bar) * steps_per_bar,
            bounds,
        }
    }

    #[cfg(test)]
    fn steps_per_bar(self) -> u32 {
        self.steps_per_bar
    }

    #[cfg(test)]
    fn bar_start_frame(self) -> u64 {
        self.bounds.start
    }

    fn column_for_step(self, step: u32) -> Option<u32> {
        (step >= self.start_step && step < self.start_step + self.steps_per_bar)
            .then(|| self.column_for_frame(self.transport.step_frame(step)))
            .flatten()
    }

    fn column_for_frame(self, frame: u64) -> Option<u32> {
        let local = frame.checked_sub(self.bounds.start)?;
        let length = self.bounds.end.saturating_sub(self.bounds.start);
        (local < length && length != 0).then(|| {
            u32::try_from(u128::from(local) * u128::from(STEPS_PER_BAR) / u128::from(length))
                .expect("sixteen-column projection fits in u32")
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct LiveIdentity {
    pub(crate) slot: PatternSlotId,
    pub(crate) playing: bool,
    pub(crate) recording: bool,
}

/// Pure Pattern-workspace view. Editing and audio commands remain in `App`.
pub(crate) fn render_pattern(frame: &mut Frame, area: Rect, app: &App) {
    let workspace = app.patterns();
    let pattern = workspace.selected_pattern();
    let transport = pattern.transport();
    let telemetry = app.telemetry();
    let identity = live_identity(app);
    let live = identity.and_then(|identity| matching_live_pattern(app, identity));
    let selected_is_live = live.is_some_and(|(_, slot, _)| slot == workspace.selected_slot());
    let recording = identity
        .is_some_and(|identity| identity.slot == workspace.selected_slot() && identity.recording);
    let playing = identity
        .is_some_and(|identity| identity.slot == workspace.selected_slot() && identity.playing);
    let state = if recording {
        "REC"
    } else if playing {
        "PLAY"
    } else {
        "STOP"
    };
    let outer = Block::new()
        .borders(Borders::ALL)
        .title(format!(
            " PATTERN {:02} ",
            workspace.selected_slot().get() + 1
        ))
        .title(live_state_label(identity).to_string());
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    if inner.is_empty() {
        return;
    }

    let cursor = workspace.cursor();
    let projection = GridProjection::new(transport, cursor.bar());
    let bar = projection.bar;
    let selected_event = workspace.selected_event().map(|event| event.id);
    let playhead = selected_is_live
        .then(|| projection.column_for_frame(telemetry.pattern_playhead % transport.loop_frames()))
        .flatten();
    line(
        frame,
        inner,
        0,
        format!(
            "{:.1} BPM · {}/{} · BAR {}/{} · {} · SW {:02}% · Q {:03}% · {state} · REC {}",
            transport.tempo().bpm(),
            transport.meter().numerator(),
            transport.meter().denominator(),
            bar + 1,
            transport.bars(),
            resolution_name(transport.resolution()),
            (transport.swing() * 100.0).round() as u8,
            (pattern.quantize_strength() * 100.0).round() as u8,
            if recording { "ON" } else { "OFF" },
        ),
    );
    line(
        frame,
        inner,
        2,
        "     01 02 03 04 05 06 07 08 09 10 11 12 13 14 15 16".to_owned(),
    );

    for row in 0..16u8 {
        let y = 3 + u16::from(row);
        if y >= inner.height {
            break;
        }
        let pad = PadId::new(app.active_bank(), row).expect("active bank pad row is valid");
        let mut spans = vec![Span::raw(format!(
            "{:02} {} ",
            row + 1,
            PAD_KEYS[usize::from(row)]
        ))];
        for column in 0..STEPS_PER_BAR {
            let (mut glyph, count) =
                event_glyph(pattern.events(), projection, pad, column, selected_event);
            let is_playhead = playhead == Some(column);
            if is_playhead {
                glyph = '>';
            }
            let is_cursor =
                cursor.pad() == pad && projection.column_for_step(cursor.step()) == Some(column);
            spans.push(Span::styled(
                glyph.to_string(),
                cell_style(is_cursor, is_playhead, count > 1),
            ));
        }
        if inner.width >= 100 {
            spans.push(Span::raw(metadata(pattern.events(), projection, pad)));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(inner.x, inner.y + y, inner.width, 1),
        );
    }
    line(
        frame,
        inner,
        19,
        format!(
            "Events {}/1024 · overflow {} · updates {}",
            pattern.events().len(),
            telemetry.pattern_overflows,
            if workspace.has_pending_snapshot(workspace.selected_slot()) {
                "pending"
            } else {
                "current"
            }
        ),
    );
    line(
        frame,
        inner,
        20,
        "Arrows cursor · PgUp/PgDn bar · Enter toggle · Del remove · +/- velocity · u undo"
            .to_owned(),
    );
    line(
        frame,
        inner,
        21,
        "Tab Perform · Space play/stop · Ctrl+R record · ,/. pattern · ? help · : command"
            .to_owned(),
    );
}

pub(crate) fn live_identity(app: &App) -> Option<LiveIdentity> {
    let telemetry = app.telemetry();
    let slot = telemetry.pattern_slot?;
    Some(LiveIdentity {
        slot,
        playing: telemetry.pattern_playing,
        recording: telemetry.pattern_recording,
    })
}

fn matching_live_pattern(
    app: &App,
    identity: LiveIdentity,
) -> Option<(LiveIdentity, PatternSlotId, &EditablePattern)> {
    let generation = app.telemetry().pattern_generation?;
    app.patterns()
        .pattern_for_generation(identity.slot, generation)
        .map(|pattern| (identity, identity.slot, pattern))
}

pub(crate) fn live_pattern(app: &App) -> Option<(PatternSlotId, &EditablePattern)> {
    live_identity(app)
        .and_then(|identity| matching_live_pattern(app, identity))
        .map(|(_, slot, pattern)| (slot, pattern))
}

fn live_state_label(identity: Option<LiveIdentity>) -> String {
    let Some(identity) = identity else {
        return " STOP ".to_owned();
    };
    let state = if identity.recording {
        "REC"
    } else if identity.playing {
        "PLAY"
    } else {
        "STOP"
    };
    format!(" P{:02} {state} ", identity.slot.get() + 1)
}

fn line(frame: &mut Frame, area: Rect, row: u16, value: String) {
    if row < area.height {
        frame.render_widget(
            Paragraph::new(value),
            Rect::new(area.x, area.y + row, area.width, 1),
        );
    }
}

fn resolution_name(resolution: Resolution) -> &'static str {
    match resolution {
        Resolution::Quarter => "1/4",
        Resolution::Eighth => "1/8",
        Resolution::Sixteenth => "1/16",
        Resolution::ThirtySecond => "1/32",
    }
}

fn event_glyph(
    events: &[PatternEvent],
    projection: GridProjection,
    pad: PadId,
    column: u32,
    selected: Option<sampler_core::EventId>,
) -> (char, usize) {
    let mut count = 0;
    let mut selected_here = false;
    for event in events {
        if event.pad == pad && projection.column_for_frame(event.frame) == Some(column) {
            count += 1;
            selected_here |= Some(event.id) == selected;
        }
    }
    (
        if count == 0 {
            '.'
        } else if count > 1 {
            '!'
        } else if selected_here {
            'O'
        } else {
            'o'
        },
        count,
    )
}

fn cell_style(cursor: bool, playhead: bool, overflow: bool) -> Style {
    let mut modifiers = Modifier::empty();
    if cursor {
        modifiers |= Modifier::REVERSED;
    }
    if playhead {
        modifiers |= Modifier::UNDERLINED | Modifier::BOLD;
    }
    if overflow {
        modifiers |= Modifier::DIM;
    }
    Style::default().add_modifier(modifiers)
}

fn metadata(events: &[PatternEvent], projection: GridProjection, pad: PadId) -> String {
    let matching = events
        .iter()
        .filter(|event| event.pad == pad && projection.column_for_frame(event.frame) == Some(0));
    let mut count = 0;
    let mut velocity = 0;
    for event in matching {
        count += 1;
        velocity = (event.velocity * 100.0).round() as u8;
    }
    format!("  v{velocity:03}% e{count}")
}

#[cfg(test)]
mod tests {
    use sampler_core::{BankId, EventId, Meter, PadId, PatternEvent, Resolution, Tempo, Transport};

    use super::{GridProjection, event_glyph, transport_bar_bounds, transport_bar_index};

    fn transport(resolution: Resolution) -> Transport {
        Transport::new(
            48_000,
            Tempo::new(120.0).unwrap(),
            Meter::new(4, 4).unwrap(),
            2,
            resolution,
        )
        .unwrap()
    }

    #[test]
    fn sixteen_columns_project_every_advertised_resolution_by_bar_phase() {
        for (resolution, steps_per_bar, last_column) in [
            (Resolution::Quarter, 4, 12),
            (Resolution::Eighth, 8, 14),
            (Resolution::Sixteenth, 16, 15),
            (Resolution::ThirtySecond, 32, 15),
        ] {
            let projection = GridProjection::new(transport(resolution), 1);
            assert_eq!(projection.steps_per_bar(), steps_per_bar);
            assert_eq!(projection.column_for_step(steps_per_bar), Some(0));
            assert_eq!(
                projection.column_for_step(steps_per_bar * 2 - 1),
                Some(last_column)
            );
            assert_eq!(
                projection.column_for_frame(projection.bar_start_frame()),
                Some(0)
            );
        }
    }

    #[test]
    fn eighth_note_second_bar_cursor_step_eight_projects_to_first_visible_column() {
        let projection = GridProjection::new(transport(Resolution::Eighth), 1);
        assert_eq!(projection.column_for_step(8), Some(0));
        assert_eq!(projection.column_for_step(7), None);
    }

    #[test]
    fn event_and_playhead_share_the_same_phase_projection_at_every_resolution() {
        let pad = PadId::new(BankId::new(0).unwrap(), 0).unwrap();
        for resolution in [
            Resolution::Quarter,
            Resolution::Eighth,
            Resolution::Sixteenth,
            Resolution::ThirtySecond,
        ] {
            let projection = GridProjection::new(transport(resolution), 1);
            let frame = projection.bounds.end - 1;
            let event = PatternEvent::new(EventId(1), pad, frame, 1.0, None).unwrap();
            let column = projection.column_for_frame(frame).unwrap();

            assert_eq!(column, 15);
            assert_eq!(
                event_glyph(&[event], projection, pad, column, None),
                ('o', 1)
            );
        }
    }

    #[test]
    fn swung_cursor_and_toggled_event_share_the_same_projected_column() {
        let pad = PadId::new(BankId::new(0).unwrap(), 0).unwrap();
        for resolution in [Resolution::Quarter, Resolution::Eighth] {
            let transport = transport(resolution).with_swing(0.75).unwrap();
            let projection = GridProjection::new(transport, 0);
            let frame = transport.step_frame(1);
            let event = PatternEvent::new(EventId(1), pad, frame, 1.0, None).unwrap();
            let column = projection.column_for_step(1).unwrap();

            assert_eq!(column, projection.column_for_frame(frame).unwrap());
            assert_eq!(
                event_glyph(&[event], projection, pad, column, None),
                ('o', 1)
            );
        }
    }

    #[test]
    fn transport_grid_boundaries_are_half_open_and_nonoverlapping_at_every_resolution() {
        for resolution in [
            Resolution::Quarter,
            Resolution::Eighth,
            Resolution::Sixteenth,
            Resolution::ThirtySecond,
        ] {
            let transport = Transport::new(
                48_000,
                Tempo::new(20.1).unwrap(),
                Meter::new(4, 4).unwrap(),
                3,
                resolution,
            )
            .unwrap();
            let first = transport_bar_bounds(transport, 0);
            let next = transport_bar_bounds(transport, 1);

            assert_eq!(first.end, next.start);
            assert_eq!(next.start, transport.step_frame(transport.step_count() / 3));
            assert_eq!(transport_bar_index(transport, next.start - 1), 1);
            assert_eq!(transport_bar_index(transport, next.start), 2);
        }
    }
}
