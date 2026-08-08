use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use sampler_core::PlaybackMode;

use crate::{
    App, EDIT_PREVIEW_COLUMNS, OffscreenDirection, PreviewColumn, SampleMarker,
    WorkspaceSampleEditorStatus,
};

const MAX_WAVE_BLOCK_WIDTH: u16 = 78;
const WAVE_HEIGHT: u16 = 9;

/// Renders only cached App projections and the worker-produced fixed preview. In particular,
/// this code must never inspect the PCM buffer while a terminal frame is being drawn.
pub(crate) fn render_sample(frame: &mut Frame, area: Rect, app: &App) {
    frame.render_widget(Clear, area);
    let outer = Block::new().borders(Borders::ALL).title(" SAMPLE EDITOR ");
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    if inner.is_empty() {
        return;
    }

    let editor = app.sample_editor();
    let offset = usize::from(u8::from(app.active_bank())) * 16 + app.selected_pad().min(15);
    let pad = &app.pads()[offset];
    let source = app
        .pad_display_source(offset)
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().to_uppercase())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "----".to_owned());
    let base_frames = editor.base_frames();
    let base_rate = editor.base_rate();
    let range = base_frames.and_then(|frames| editor.draft().frame_range(frames).ok());
    let visible = editor.project(MAX_WAVE_BLOCK_WIDTH.saturating_sub(2).min(inner.width));
    let visible_frames = base_frames
        .map(|frames| phase_range_to_frames(visible.visible.start, visible.visible.end, frames));

    render_line(
        frame,
        inner,
        0,
        format!(
            " PAD {:02} · {} · {}",
            app.selected_pad() + 1,
            source,
            load_state_name(&pad.state)
        ),
        Style::default().add_modifier(Modifier::BOLD),
    );
    render_line(
        frame,
        inner,
        1,
        format!(
            " FRAME {} · VISIBLE {} · ZOOM {}",
            frame_range_text(range.clone()),
            visible_range_text(visible_frames, base_rate),
            editor.zoom_level()
        ),
        Style::default(),
    );

    let wave_width = inner.width.min(MAX_WAVE_BLOCK_WIDTH);
    let wave_area = Rect::new(inner.x, inner.y.saturating_add(3), wave_width, WAVE_HEIGHT);
    render_waveform(frame, wave_area, app);

    render_line(
        frame,
        inner,
        13,
        format!(
            " DRAFT {} · ACTIVE {} · START {} · END {}",
            editor_status_name(&editor.status()),
            marker_name(editor.marker()),
            marker_frame(editor.draft().start_phase, base_frames, false),
            marker_frame(editor.draft().end_phase, base_frames, true),
        ),
        draft_style(&editor.status()),
    );
    render_line(
        frame,
        inner,
        14,
        format!(
            " NORMALIZE {} · REVERSE {} · PITCH {:+.0} · MODE {}",
            on_off(editor.draft().normalize),
            on_off(editor.draft().reversed),
            editor.settings().pitch_semitones,
            mode_name(editor.settings().mode),
        ),
        Style::default(),
    );
    render_line(
        frame,
        inner,
        16,
        sample_status_line(app.status(), editor.status()),
        Style::default().fg(Color::Yellow),
    );
    render_line(
        frame,
        inner,
        18,
        " Left/Right trim · Shift coarse · m marker · PgUp/PgDn zoom",
        Style::default().add_modifier(Modifier::DIM),
    );
    render_line(
        frame,
        inner,
        19,
        " n normalize · u reverse · Up/Down pitch · o/g/l mode",
        Style::default().add_modifier(Modifier::DIM),
    );
    render_line(
        frame,
        inner,
        20,
        " Enter apply · Ctrl+Z undo · Esc back · source file unchanged",
        Style::default().add_modifier(Modifier::DIM),
    );

    if inner.width > MAX_WAVE_BLOCK_WIDTH {
        let metadata = Rect::new(
            inner.x.saturating_add(MAX_WAVE_BLOCK_WIDTH),
            inner.y.saturating_add(3),
            inner.width.saturating_sub(MAX_WAVE_BLOCK_WIDTH),
            inner.height.saturating_sub(3),
        );
        render_line(
            frame,
            metadata,
            0,
            format!(" SOURCE {}kHz", base_rate.unwrap_or(0) / 1_000),
            Style::default(),
        );
        render_line(
            frame,
            metadata,
            1,
            format!(" {EDIT_PREVIEW_COLUMNS} PREVIEW"),
            Style::default(),
        );
    }
}

fn render_waveform(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::new().borders(Borders::ALL).title(" WAVEFORM ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let editor = app.sample_editor();
    let projection = editor.project(inner.width);
    let preview = app.edit_preview(editor.pad());
    let rows = usize::from(inner.height);
    let columns = usize::from(inner.width);
    for row in 0..rows {
        let mut spans = Vec::with_capacity(columns);
        for column in 0..columns {
            let index = preview_index(
                projection.visible.start,
                projection.visible.end,
                column,
                columns,
            );
            let value = preview
                .and_then(|preview| preview.get(index))
                .copied()
                .unwrap_or_default();
            let (mut glyph, mut style) = waveform_cell(value, row, rows);
            let column = u16::try_from(column).expect("terminal column fits u16");
            if let Some((marker, marker_style)) = marker_cell(editor.marker(), &projection, column)
            {
                glyph = marker;
                style = marker_style;
            }
            spans.push(Span::styled(glyph.to_string(), style));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(
                inner.x,
                inner
                    .y
                    .saturating_add(u16::try_from(row).unwrap_or(u16::MAX)),
                inner.width,
                1,
            ),
        );
    }
}

fn preview_index(visible_start: u64, visible_end: u64, column: usize, columns: usize) -> usize {
    if columns == 0 {
        return 0;
    }
    let span = visible_end.saturating_sub(visible_start).max(1);
    let phase = u128::from(visible_start).saturating_add(
        u128::from(span)
            .saturating_mul(u128::try_from(column).unwrap_or(u128::MAX))
            .saturating_add(u128::try_from(columns / 2).unwrap_or(0))
            / u128::try_from(columns).unwrap_or(1),
    );
    usize::try_from(
        phase.saturating_mul(u128::try_from(EDIT_PREVIEW_COLUMNS).unwrap_or(0))
            / u128::from(sampler_core::SAMPLE_PHASE_SCALE),
    )
    .unwrap_or(EDIT_PREVIEW_COLUMNS.saturating_sub(1))
    .min(EDIT_PREVIEW_COLUMNS.saturating_sub(1))
}

fn waveform_cell(value: PreviewColumn, row: usize, rows: usize) -> (char, Style) {
    if value.min == 0 && value.max == 0 {
        return (' ', Style::default());
    }
    let middle = rows / 2;
    let max = amplitude_row(value.max, rows);
    let min = amplitude_row(value.min, rows);
    if value.max > 0 && row >= max && row <= middle {
        ('+', Style::default().fg(Color::Cyan))
    } else if value.min < 0 && row >= middle && row <= min {
        ('-', Style::default().fg(Color::Magenta))
    } else if row == middle {
        ('-', Style::default().add_modifier(Modifier::DIM))
    } else {
        (' ', Style::default())
    }
}

fn amplitude_row(value: i8, rows: usize) -> usize {
    let middle = rows / 2;
    let magnitude = usize::from(i16::from(value).unsigned_abs()).min(8);
    let height = middle.max(1);
    let delta = magnitude.saturating_mul(height).div_ceil(8);
    if value >= 0 {
        middle.saturating_sub(delta)
    } else {
        middle.saturating_add(delta).min(rows.saturating_sub(1))
    }
}

fn marker_cell(
    active: SampleMarker,
    projection: &crate::SampleProjection,
    column: u16,
) -> Option<(char, Style)> {
    let start_here = projection.start_column == column;
    let end_here = projection.end_column == column;
    if start_here && end_here {
        return Some((
            'X',
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if start_here {
        return Some((
            offscreen_glyph(projection.start_offscreen, 'S'),
            marker_style(matches!(active, SampleMarker::Start), Color::Green),
        ));
    }
    if end_here {
        return Some((
            offscreen_glyph(projection.end_offscreen, 'E'),
            marker_style(matches!(active, SampleMarker::End), Color::Red),
        ));
    }
    None
}

fn offscreen_glyph(direction: Option<OffscreenDirection>, marker: char) -> char {
    match direction {
        Some(OffscreenDirection::Left) => '<',
        Some(OffscreenDirection::Right) => '>',
        None => marker,
    }
}

fn marker_style(active: bool, color: Color) -> Style {
    let mut style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    if active {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn render_line(frame: &mut Frame, area: Rect, row: u16, text: impl AsRef<str>, style: Style) {
    if row >= area.height {
        return;
    }
    frame.render_widget(
        Paragraph::new(truncate(text.as_ref(), usize::from(area.width))).style(style),
        Rect::new(area.x, area.y.saturating_add(row), area.width, 1),
    );
}

fn phase_range_to_frames(start: u64, end: u64, frames: usize) -> std::ops::Range<usize> {
    let scale = u128::from(sampler_core::SAMPLE_PHASE_SCALE);
    let start = u128::from(start).saturating_mul(frames as u128) / scale;
    let end = u128::from(end)
        .saturating_mul(frames as u128)
        .div_ceil(scale);
    usize::try_from(start.min(frames as u128)).unwrap_or(frames)
        ..usize::try_from(end.min(frames as u128)).unwrap_or(frames)
}

fn visible_range_text(range: Option<std::ops::Range<usize>>, rate: Option<u32>) -> String {
    let Some(range) = range else {
        return "--".to_owned();
    };
    let Some(rate) = rate.filter(|rate| *rate != 0) else {
        return format!("{}..{}", range.start, range.end);
    };
    format!(
        "{}..{} ({:.3}..{:.3}s)",
        range.start,
        range.end,
        range.start as f64 / f64::from(rate),
        range.end as f64 / f64::from(rate)
    )
}

fn frame_range_text(range: Option<std::ops::Range<usize>>) -> String {
    range.map_or_else(
        || "--".to_owned(),
        |range| format!("{}..{}", range.start, range.end),
    )
}

fn marker_frame(phase: u64, frames: Option<usize>, ceil: bool) -> String {
    let Some(frames) = frames else {
        return "--".to_owned();
    };
    let scaled = u128::from(phase).saturating_mul(frames as u128);
    let value = if ceil {
        scaled.div_ceil(u128::from(sampler_core::SAMPLE_PHASE_SCALE))
    } else {
        scaled / u128::from(sampler_core::SAMPLE_PHASE_SCALE)
    };
    value.min(frames as u128).to_string()
}

fn sample_status_line(app_status: &str, status: WorkspaceSampleEditorStatus) -> String {
    if !app_status.is_empty() {
        return format!(" STATUS {app_status}");
    }
    format!(" STATUS {}", editor_status_name(&status))
}

fn editor_status_name(status: &WorkspaceSampleEditorStatus) -> &'static str {
    match status {
        WorkspaceSampleEditorStatus::Empty => "EMPTY",
        WorkspaceSampleEditorStatus::Clean => "CLEAN",
        WorkspaceSampleEditorStatus::Dirty => "DIRTY",
        WorkspaceSampleEditorStatus::Pending => "PENDING",
        WorkspaceSampleEditorStatus::Error(_) => "ERROR",
        WorkspaceSampleEditorStatus::ApplyConfirmation => "APPLY CONFIRM",
        WorkspaceSampleEditorStatus::DiscardConfirmation => "DISCARD CONFIRM",
        WorkspaceSampleEditorStatus::UndoAvailable => "UNDO AVAILABLE",
    }
}

fn draft_style(status: &WorkspaceSampleEditorStatus) -> Style {
    match status {
        WorkspaceSampleEditorStatus::Dirty => Style::default().fg(Color::Yellow),
        WorkspaceSampleEditorStatus::Pending => Style::default().fg(Color::Cyan),
        WorkspaceSampleEditorStatus::Error(_) => Style::default().fg(Color::Red),
        _ => Style::default(),
    }
}

fn marker_name(marker: SampleMarker) -> &'static str {
    match marker {
        SampleMarker::Start => "START",
        SampleMarker::End => "END",
    }
}
fn mode_name(mode: PlaybackMode) -> &'static str {
    match mode {
        PlaybackMode::OneShot => "ONESHOT",
        PlaybackMode::Gate => "GATE",
        PlaybackMode::Loop => "LOOP",
    }
}
fn on_off(value: bool) -> &'static str {
    if value { "ON" } else { "OFF" }
}
fn load_state_name(state: &crate::PadLoadState) -> &'static str {
    match state {
        crate::PadLoadState::Empty => "EMPTY",
        crate::PadLoadState::WaitingForDevice => "WAITING",
        crate::PadLoadState::Loading => "LOADING",
        crate::PadLoadState::Ready => "READY",
        crate::PadLoadState::Error(_) => "ERROR",
    }
}
fn truncate(value: &str, width: usize) -> String {
    if width == 0 || value.is_empty() {
        return String::new();
    }
    if display_width(value) <= width {
        return value.to_owned();
    }
    let target = width.saturating_sub(1);
    let mut result = String::new();
    for character in value.chars() {
        let mut candidate = result.clone();
        candidate.push(character);
        if display_width(&candidate) > target {
            break;
        }
        result.push(character);
    }
    result.push('…');
    result
}

fn display_width(value: &str) -> usize {
    Line::from(Span::raw(value)).width()
}
