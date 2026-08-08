use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use sampler_core::{PadId, PatternEvent, Resolution, Transport};

use crate::App;

const PAD_KEYS: [char; 16] = [
    '1', '2', '3', '4', 'Q', 'W', 'E', 'R', 'A', 'S', 'D', 'F', 'Z', 'X', 'C', 'V',
];
const STEPS_PER_BAR: u32 = 16;

/// Pure Pattern-workspace view. Editing and audio commands remain in `App`.
pub(crate) fn render_pattern(frame: &mut Frame, area: Rect, app: &App) {
    let workspace = app.patterns();
    let pattern = workspace.selected_pattern();
    let transport = pattern.transport();
    let telemetry = app.telemetry();
    let recording = workspace.is_recording() || telemetry.pattern_recording;
    let playing = workspace.is_playing() || telemetry.pattern_playing;
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
        .title(format!(" {state} ").to_string());
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    if inner.is_empty() {
        return;
    }

    let cursor = workspace.cursor();
    let bar = cursor.bar().min(transport.bars().saturating_sub(1));
    let first_step = u32::from(bar) * STEPS_PER_BAR;
    let selected_event = workspace.selected_event().map(|event| event.id);
    let playhead = playhead_step(transport, telemetry.pattern_playhead);
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
        for local_step in 0..STEPS_PER_BAR {
            let step = first_step + local_step;
            let (mut glyph, count) =
                event_glyph(pattern.events(), transport, pad, step, selected_event);
            let is_playhead = playhead == Some(step);
            if is_playhead {
                glyph = '>';
            }
            let is_cursor = cursor.pad() == pad && cursor.step() == step;
            spans.push(Span::styled(
                glyph.to_string(),
                cell_style(is_cursor, is_playhead, count > 1),
            ));
        }
        if inner.width >= 100 {
            spans.push(Span::raw(metadata(
                pattern.events(),
                transport,
                pad,
                first_step,
            )));
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

fn playhead_step(transport: Transport, playhead: u64) -> Option<u32> {
    (transport.loop_frames() != 0).then(|| {
        let playhead = playhead % transport.loop_frames();
        (0..transport.step_count())
            .take_while(|step| transport.step_frame(*step) <= playhead)
            .last()
            .unwrap_or(0)
    })
}

fn event_glyph(
    events: &[PatternEvent],
    transport: Transport,
    pad: PadId,
    step: u32,
    selected: Option<sampler_core::EventId>,
) -> (char, usize) {
    let start = transport.step_frame(step);
    let end = transport.step_frame(step + 1);
    let mut count = 0;
    let mut selected_here = false;
    for event in events {
        if event.pad == pad && event.frame >= start && event.frame < end {
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

fn metadata(events: &[PatternEvent], transport: Transport, pad: PadId, step: u32) -> String {
    let start = transport.step_frame(step);
    let end = transport.step_frame(step + 1);
    let matching = events
        .iter()
        .filter(|event| event.pad == pad && event.frame >= start && event.frame < end);
    let mut count = 0;
    let mut velocity = 0;
    for event in matching {
        count += 1;
        velocity = (event.velocity * 100.0).round() as u8;
    }
    format!("  v{velocity:03}% e{count}")
}
