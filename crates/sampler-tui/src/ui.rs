use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph};

use crate::input::PAD_KEYS;
use crate::pattern::WorkspaceView;
use crate::ui_pattern::{live_identity, live_pattern, render_pattern, transport_bar_index};
use crate::ui_sample::render_sample as render_sample_workspace;
use crate::{
    App, Overlay, PREVIEW_COLUMNS, PadLoadState, PadView, ProjectAction, ProjectOpenPhase,
};

const MIN_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 24;
const WAVE_CHARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const HELP_LINES: [&str; 20] = [
    "GLOBAL: Ctrl+R retries audio first when the device is unavailable",
    "Tab / Shift+Tab: cycle Perform / Pattern / Sample",
    "Space: play / stop selected pattern",
    "Ctrl+R: overdub record selected pattern",
    ", / .: previous / next pattern (1..16)",
    "PATTERN",
    "Arrows / PgUp / PgDn: cursor / visible bar",
    "Enter / Delete: toggle / remove event",
    "+ / - / u / Ctrl+Delete: velocity / undo / clear",
    "SAMPLE: arrows trim · m marker · PgUp/PgDn zoom · n/u edits",
    "Up/Down pitch · o/g/l mode · Enter apply · Ctrl+Z undo",
    "plain z remains pad 13; Apply is in-memory only; Source file unchanged",
    "PROJECT: save · save-as <directory> · open-project <directory>",
    "Recovery: R restore · D discard · C cancel",
    "CAPTURE: resample · record-input · capture-stop · capture-cancel",
    "Recording: Enter stop · Esc review discard · pads/pattern stay live",
    "Capture lifecycle: Finalize · Discard · Cancel are explicit choices",
    "PADS: 1-4/Q-R/A-F/Z-V global · Shift+pad stop · [/] bank",
    "Arrow select · Enter trigger · l load · : command · Shift+Esc stop all",
    "Ctrl+Q / Ctrl+C quit · Esc or ? close help",
];

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_resize_message(frame, area);
        return;
    }

    render_base(frame, area, app);
    if let Some(overlay) = app.overlay() {
        render_overlay(frame, area, app, overlay);
    }
}

fn render_resize_message(frame: &mut Frame, area: Rect) {
    let current = format!("Terminal too small: {}x{}", area.width, area.height);
    let required = format!(
        "Resize terminal to at least {MIN_WIDTH}x{MIN_HEIGHT} (Required: {MIN_WIDTH}x{MIN_HEIGHT})"
    );
    let width = display_width(&current)
        .max(display_width(&required))
        .min(usize::from(area.width));
    let message_area = centered_rect(area, u16::try_from(width).unwrap_or(area.width), 2);
    frame.render_widget(
        Paragraph::new(vec![Line::from(current), Line::from(required)])
            .alignment(Alignment::Center),
        message_area,
    );
}

fn render_base(frame: &mut Frame, area: Rect, app: &App) {
    if app.workspace_view() == WorkspaceView::Pattern {
        render_pattern(frame, area, app);
        return;
    }
    if app.workspace_view() == WorkspaceView::Sample {
        render_sample_workspace(frame, area, app);
        return;
    }
    let bank = char::from(b'A'.saturating_add(u8::from(app.active_bank())));
    let (format, state) = match app.audio_format() {
        Some((rate, channels)) => (format!("{}kHz/{channels}ch", rate / 1_000), "RUN"),
        None => ("--kHz/--ch".to_owned(), "NO AUDIO"),
    };
    let outer = Block::new()
        .borders(Borders::ALL)
        .title(Line::from(format!(
            " BANK {bank} · sampler-tui · {} ",
            app.project_header()
        )))
        .title(Line::from(format!(" {format} · {state} ")).right_aligned());
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(15),
            Constraint::Min(4),
        ])
        .split(inner);
    render_sample(frame, sections[0], app);
    render_body(frame, sections[1], app);
    render_status(frame, sections[2], app);
}

fn render_sample(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let selected = app.selected_pad().min(15);
    let offset = usize::from(u8::from(app.active_bank())) * 16 + selected;
    let pad = &app.pads()[offset];
    let label = selected_sample_label(pad, app.pad_display_source(offset));
    let state = load_state_name(&pad.state);
    let summary = format!(
        " PAD {:02} · {} · {}",
        selected + 1,
        truncate(&label, usize::from(area.width).saturating_sub(26)),
        state
    );
    frame.render_widget(
        Paragraph::new(truncate(&summary, usize::from(area.width))),
        Rect::new(area.x, area.y, area.width, 1),
    );

    if area.height > 1 {
        let preview_width = usize::from(area.width)
            .saturating_sub(7)
            .min(PREVIEW_COLUMNS);
        let mut preview = String::from(" WAVE ");
        for column in pad.preview.iter().take(preview_width) {
            let magnitude = i16::from(column.min)
                .unsigned_abs()
                .max(i16::from(column.max).unsigned_abs());
            let level = usize::from(magnitude).min(8);
            preview.push(WAVE_CHARS[level.min(8)]);
        }
        frame.render_widget(
            Paragraph::new(preview),
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        );
    }
    if area.height > 2 {
        frame.render_widget(
            Block::new().borders(Borders::BOTTOM),
            Rect::new(
                area.x,
                area.y.saturating_add(2),
                area.width,
                area.height.saturating_sub(2),
            ),
        );
    }
}

fn render_body(frame: &mut Frame, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(49), Constraint::Min(1)])
        .split(area);
    render_pads(frame, columns[0], app);
    render_performance(frame, columns[1], app);
}

fn render_pads(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::new().borders(Borders::ALL).title(" PADS ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    frame.render_widget(
        Paragraph::new(" state: ●ready …load ×error ▶active !held"),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    if inner.height < 5 {
        return;
    }

    let bank_offset = usize::from(u8::from(app.active_bank())) * 16;
    let cell_width = usize::from(inner.width.saturating_sub(3)) / 4;
    for row in 0..4usize {
        let mut spans = Vec::with_capacity(7);
        for column in 0..4usize {
            let index = row * 4 + column;
            if column > 0 {
                spans.push(Span::raw(" "));
            }
            let pad = &app.pads()[bank_offset + index];
            let selected = app.selected_pad() == index;
            let held = app.is_pad_held(index);
            spans.push(Span::styled(
                pad_cell(
                    PAD_KEYS[index],
                    pad,
                    app.pad_display_source(bank_offset + index),
                    selected,
                    held,
                    cell_width,
                ),
                pad_style(&pad.state, selected, held),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(
                inner.x,
                inner.y.saturating_add(u16::try_from(row + 1).unwrap_or(0)),
                inner.width,
                1,
            ),
        );
    }
}

fn pad_style(state: &PadLoadState, selected: bool, held: bool) -> Style {
    let mut modifiers = Modifier::empty();
    if selected {
        modifiers |= Modifier::REVERSED;
    }
    if held {
        modifiers |= Modifier::UNDERLINED;
    }
    match state {
        PadLoadState::Loading | PadLoadState::WaitingForDevice => modifiers |= Modifier::DIM,
        PadLoadState::Error(_) => modifiers |= Modifier::BOLD,
        PadLoadState::Empty | PadLoadState::Ready => {}
    }
    Style::default().add_modifier(modifiers)
}

fn pad_cell(
    key: char,
    pad: &PadView,
    display_source: Option<&Path>,
    selected: bool,
    held: bool,
    width: usize,
) -> String {
    if width == 0 {
        return String::new();
    }
    let label_budget = width.saturating_sub(7);
    let label = truncate(&pad_label(pad, display_source), label_budget.max(1));
    let state = match pad.state {
        PadLoadState::Empty => '·',
        PadLoadState::WaitingForDevice => '◇',
        PadLoadState::Loading => '…',
        PadLoadState::Ready if pad.active => '▶',
        PadLoadState::Ready => '●',
        PadLoadState::Error(_) => '×',
    };
    let select = if selected { '>' } else { ' ' };
    let hold = if held { '!' } else { ' ' };
    let value = format!(
        "{select}[{} {label}{state}{hold}]",
        key.to_ascii_uppercase()
    );
    fit(&value, width)
}

fn render_performance(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::new().borders(Borders::ALL).title(" PERFORMANCE ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    let gauge_width = inner.width.saturating_sub(9);
    let (left, right) = app.meter_levels();
    for (row, channel, level) in [(0u16, "L", left), (1, "R", right)] {
        if row >= inner.height {
            continue;
        }
        frame.render_widget(
            Paragraph::new(format!("OUT {channel}")),
            Rect::new(inner.x, inner.y.saturating_add(row), 6.min(inner.width), 1),
        );
        if gauge_width > 0 {
            frame.render_widget(
                Gauge::default()
                    .gauge_style(Style::default().fg(Color::White))
                    .ratio(safe_meter_ratio(level))
                    .label(format!("{:>3}", (safe_meter_ratio(level) * 100.0).round())),
                Rect::new(
                    inner.x.saturating_add(7),
                    inner.y.saturating_add(row),
                    gauge_width,
                    1,
                ),
            );
        }
    }

    let (rate, channels) = app.audio_format().unwrap_or((0, 0));
    let telemetry = app.telemetry();
    let release = if app.release_events_available() {
        "yes"
    } else {
        "no (Shift stops)"
    };
    let mut rows = vec![
        format!("Voices {:02}", telemetry.active_voices),
        format!("Late {}", telemetry.late_commands),
        format!("Invalid {}", telemetry.invalid_commands),
        format!("Overflow {}", telemetry.command_overflows),
        format!("Frame {}", telemetry.rendered_frame),
        format!("Release keys: {release}"),
        format!("Device {rate}Hz/{channels}ch"),
        perform_pattern_summary(app),
    ];
    if let Some((source, target, elapsed, maximum, peak, hard_limit)) = capture_summary(app) {
        rows.push(format!("CAP {source} · {target}"));
        rows.push(format!("TIME {elapsed}/{maximum}"));
        rows.push(format!(
            "PEAK {peak}{}",
            if hard_limit { " · MAX" } else { "" }
        ));
    }
    for (index, row) in rows.iter().enumerate() {
        let y = inner
            .y
            .saturating_add(2)
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if y >= inner.bottom() {
            break;
        }
        frame.render_widget(
            Paragraph::new(truncate(row, usize::from(inner.width))),
            Rect::new(inner.x, y, inner.width, 1),
        );
    }
}

fn perform_pattern_summary(app: &App) -> String {
    let telemetry = app.telemetry();
    let Some(identity) = live_identity(app) else {
        return format!("P{:02} STOP", app.patterns().selected_slot().get() + 1);
    };
    let state = if identity.recording {
        "REC"
    } else if identity.playing {
        "PLAY"
    } else {
        "STOP"
    };
    if state == "STOP" {
        return format!("P{:02} STOP", identity.slot.get() + 1);
    }
    let Some((_, pattern)) = live_pattern(app) else {
        return format!("P{:02} {state}", identity.slot.get() + 1);
    };
    let transport = pattern.transport();
    let bar = transport_bar_index(transport, telemetry.pattern_playhead);
    format!(
        "P{:02} {state} {bar}/{}",
        identity.slot.get() + 1,
        transport.bars()
    )
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let block = Block::new().borders(Borders::TOP).title(" STATUS ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows = [
        if app.status().is_empty() {
            "Ready"
        } else {
            app.status()
        },
        "Enter trigger · Shift+pad stop · Shift+Esc stop all",
        "l load · [/] bank · ? help · : cmd · Ctrl+Q quit",
    ];
    for (index, row) in rows.iter().enumerate() {
        let y = inner
            .y
            .saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if y >= inner.bottom() {
            break;
        }
        frame.render_widget(
            Paragraph::new(format!(
                " {}",
                truncate(row, usize::from(inner.width).saturating_sub(1))
            )),
            Rect::new(inner.x, y, inner.width, 1),
        );
    }
}

fn render_overlay(frame: &mut Frame, area: Rect, app: &App, overlay: &Overlay) {
    match overlay {
        Overlay::Help => render_list_overlay(frame, area, " HELP ", 72, 23, HELP_LINES),
        Overlay::Palette => render_palette(frame, area, app),
        Overlay::FilePicker => render_picker(frame, area, app),
        Overlay::DeviceError(error) => render_list_overlay(
            frame,
            area,
            " AUDIO DEVICE ERROR ",
            62,
            7,
            [
                truncate(error, 56),
                "Pads and loaded samples remain available.".to_owned(),
                "r retry · Esc browse without audio".to_owned(),
            ],
        ),
        Overlay::ProjectOpenProgress => {
            let stage = app.project_open_stage();
            let title = if stage
                .is_some_and(|stage| stage.phase == ProjectOpenPhase::AwaitingRecoveryChoice)
            {
                " RECOVERY AVAILABLE "
            } else {
                " OPEN PROJECT "
            };
            let mut lines = vec![app.status().to_owned()];
            if let Some(stage) = stage {
                lines.push(format!("Directory {}", stage.directory.to_string_lossy()));
                if let Some(revision) = stage.revision {
                    lines.push(format!("Revision {revision}"));
                }
                match stage.phase {
                    ProjectOpenPhase::Probing => {
                        lines.push("Checking project and recovery documents…".to_owned());
                        lines.push("Esc cancels before audio admission begins.".to_owned());
                    }
                    ProjectOpenPhase::AwaitingRecoveryChoice => {
                        lines.push("A newer recovery can replace the explicit save.".to_owned());
                        lines.push("R Restore · D Discard · C Cancel".to_owned());
                    }
                    ProjectOpenPhase::Staging => {
                        lines.push(format!(
                            "Staging samples {}/{}",
                            stage.staged_pads, stage.total_pads
                        ));
                        lines.push("Esc cancels before audio admission begins.".to_owned());
                    }
                    ProjectOpenPhase::Admitting => {
                        lines.push(format!(
                            "Installing audio {}/{}",
                            stage.admitted_actions, stage.total_actions
                        ));
                        lines.push("Audio admission is in progress; please wait.".to_owned());
                    }
                }
            }
            lines.push("Current project remains unchanged until open completes.".to_owned());
            render_list_overlay(frame, area, title, 72, 9, lines);
        }
        Overlay::ClearPattern { slot, event_count } => render_list_overlay(
            frame,
            area,
            " CLEAR PATTERN ",
            52,
            6,
            [
                format!("Clear pattern {} ({event_count} events)?", slot.get() + 1),
                "This removes events; one undo is available.".to_owned(),
                "Enter confirms · Esc cancels".to_owned(),
            ],
        ),
        Overlay::ApplySample {
            pad,
            before_frames,
            after_frames,
        } => render_list_overlay(
            frame,
            area,
            " APPLY SAMPLE EDIT ",
            72,
            8,
            [
                format!("Apply in-memory edit to pad {}?", pad.index() + 1),
                format!("Before {before_frames} frames"),
                format!("After {after_frames} frames"),
                "in-memory only; source file unchanged".to_owned(),
                "Project becomes MODIFIED until saved.".to_owned(),
                "Enter confirms · Esc cancels".to_owned(),
            ],
        ),
        Overlay::DiscardSample { pad } => render_list_overlay(
            frame,
            area,
            " DISCARD SAMPLE DRAFT ",
            72,
            7,
            [
                format!("Discard un-applied edits for pad {}?", pad.index() + 1),
                "Committed in-memory audio remains unchanged.".to_owned(),
                "Source file unchanged.".to_owned(),
                "Enter confirms · Esc keeps editing".to_owned(),
            ],
        ),
        Overlay::ResolveSampleDraft { pad, action } => render_list_overlay(
            frame,
            area,
            " RESOLVE SAMPLE DRAFT ",
            72,
            8,
            [
                format!(
                    "Pad {} has un-applied edits before {}.",
                    pad.index() + 1,
                    project_action_name(*action)
                ),
                "Enter Apply · Backspace Discard · Esc Cancel".to_owned(),
                "Apply changes only in-memory audio and marks the project modified.".to_owned(),
                "Discard keeps committed audio; source file remains unchanged.".to_owned(),
            ],
        ),
        Overlay::UnsavedProject { action } => render_list_overlay(
            frame,
            area,
            " UNSAVED PROJECT ",
            72,
            8,
            [
                format!("Save changes before {}?", project_action_name(*action)),
                "Y Save · N Discard · Esc Cancel".to_owned(),
                if app.project_header().starts_with("Untitled") {
                    "Untitled project: use save-as <directory> before continuing.".to_owned()
                } else {
                    "Save waits for the exact matching worker result.".to_owned()
                },
                "Discard removes an exact newer recovery before continuing.".to_owned(),
            ],
        ),
        Overlay::ProjectLifecycleProgress { action } => render_list_overlay(
            frame,
            area,
            " PROJECT OPERATION ",
            72,
            7,
            [
                app.status().to_owned(),
                format!("Preparing to {} safely.", project_action_name(*action)),
                "Waiting for the exact save result or recovery deletion.".to_owned(),
            ],
        ),
        Overlay::ProjectSaveProgress => render_list_overlay(
            frame,
            area,
            " SAVE PROJECT ",
            72,
            6,
            [
                app.status().to_owned(),
                "Waiting for the exact matching save result.".to_owned(),
                "The project remains MODIFIED if saving fails.".to_owned(),
            ],
        ),
        Overlay::ProjectError { title, message } => render_list_overlay(
            frame,
            area,
            &format!(" {title} "),
            72,
            7,
            [
                message.to_owned(),
                "Current project remains open and unchanged.".to_owned(),
                "Enter or Esc dismisses this error.".to_owned(),
            ],
        ),
        Overlay::CaptureConfirm => {
            let (source, target, prior) = capture_context(app);
            render_list_overlay(
                frame,
                area,
                &format!(" REPLACE PAD {target} "),
                72,
                8,
                [
                    format!("{source} will replace {prior} on pad {target}."),
                    "The old pad stays audible until exact audio admission.".to_owned(),
                    "Enter starts · Esc cancels without changing the old pad.".to_owned(),
                    "Stop all and held-pad releases remain available.".to_owned(),
                ],
            );
        }
        Overlay::CaptureDiscard => {
            let (source, target, _) = capture_context(app);
            render_list_overlay(
                frame,
                area,
                " DISCARD CAPTURE ",
                72,
                8,
                [
                    format!("Discard the {source} take for pad {target}?"),
                    "The old pad remains unchanged.".to_owned(),
                    "Enter discards · Esc keeps the capture.".to_owned(),
                    "Stop all and held-pad releases remain available.".to_owned(),
                ],
            );
        }
        Overlay::ResolveCapture { action } => {
            let (source, target, _) = capture_context(app);
            render_list_overlay(
                frame,
                area,
                " RESOLVE ACTIVE CAPTURE ",
                72,
                9,
                [
                    format!(
                        "{source} for pad {target} is unresolved before {}.",
                        project_action_name(*action)
                    ),
                    "Enter Finalize · Backspace Discard · Esc Cancel".to_owned(),
                    "Finalize waits for exact worker and audio success.".to_owned(),
                    "Discard waits until callback or worker ownership returns.".to_owned(),
                    "Cancel abandons the project action and preserves the capture.".to_owned(),
                ],
            );
        }
        Overlay::CaptureProgress { action, discarding } => {
            let (source, target, _) = capture_context(app);
            let title = if *discarding {
                " DISCARDING CAPTURE "
            } else {
                " FINALIZING CAPTURE "
            };
            let mut lines = vec![
                format!("{source} · pad {target}"),
                capture_progress_line(app),
                if *discarding {
                    "Waiting for exact callback or worker ownership.".to_owned()
                } else {
                    "Waiting for the exact callback, worker, and audio result.".to_owned()
                },
                "The old pad and project remain unchanged while waiting.".to_owned(),
                "Stop all and held-pad releases remain available.".to_owned(),
            ];
            if let Some(action) = action {
                lines.push(format!(
                    "Then continue {} through the existing project lifecycle.",
                    project_action_name(*action)
                ));
            }
            render_list_overlay(frame, area, title, 72, 9, lines);
        }
        Overlay::CaptureFailed { action } => {
            let (source, target, _) = capture_context(app);
            let mut lines = vec![
                format!("{source} · pad {target}"),
                app.capture_session()
                    .failure()
                    .unwrap_or("capture failed")
                    .to_owned(),
                "The old pad and project remain unchanged.".to_owned(),
            ];
            if app.capture_session().failure_is_retryable() {
                lines.push("R or Enter retries finalization from the retained take.".to_owned());
            } else {
                lines.push("This failure requires a fresh take; retry is unavailable.".to_owned());
            }
            lines.push(if action.is_some() {
                "D/Backspace Discard · C/Esc Cancel project action".to_owned()
            } else {
                "D/Backspace Discard or Cancel capture · Ctrl+R retries audio".to_owned()
            });
            lines.push("Stop all and held-pad releases remain available.".to_owned());
            render_list_overlay(frame, area, " CAPTURE FAILED ", 72, 10, lines);
        }
    }
}

fn capture_summary(app: &App) -> Option<(&'static str, String, String, String, String, bool)> {
    let session = app.capture_session();
    let source = capture_source_name(session.source()?);
    let target = pad_name(session.target()?);
    let maximum = format_capture_time(session.max_frames()?, session.source_rate()?);
    let status = app.capture_status_view().filter(|status| {
        session.token() == Some(status.token)
            && session.source() == Some(status.source)
            && session.target() == Some(status.target)
            && session.max_frames() == Some(status.max_frames)
    });
    let (elapsed, peak, hard_limit) = status.map_or_else(
        || ("--:--.---".to_owned(), "---.---".to_owned(), false),
        |status| {
            (
                format_capture_time(status.frames, session.source_rate().unwrap_or(1)),
                format!("{:.3}", status.peak),
                status.hard_limit,
            )
        },
    );
    Some((source, target, elapsed, maximum, peak, hard_limit))
}

fn capture_context(app: &App) -> (&'static str, String, String) {
    let session = app.capture_session();
    let source = session.source().map_or("CAPTURE", capture_source_name);
    let target_pad = session.target().unwrap_or_else(sampler_core::PadId::first);
    let target = pad_name(target_pad);
    let offset = usize::from(u8::from(target_pad.bank())) * 16 + usize::from(target_pad.index());
    let prior = app
        .pads()
        .get(offset)
        .map(|pad| selected_sample_label(pad, app.pad_display_source(offset)))
        .unwrap_or_else(|| "EMPTY PAD".to_owned());
    (source, target, prior)
}

fn capture_progress_line(app: &App) -> String {
    capture_summary(app).map_or_else(
        || "Progress unavailable".to_owned(),
        |(_, _, elapsed, maximum, peak, hard_limit)| {
            format!(
                "Elapsed {elapsed}/{maximum} · Peak {peak}{}",
                if hard_limit { " · MAX" } else { "" }
            )
        },
    )
}

const fn capture_source_name(source: sampler_audio::CaptureSource) -> &'static str {
    match source {
        sampler_audio::CaptureSource::Resample => "RESAMPLE",
        sampler_audio::CaptureSource::Input => "INPUT",
    }
}

fn pad_name(pad: sampler_core::PadId) -> String {
    let bank = char::from(b'A'.saturating_add(u8::from(pad.bank())));
    format!("{bank}{:02}", pad.index() + 1)
}

fn format_capture_time(frames: usize, sample_rate: u32) -> String {
    if sample_rate == 0 {
        return "--:--.---".to_owned();
    }
    let milliseconds = (frames as u128)
        .saturating_mul(1_000)
        .checked_div(u128::from(sample_rate))
        .unwrap_or(0);
    let minutes = milliseconds / 60_000;
    let seconds = milliseconds / 1_000 % 60;
    let millis = milliseconds % 1_000;
    format!("{minutes:02}:{seconds:02}.{millis:03}")
}

const fn project_action_name(action: ProjectAction) -> &'static str {
    match action {
        ProjectAction::Open => "opening another project",
        ProjectAction::Quit => "quitting",
    }
}

fn render_palette(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(area, 62, if app.palette_error().is_some() { 7 } else { 5 });
    frame.render_widget(Clear, popup);
    let block = Block::new()
        .borders(Borders::ALL)
        .title(" COMMAND ")
        .style(Style::default().add_modifier(Modifier::BOLD));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.is_empty() {
        return;
    }
    let input = palette_window(
        app.palette_text(),
        app.palette_cursor(),
        usize::from(inner.width),
    );
    frame.render_widget(
        Paragraph::new(input),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    if let Some(error) = app.palette_error()
        && inner.height > 1
    {
        frame.render_widget(
            Paragraph::new(truncate(error, usize::from(inner.width)))
                .style(Style::default().add_modifier(Modifier::BOLD)),
            Rect::new(inner.x, inner.y.saturating_add(1), inner.width, 1),
        );
    }
}

fn palette_window(text: &str, cursor: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cursor = cursor.min(text.len());
    let (before, after) = text.split_at(cursor);
    let before = format!(":{before}");
    let full = format!("{before}▏{after}");
    if display_width(&full) <= width {
        return full;
    }

    let context_width = width.saturating_sub(1);
    let mut right = prefix_with_width(after, context_width / 2);
    let left = suffix_with_width(&before, context_width.saturating_sub(display_width(&right)));
    let remaining = context_width
        .saturating_sub(display_width(&left))
        .saturating_sub(display_width(&right));
    if remaining > 0 {
        right = prefix_with_width(after, display_width(&right).saturating_add(remaining));
    }
    format!("{left}▏{right}")
}

fn prefix_with_width(value: &str, width: usize) -> String {
    let mut result = String::new();
    for character in value.chars() {
        let mut candidate = result.clone();
        candidate.push(character);
        if display_width(&candidate) > width {
            break;
        }
        result.push(character);
    }
    result
}

fn suffix_with_width(value: &str, width: usize) -> String {
    let mut result = String::new();
    for character in value.chars().rev() {
        let mut candidate = String::with_capacity(character.len_utf8() + result.len());
        candidate.push(character);
        candidate.push_str(&result);
        if display_width(&candidate) > width {
            break;
        }
        result = candidate;
    }
    result
}

fn render_picker(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(area, 72, 19);
    frame.render_widget(Clear, popup);
    let block = Block::new().borders(Borders::ALL).title(" LOAD SAMPLE ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.is_empty() {
        return;
    }
    let picker = app.file_picker();
    let directory = picker.directory().to_string_lossy();
    let mut header = if let Some(pending) = picker.pending_directory() {
        format!(
            "Loading {}… · Viewing {directory}",
            pending.to_string_lossy(),
        )
    } else if let (Some(failed), Some(error)) = (picker.failed_directory(), picker.error()) {
        format!(
            "× {}: {error} · Viewing {directory}",
            failed.to_string_lossy(),
        )
    } else {
        format!("Viewing {directory}")
    };
    if picker.truncated() {
        header.push_str(" · limited result set");
    }
    frame.render_widget(
        Paragraph::new(truncate(&header, usize::from(inner.width))),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let list_height = inner.height.saturating_sub(2);
    let visible_rows = usize::from(list_height);
    let offset = picker
        .cursor()
        .saturating_add(1)
        .saturating_sub(visible_rows);
    let items: Vec<ListItem> = if picker.entries().is_empty() {
        vec![ListItem::new(if picker.is_scanning() {
            "… loading (no committed entries)".to_owned()
        } else {
            "(empty directory)".to_owned()
        })]
    } else {
        picker
            .entries()
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible_rows)
            .map(|(index, entry)| {
                let select = if index == picker.cursor() { '>' } else { ' ' };
                let kind = if entry.is_directory() { '/' } else { ' ' };
                ListItem::new(truncate(
                    &format!("{select} {}{kind}", entry.display_name()),
                    usize::from(inner.width),
                ))
            })
            .collect()
    };
    frame.render_widget(
        List::new(items),
        Rect::new(inner.x, inner.y.saturating_add(1), inner.width, list_height),
    );
    if inner.height > 1 {
        frame.render_widget(
            Paragraph::new("Enter open/load · Backspace parent · . hidden · Esc close"),
            Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
        );
    }
}

fn render_list_overlay<I, S>(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    width: u16,
    height: u16,
    lines: I,
) where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let popup = centered_rect(area, width, height);
    frame.render_widget(Clear, popup);
    let block = Block::new()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().add_modifier(Modifier::BOLD));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let items = lines
        .into_iter()
        .map(|line| ListItem::new(truncate(&line.into(), usize::from(inner.width))))
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), inner);
}

fn centered_rect(area: Rect, requested_width: u16, requested_height: u16) -> Rect {
    let width = requested_width.min(area.width);
    let height = requested_height.min(area.height);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

fn pad_label(pad: &PadView, display_source: Option<&Path>) -> String {
    if !pad.label.trim().is_empty() {
        return Path::new(pad.label.trim())
            .file_stem()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| pad.label.trim().to_owned());
    }
    display_source
        .and_then(Path::file_stem)
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "----".to_owned())
}

fn selected_sample_label(pad: &PadView, display_source: Option<&Path>) -> String {
    let label = if !pad.label.trim().is_empty() {
        Path::new(pad.label.trim())
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| pad.label.trim().to_owned())
    } else {
        display_source
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "----".to_owned())
    };
    label.to_uppercase()
}

fn load_state_name(state: &PadLoadState) -> &'static str {
    match state {
        PadLoadState::Empty => "EMPTY",
        PadLoadState::WaitingForDevice => "WAITING FOR DEVICE",
        PadLoadState::Loading => "LOADING…",
        PadLoadState::Ready => "READY",
        PadLoadState::Error(_) => "ERROR",
    }
}

fn safe_meter_ratio(value: f32) -> f64 {
    if value.is_finite() {
        f64::from(value.clamp(0.0, 1.0))
    } else {
        0.0
    }
}

fn fit(value: &str, width: usize) -> String {
    let mut fitted = truncate(value, width);
    let used = display_width(&fitted);
    fitted.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    fitted
}

fn truncate(value: &str, width: usize) -> String {
    if width == 0 {
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::Duration;

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Modifier, Style};
    use sampler_audio::{
        CaptureBuffer, CaptureSource, CaptureState, CaptureStatus, Frame, SampleBuffer, SampleSlot,
        Telemetry,
    };
    use sampler_core::{PadId, PadSettings, ProjectDocument, ProjectId};

    use crate::audio::{AudioPort, CaptureSupport};
    use crate::input::InputAction;
    use crate::loader::{LoadPurpose, LoadSampleError, LoadedSample, WorkerResult};
    use crate::{
        App, DirectoryEntry, DirectoryEntryKind, EDIT_PREVIEW_COLUMNS, Overlay, PatternWorkspace,
        PreviewColumn,
    };

    use super::{render, transport_bar_index};

    struct FakeAudio {
        stop_error: Option<String>,
        telemetry: Option<Telemetry>,
        format_reads: Option<Rc<Cell<usize>>>,
        capture_status: Option<CaptureStatus>,
        capture_error: Option<crate::CaptureError>,
    }

    impl FakeAudio {
        fn ready() -> Self {
            Self {
                stop_error: None,
                telemetry: None,
                format_reads: None,
                capture_status: None,
                capture_error: None,
            }
        }

        fn with_stop_error(mut self, error: &str) -> Self {
            self.stop_error = Some(error.to_owned());
            self
        }

        fn with_telemetry(mut self, telemetry: Telemetry) -> Self {
            self.telemetry = Some(telemetry);
            self
        }

        fn with_format_reads(mut self, reads: Rc<Cell<usize>>) -> Self {
            self.format_reads = Some(reads);
            self
        }

        fn with_capture_status(mut self, status: CaptureStatus) -> Self {
            self.capture_status = Some(status);
            self
        }

        fn with_capture_error(mut self, error: crate::CaptureError) -> Self {
            self.capture_error = Some(error);
            self
        }

        fn record_format_read(&self) {
            if let Some(reads) = &self.format_reads {
                reads.set(reads.get().saturating_add(1));
            }
        }
    }

    impl AudioPort for FakeAudio {
        fn capture_support(&self) -> CaptureSupport {
            if self.capture_status.is_some() {
                CaptureSupport::Available
            } else {
                CaptureSupport::Unsupported
            }
        }

        fn capture_source_rate(
            &mut self,
            source: CaptureSource,
        ) -> Result<u32, crate::CaptureError> {
            let status = self
                .capture_status
                .filter(|status| status.source == source)
                .ok_or(crate::CaptureError::Unsupported)?;
            Ok(match source {
                CaptureSource::Resample => 48_000,
                CaptureSource::Input => {
                    if status.max_frames == 88_200 {
                        44_100
                    } else {
                        48_000
                    }
                }
            })
        }

        fn begin_capture(
            &mut self,
            _buffer: CaptureBuffer,
        ) -> Result<(), crate::audio::CaptureCommandFailure> {
            Ok(())
        }

        fn start_capture(
            &mut self,
            _source: CaptureSource,
            _token: u64,
        ) -> Result<(), crate::audio::CaptureCommandFailure> {
            Ok(())
        }

        fn stop_capture(
            &mut self,
            _source: CaptureSource,
            _token: u64,
        ) -> Result<(), crate::audio::CaptureCommandFailure> {
            Ok(())
        }

        fn cancel_capture(
            &mut self,
            _source: CaptureSource,
            _token: u64,
        ) -> Result<(), crate::audio::CaptureCommandFailure> {
            Ok(())
        }

        fn capture_status(&mut self, source: CaptureSource) -> Option<CaptureStatus> {
            self.capture_status.filter(|status| status.source == source)
        }

        fn capture_runtime_error(&mut self, source: CaptureSource) -> Option<crate::CaptureError> {
            let error = self.capture_error.take()?;
            if matches!(
                (source, &error),
                (
                    CaptureSource::Resample,
                    crate::CaptureError::OutputRuntime(_)
                ) | (CaptureSource::Input, crate::CaptureError::InputRuntime(_))
            ) {
                Some(error)
            } else {
                self.capture_error = Some(error);
                None
            }
        }

        fn sample_rate(&self) -> u32 {
            self.record_format_read();
            48_000
        }

        fn channels(&self) -> u16 {
            self.record_format_read();
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
            SampleSlot::new(0).map_err(|error| error.to_string())
        }

        fn trigger(&mut self, _pad: PadId, _at: Frame, _velocity: f32) -> Result<(), String> {
            Ok(())
        }

        fn release(&mut self, _pad: PadId, _at: Frame) -> Result<(), String> {
            Ok(())
        }

        fn stop_pad(&mut self, _pad: PadId) -> Result<(), String> {
            Ok(())
        }

        fn stop_all(&mut self) -> Result<(), String> {
            self.stop_error.clone().map_or(Ok(()), Err)
        }

        fn update_pad(&mut self, _pad: PadId, _settings: PadSettings) -> Result<(), String> {
            Ok(())
        }

        fn reclaim_retired(&mut self) -> usize {
            0
        }

        fn latest_telemetry(&mut self) -> Option<Telemetry> {
            self.telemetry.take()
        }

        fn poll_runtime_error(&mut self) -> Option<String> {
            None
        }
    }

    fn pad(index: u8) -> PadId {
        PadId::new(sampler_core::BankId::new(0).unwrap(), index).unwrap()
    }

    fn ready_app() -> App {
        App::with_audio(Box::new(FakeAudio::ready()))
    }

    fn populated_app() -> App {
        populated_app_with_audio(FakeAudio::ready().with_stop_error("Overflow 3"))
    }

    fn populated_app_with_audio(audio: FakeAudio) -> App {
        let mut app = loaded_states_app(audio);
        app.set_keyboard_capabilities(crate::KeyboardCapabilities {
            release_events: true,
        });
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::StopAll);
        app
    }

    fn loaded_states_app(audio: FakeAudio) -> App {
        let mut app = App::with_audio(Box::new(audio));
        let kick = pad(0);
        let path = std::path::PathBuf::from("/samples/KICK.wav");
        let request = app.begin_load(kick, path.clone()).unwrap();
        let generation = match request {
            crate::WorkerRequest::LoadSample { generation, .. } => generation,
            crate::WorkerRequest::EditSample { .. }
            | crate::WorkerRequest::ScanDirectory { .. }
            | crate::WorkerRequest::SaveProject(_)
            | crate::WorkerRequest::ProbeProject { .. }
            | crate::WorkerRequest::DiscardRecovery { .. }
            | crate::WorkerRequest::StageProjectSample(_)
            | crate::WorkerRequest::FinalizeCapture(_)
            | crate::WorkerRequest::ReleaseManagedCapture { .. }
            | crate::WorkerRequest::Shutdown => {
                unreachable!()
            }
        };
        let mut preview = [PreviewColumn::default(); EDIT_PREVIEW_COLUMNS];
        for (index, column) in preview.iter_mut().enumerate() {
            let height = i8::try_from(index % 9).unwrap();
            *column = PreviewColumn {
                min: -height,
                max: height,
            };
        }
        assert!(
            app.apply_worker_result(WorkerResult::Loaded {
                pad: kick,
                generation,
                purpose: LoadPurpose::User,
                path,
                result: Ok(LoadedSample {
                    fingerprint: crate::SourceFingerprint::from_encoded_bytes(
                        std::path::Path::new("fixture.wav"),
                        &[],
                    )
                    .unwrap(),
                    base: Arc::new(SampleBuffer::new(48_000, vec![0.0; 256]).unwrap()),
                    base_preview: Arc::new(preview),
                    rendered: Arc::new(SampleBuffer::new(48_000, vec![0.0; 256]).unwrap()),
                    rendered_preview: Arc::new(preview),
                    recipe: sampler_core::SampleEditRecipe::identity(),
                    source_rate: 48_000,
                    source_frames: 128,
                    duration: Duration::from_secs_f64(128.0 / 48_000.0),
                }),
            })
        );
        assert_eq!(app.pad(kick).preview[0], PreviewColumn { min: -8, max: 8 });
        app.begin_load(pad(4), "/samples/HAT.wav");
        let error_path = std::path::PathBuf::from("/samples/CLAP.wav");
        let request = app.begin_load(pad(5), error_path.clone()).unwrap();
        let generation = match request {
            crate::WorkerRequest::LoadSample { generation, .. } => generation,
            crate::WorkerRequest::EditSample { .. }
            | crate::WorkerRequest::ScanDirectory { .. }
            | crate::WorkerRequest::SaveProject(_)
            | crate::WorkerRequest::ProbeProject { .. }
            | crate::WorkerRequest::DiscardRecovery { .. }
            | crate::WorkerRequest::StageProjectSample(_)
            | crate::WorkerRequest::FinalizeCapture(_)
            | crate::WorkerRequest::ReleaseManagedCapture { .. }
            | crate::WorkerRequest::Shutdown => {
                unreachable!()
            }
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(5),
            generation,
            purpose: LoadPurpose::User,
            path: error_path,
            result: Err(LoadSampleError::Decode("decode failed".to_owned())),
        });
        app
    }

    fn render_lines(width: u16, height: u16, app: &App) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let mut line = String::new();
                for x in 0..width {
                    line.push_str(buffer[(x, y)].symbol());
                }
                line.trim_end().to_owned()
            })
            .collect()
    }

    fn render_style(width: u16, height: u16, app: &App, x: u16, y: u16) -> Style {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal.backend().buffer()[(x, y)].style()
    }

    fn render_symbol(width: u16, height: u16, app: &App, x: u16, y: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal.backend().buffer()[(x, y)].symbol().to_owned()
    }

    fn complete_picker_scan(app: &mut App, count: usize) {
        let requests = app.take_worker_requests();
        let [
            crate::WorkerRequest::ScanDirectory {
                request_id, path, ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected one picker scan")
        };
        let entries = (0..count)
            .map(|index| DirectoryEntry {
                path: std::path::PathBuf::from("/samples").join(format!("sample-{index:02}.wav")),
                kind: DirectoryEntryKind::File,
            })
            .collect();
        assert!(app.apply_worker_result(WorkerResult::Scanned {
            request_id: *request_id,
            path: path.clone(),
            result: Ok(entries),
        }));
    }

    fn picker_at(cursor: usize) -> App {
        let mut app = ready_app();
        app.open_picker_at("/samples");
        complete_picker_scan(&mut app, 20);
        for _ in 0..cursor {
            app.apply_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        app
    }

    #[test]
    fn too_small_terminal_renders_only_the_resize_message() {
        let lines = render_lines(79, 23, &ready_app());
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Terminal too small: 79x23"))
        );
        assert!(lines.iter().any(|line| line.contains("Required: 80x24")));
        assert!(!lines.iter().any(|line| line.contains("PADS")));
    }

    #[test]
    fn exact_eighty_by_twenty_four_pattern_view_contains_full_grid_and_transport() {
        let mut app = ready_app();
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        let screen = render_lines(80, 24, &app).join("\n");

        assert!(screen.contains("PATTERN 01"));
        assert!(screen.contains("120.0 BPM"));
        assert!(screen.contains("REC"));
        assert!(screen.contains("01 02 03 04 05 06 07 08 09 10 11 12 13 14 15 16"));
        for label in [
            "01 1", "02 2", "03 3", "04 4", "05 Q", "06 W", "07 E", "08 R", "09 A", "10 S", "11 D",
            "12 F", "13 Z", "14 X", "15 C", "16 V",
        ] {
            assert!(screen.contains(label), "missing {label}");
        }
    }

    #[test]
    fn pattern_view_below_minimum_only_renders_resize_message() {
        let mut app = ready_app();
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let screen = render_lines(79, 23, &app).join("\n");

        assert!(screen.contains("Resize terminal to at least 80x24"));
        assert!(!screen.contains("PATTERN 01"));
        assert!(!screen.contains("01 02 03 04"));
    }

    #[test]
    fn pattern_playhead_and_cursor_are_distinct_fixed_width_cells() {
        let telemetry = Telemetry {
            active_pads: [0; 3],
            rendered_frame: 0,
            last_triggered_frame: None,
            peak_left: 0.0,
            peak_right: 0.0,
            active_voices: 0,
            late_commands: 0,
            invalid_commands: 0,
            command_overflows: 0,
            pattern_slot: Some(sampler_core::PatternSlotId::new(0).unwrap()),
            pattern_generation: Some(0),
            pattern_playing: true,
            pattern_recording: false,
            pattern_origin: Some(0),
            pattern_playhead: 0,
            pattern_loop_count: 0,
            pattern_overflows: 0,
            live_ack_overflows: 0,
        };
        let mut app = App::with_audio(Box::new(FakeAudio::ready().with_telemetry(telemetry)));
        app.tick();
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));

        let playhead_x = 6;
        let cursor_x = 7;
        assert_eq!(playhead_x + 15, 21);
        assert_ne!(
            render_style(109, 30, &app, playhead_x, 4),
            render_style(109, 30, &app, cursor_x, 4),
        );
        assert_eq!(render_symbol(109, 30, &app, playhead_x, 4), ">");
        assert_eq!(render_symbol(109, 30, &app, cursor_x, 4), ".");
    }

    #[test]
    fn perform_view_summarizes_the_playing_pattern_bar() {
        let telemetry = Telemetry {
            active_pads: [0; 3],
            rendered_frame: 96_000,
            last_triggered_frame: None,
            peak_left: 0.0,
            peak_right: 0.0,
            active_voices: 0,
            late_commands: 0,
            invalid_commands: 0,
            command_overflows: 0,
            pattern_slot: Some(sampler_core::PatternSlotId::new(0).unwrap()),
            pattern_generation: Some(1),
            pattern_playing: true,
            pattern_recording: false,
            pattern_origin: Some(0),
            pattern_playhead: 96_000,
            pattern_loop_count: 0,
            pattern_overflows: 0,
            live_ack_overflows: 0,
        };
        let mut app = App::with_audio(Box::new(FakeAudio::ready().with_telemetry(telemetry)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("bars 4".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.tick();

        assert!(
            render_lines(80, 24, &app)
                .join("\n")
                .contains("P01 PLAY 2/4")
        );
    }

    #[test]
    fn pending_slot_switch_keeps_the_perform_summary_on_the_live_generation() {
        let telemetry = Telemetry {
            active_pads: [0; 3],
            rendered_frame: 0,
            last_triggered_frame: None,
            peak_left: 0.0,
            peak_right: 0.0,
            active_voices: 0,
            late_commands: 0,
            invalid_commands: 0,
            command_overflows: 0,
            pattern_slot: Some(sampler_core::PatternSlotId::new(0).unwrap()),
            pattern_generation: Some(0),
            pattern_playing: true,
            pattern_recording: false,
            pattern_origin: Some(0),
            pattern_playhead: 0,
            pattern_loop_count: 0,
            pattern_overflows: 0,
            live_ack_overflows: 0,
        };
        let mut app = App::with_audio(Box::new(FakeAudio::ready().with_telemetry(telemetry)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("pattern 2".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.close_overlay();
        app.tick();

        let screen = render_lines(80, 24, &app).join("\n");
        assert!(screen.contains("P01 PLAY 1/1"));
        assert!(!screen.contains("P02 PLAY"));
    }

    #[test]
    fn transport_grid_bar_boundary_handles_non_divisible_three_bar_loops() {
        let transport = sampler_core::Transport::new(
            48_000,
            sampler_core::Tempo::new(20.1).unwrap(),
            sampler_core::Meter::new(4, 4).unwrap(),
            3,
            sampler_core::Resolution::Sixteenth,
        )
        .unwrap();
        let boundary = transport.step_frame(16);

        assert_eq!(transport_bar_index(transport, boundary - 1), 1);
        assert_eq!(transport_bar_index(transport, boundary), 2);
    }

    #[test]
    fn eighth_note_second_bar_cursor_is_visible_in_the_first_of_sixteen_columns() {
        let mut app = ready_app();
        for command in ["bars 2", "resolution 1/8"] {
            app.open_palette();
            app.apply_terminal_event(Event::Paste(command.to_owned()));
            app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));

        let screen = render_lines(80, 24, &app).join("\n");
        assert!(screen.contains("BAR 2/2 · 1/8"));
        assert!(
            render_style(80, 24, &app, 6, 4)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn pattern_view_labels_a_pending_slot_switch_without_borrowing_the_live_playhead() {
        let telemetry = Telemetry {
            active_pads: [0; 3],
            rendered_frame: 0,
            last_triggered_frame: None,
            peak_left: 0.0,
            peak_right: 0.0,
            active_voices: 0,
            late_commands: 0,
            invalid_commands: 0,
            command_overflows: 0,
            pattern_slot: Some(sampler_core::PatternSlotId::new(0).unwrap()),
            pattern_generation: Some(0),
            pattern_playing: true,
            pattern_recording: false,
            pattern_origin: Some(0),
            pattern_playhead: 0,
            pattern_loop_count: 0,
            pattern_overflows: 0,
            live_ack_overflows: 0,
        };
        let mut app = App::with_audio(Box::new(FakeAudio::ready().with_telemetry(telemetry)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("pattern 2".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.close_overlay();
        app.tick();
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        let screen = render_lines(80, 24, &app).join("\n");
        assert!(screen.contains("PATTERN 02"));
        assert!(screen.contains("P01 PLAY"));
        assert!(!screen.contains("P02 PLAY"));
        assert_eq!(render_symbol(80, 24, &app, 6, 4), ".");
    }

    #[test]
    fn stale_edit_generation_keeps_the_live_play_state_but_omits_a_fabricated_bar() {
        let telemetry = Telemetry {
            active_pads: [0; 3],
            rendered_frame: 0,
            last_triggered_frame: None,
            peak_left: 0.0,
            peak_right: 0.0,
            active_voices: 0,
            late_commands: 0,
            invalid_commands: 0,
            command_overflows: 0,
            pattern_slot: Some(sampler_core::PatternSlotId::new(0).unwrap()),
            pattern_generation: Some(0),
            pattern_playing: true,
            pattern_recording: false,
            pattern_origin: Some(0),
            pattern_playhead: 48_000,
            pattern_loop_count: 0,
            pattern_overflows: 0,
            live_ack_overflows: 0,
        };
        let mut app = App::with_audio(Box::new(FakeAudio::ready().with_telemetry(telemetry)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("tempo 121".to_owned()));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.tick();

        let perform = render_lines(80, 24, &app).join("\n");
        assert!(perform.contains("P01 PLAY"));
        assert!(!perform.contains("P01 PLAY 1/1"));

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let pattern = render_lines(80, 24, &app).join("\n");
        assert!(pattern.contains("P01 PLAY"));
        assert_eq!(render_symbol(80, 24, &app, 6, 4), ".");
    }

    #[test]
    fn logical_transport_bar_boundary_keeps_pgdn_cursor_toggle_and_summary_together() {
        let telemetry = Telemetry {
            active_pads: [0; 3],
            rendered_frame: 573_134,
            last_triggered_frame: None,
            peak_left: 0.0,
            peak_right: 0.0,
            active_voices: 0,
            late_commands: 0,
            invalid_commands: 0,
            command_overflows: 0,
            pattern_slot: Some(sampler_core::PatternSlotId::new(0).unwrap()),
            pattern_generation: Some(3),
            pattern_playing: true,
            pattern_recording: false,
            pattern_origin: Some(0),
            pattern_playhead: 573_134,
            pattern_loop_count: 0,
            pattern_overflows: 0,
            live_ack_overflows: 0,
        };
        let mut app = App::with_audio(Box::new(FakeAudio::ready().with_telemetry(telemetry)));
        for command in ["tempo 20.1", "bars 3", "resolution 1/16"] {
            app.open_palette();
            app.apply_terminal_event(Event::Paste(command.to_owned()));
            app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        app.tick();
        assert!(
            render_lines(80, 24, &app)
                .join("\n")
                .contains("P01 PLAY 2/3")
        );

        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert!(render_lines(80, 24, &app).join("\n").contains("BAR 2/3"));
        assert!(
            render_style(80, 24, &app, 6, 4)
                .add_modifier
                .contains(Modifier::REVERSED)
        );

        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(render_symbol(80, 24, &app, 6, 4), "O");
    }

    #[test]
    fn minimum_layout_keeps_grid_meters_and_status_visible() {
        let snapshot = render_lines(80, 24, &populated_app());
        assert_eq!(snapshot[0].chars().count(), 80);
        assert!(snapshot[0].starts_with("┌ BANK A · sampler-tui"));
        assert!(snapshot[0].ends_with("48kHz/2ch · RUN ┐"));
        assert!(snapshot[4].contains("PADS"));
        assert!(snapshot[4].contains("PERFORMANCE"));
        assert!(snapshot[2].contains("WAVE"));
        assert!(snapshot[2].contains('█'));
        assert!(snapshot[6].contains("[1 KICK"));
        assert!(snapshot[7].contains("[Q HAT"));
        assert!(snapshot[8].contains("[A ----"));
        assert!(snapshot[9].contains("[Z ----"));
        assert!(snapshot.iter().any(|line| line.contains("Overflow 3")));
        assert!(snapshot[22].contains("Ctrl+Q quit"));
    }

    #[test]
    fn sample_workspace_renders_the_bounded_editor_projection_at_minimum_size() {
        let mut app = loaded_states_app(FakeAudio::ready());
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        let screen = render_lines(80, 24, &app).join("\n");

        for expected in [
            "SAMPLE EDITOR",
            "PAD 01",
            "KICK.WAV",
            "START",
            "END",
            "ZOOM 0",
            "DRAFT CLEAN",
            "NORMALIZE OFF",
            "REVERSE OFF",
            "PITCH +0",
            "MODE ONESHOT",
            "FRAME 0..128",
        ] {
            assert!(screen.contains(expected), "missing {expected}:\n{screen}");
        }
    }

    #[test]
    fn sample_workspace_wide_layout_adds_metadata_without_losing_the_editor() {
        let mut app = loaded_states_app(FakeAudio::ready());
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        let minimum = render_lines(80, 24, &app).join("\n");
        let wide = render_lines(110, 24, &app).join("\n");

        assert!(minimum.contains("SAMPLE EDITOR"));
        assert!(wide.contains("SAMPLE EDITOR"));
        assert!(wide.contains("SOURCE 48kHz"));
        assert!(wide.contains("1024 PREVIEW"));
    }

    #[test]
    fn sample_workspace_keeps_fixed_marker_columns_and_uses_ascii_offscreen_direction() {
        let mut app = loaded_states_app(FakeAudio::ready());
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        for width in [80, 110] {
            assert_eq!(render_symbol(width, 24, &app, 2, 5), "S");
            assert_eq!(render_symbol(width, 24, &app, 77, 5), "E");
            assert!(
                render_style(width, 24, &app, 2, 5)
                    .add_modifier
                    .contains(Modifier::REVERSED)
            );
        }

        app.apply_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(render_symbol(80, 24, &app, 77, 5), ">");
    }

    #[test]
    fn sample_workspace_uses_only_the_preview_for_visible_positive_and_negative_waveform() {
        let mut app = loaded_states_app(FakeAudio::ready());
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        let waveform = render_lines(80, 24, &app)[5..12].join("\n");

        assert!(waveform.contains('+'));
        assert!(waveform.contains('-'));
    }

    #[test]
    fn sample_workspace_status_ignores_an_unrelated_pad_error() {
        let mut app = loaded_states_app(FakeAudio::ready());
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        let screen = render_lines(80, 24, &app).join("\n");

        assert!(screen.contains("STATUS CLEAN"));
        assert!(!screen.contains("decode failed"));
    }

    #[test]
    fn help_keeps_sample_safety_and_persistence_tails_visible_at_eighty_columns() {
        let mut app = ready_app();
        app.open_help();

        let screen = render_lines(80, 24, &app).join("\n");

        assert!(screen.contains("Apply is in-memory only"));
        assert!(screen.contains("Source file unchanged"));
        assert!(screen.contains("save · save-as <directory> · open-project <directory>"));
        assert!(screen.contains("Recovery: R restore · D discard · C cancel"));
    }

    #[test]
    fn project_unsaved_and_progress_overlays_show_safe_choices_at_eighty_columns() {
        let mut app = loaded_states_app(FakeAudio::ready());
        app.apply(InputAction::Quit);

        let unsaved = render_lines(80, 24, &app).join("\n");
        assert!(unsaved.contains("UNSAVED PROJECT"));
        assert!(unsaved.contains("Save"));
        assert!(unsaved.contains("Discard"));
        assert!(unsaved.contains("Cancel"));

        app.apply_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        let untitled = render_lines(80, 24, &app).join("\n");
        assert!(untitled.contains("use save-as <directory>"));

        let mut saving = ready_app();
        saving.open_palette();
        for character in "save-as /projects/new project".chars() {
            saving.apply_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        saving.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(saving.overlay(), Some(Overlay::ProjectSaveProgress)),
            "unexpected overlay {:?}, status {}",
            saving.overlay(),
            saving.status()
        );
        let progress = render_lines(80, 24, &saving).join("\n");
        assert!(progress.contains("SAVE PROJECT"));
        assert!(progress.contains("Waiting for the exact matching save result"));
    }

    #[test]
    fn project_open_recovery_overlay_shows_revision_path_and_keyboard_choices() {
        let mut app = ready_app();
        let directory = std::path::PathBuf::from("/projects/a project with a very long name");
        let token = app.request_open_project(&directory).unwrap();
        let project_id = ProjectId::from_bytes([0x51; 16]);
        let explicit = ProjectDocument::new_v2(
            project_id,
            "Explicit",
            i64::MAX as u64 - 1,
            Vec::new(),
            PatternWorkspace::new(48_000)
                .export_project_patterns()
                .unwrap(),
        )
        .unwrap();
        let recovery = ProjectDocument::new_v2(
            project_id,
            "Recovery",
            i64::MAX as u64,
            Vec::new(),
            PatternWorkspace::new(48_000)
                .export_project_patterns()
                .unwrap(),
        )
        .unwrap();
        assert!(app.apply_worker_result(WorkerResult::ProjectProbed {
            token,
            directory: directory.clone(),
            result: Ok(crate::ProjectProbe {
                directory,
                explicit: Some(Ok(explicit)),
                recovery: Some(Ok(recovery)),
            }),
        }));

        let screen = render_lines(80, 24, &app).join("\n");
        assert!(screen.contains("RECOVERY AVAILABLE"));
        assert!(screen.contains("Revision 9223372036854775807"));
        assert!(screen.contains("R Restore · D Discard · C Cancel"));
        assert!(screen.contains("a project with a very long name"));
    }

    #[test]
    fn help_lines_fit_the_seventy_column_inner_width_with_display_width_safe_text() {
        assert!(
            super::HELP_LINES
                .iter()
                .all(|line| super::display_width(line) <= 70)
        );
        assert_eq!(super::display_width(&super::truncate("샘플 help", 7)), 7);
    }

    #[test]
    fn sample_confirmation_overlays_clear_and_state_that_edits_are_in_memory_only() {
        let mut apply = loaded_states_app(FakeAudio::ready());
        apply.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        apply.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        apply.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        apply.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let apply_screen = render_lines(80, 24, &apply).join("\n");
        assert!(apply_screen.contains("in-memory only; source file unchanged"));
        assert!(apply_screen.contains("Project becomes MODIFIED until saved"));

        let mut discard = loaded_states_app(FakeAudio::ready());
        discard.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        discard.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        discard.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        discard.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(render_symbol(80, 24, &discard, 4, 8), "┌");
        let discard_screen = render_lines(80, 24, &discard).join("\n");
        assert!(discard_screen.contains("source file unchanged"));
    }

    #[test]
    fn apply_overlay_keeps_maximum_frame_counts_and_source_safety_visible() {
        let app = ready_app();
        let overlay = Overlay::ApplySample {
            pad: pad(0),
            before_frames: 8_388_608,
            after_frames: 8_388_607,
        };
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render_overlay(frame, frame.area(), &app, &overlay))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut screen = String::new();
        for y in 0..24 {
            for x in 0..80 {
                screen.push_str(buffer[(x, y)].symbol());
            }
        }

        assert!(screen.contains("Before 8388608 frames"));
        assert!(screen.contains("After 8388607 frames"));
        assert!(screen.contains("in-memory only; source file unchanged"));
    }

    #[test]
    fn help_palette_picker_and_device_error_are_centered_overlays() {
        let mut help = ready_app();
        help.open_help();
        let mut palette = ready_app();
        palette.open_palette();
        let mut picker = ready_app();
        picker.open_picker_at("/samples");
        let failed = App::without_audio("device disconnected");

        for (app, title) in [
            (help, "HELP"),
            (palette, "COMMAND"),
            (picker, "LOAD SAMPLE"),
            (failed, "AUDIO DEVICE ERROR"),
        ] {
            let snapshot = render_lines(100, 30, &app);
            assert!(snapshot.iter().any(|line| line.contains(title)));
            assert!(snapshot.iter().any(|line| line.contains("BANK A")));
        }
    }

    #[test]
    fn overlay_rects_are_exactly_centered_and_clear_base_cells() {
        let mut help = ready_app();
        help.open_help();
        let mut palette = ready_app();
        palette.open_palette();
        let mut picker = ready_app();
        picker.open_picker_at("/samples");
        let failed = App::without_audio("device disconnected");

        for (app, rect) in [
            (help, (14, 3, 72, 23)),
            (palette, (19, 12, 62, 5)),
            (picker, (14, 5, 72, 19)),
            (failed, (19, 11, 62, 7)),
        ] {
            let (x, y, width, height) = rect;
            assert_eq!(render_symbol(100, 30, &app, x, y), "┌");
            assert_eq!(render_symbol(100, 30, &app, x + width - 1, y), "┐");
            assert_eq!(render_symbol(100, 30, &app, x, y + height - 1), "└");
            assert_eq!(
                render_symbol(100, 30, &app, x + width - 1, y + height - 1),
                "┘"
            );
        }

        let mut palette = ready_app();
        palette.open_palette();
        assert_eq!(render_symbol(100, 30, &palette, 49, 14), " ");
    }

    #[test]
    fn pad_states_have_monochrome_text_markers() {
        let snapshot = render_lines(80, 24, &populated_app()).join("\n");
        assert!(snapshot.contains("KICK●!"), "held marker missing");
        assert!(snapshot.contains("HAT…"), "loading marker missing");
        assert!(snapshot.contains("CLAP×"), "error marker missing");
        assert!(snapshot.contains(">[1"), "selected marker missing");
        assert!(snapshot.contains("----·"), "empty marker missing");
    }

    #[test]
    fn production_scale_preview_amplitudes_render_a_visible_waveform() {
        let mut app = ready_app();
        let sample_path = std::path::PathBuf::from("/samples/visible.wav");
        let request = app.begin_load(pad(0), sample_path.clone()).unwrap();
        let crate::WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };
        assert!(
            app.apply_worker_result(WorkerResult::Loaded {
                pad: pad(0),
                generation,
                purpose: LoadPurpose::User,
                path: sample_path,
                result: Ok(LoadedSample {
                    fingerprint: crate::SourceFingerprint::from_encoded_bytes(
                        std::path::Path::new("fixture.wav"),
                        &[],
                    )
                    .unwrap(),
                    base: Arc::new(SampleBuffer::new(48_000, vec![0.5; 128]).unwrap()),
                    base_preview: Arc::new(
                        [PreviewColumn { min: -8, max: 8 }; EDIT_PREVIEW_COLUMNS],
                    ),
                    rendered: Arc::new(SampleBuffer::new(48_000, vec![0.5; 128]).unwrap()),
                    rendered_preview: Arc::new(
                        [PreviewColumn { min: -8, max: 8 }; EDIT_PREVIEW_COLUMNS],
                    ),
                    recipe: sampler_core::SampleEditRecipe::identity(),
                    source_rate: 48_000,
                    source_frames: 64,
                    duration: Duration::from_secs_f64(64.0 / 48_000.0),
                }),
            })
        );

        let snapshot = render_lines(80, 24, &app).join("\n");

        assert!(snapshot.lines().any(|line| line.contains("WAVE █")));
    }

    #[test]
    fn tiny_and_empty_overlay_inputs_never_panic() {
        for (width, height) in [(0, 0), (1, 1), (2, 24), (80, 1), (79, 24)] {
            let _ = render_lines(width, height, &ready_app());
        }

        let mut app = ready_app();
        app.open_picker_at("/");
        let snapshot = render_lines(80, 24, &app);
        assert!(snapshot.iter().any(|line| line.contains("LOAD SAMPLE")));
        assert!(matches!(app.overlay(), Some(Overlay::FilePicker)));
    }

    #[test]
    fn tick_clamps_non_finite_meters_and_render_uses_cached_counters() {
        let telemetry = Telemetry {
            active_pads: [0; 3],
            rendered_frame: 512,
            last_triggered_frame: Some(500),
            peak_left: f32::NAN,
            peak_right: 1.5,
            active_voices: 2,
            late_commands: 1,
            invalid_commands: 2,
            command_overflows: 3,
            pattern_slot: None,
            pattern_generation: None,
            pattern_playing: false,
            pattern_recording: false,
            pattern_origin: None,
            pattern_playhead: 0,
            pattern_loop_count: 0,
            pattern_overflows: 0,
            live_ack_overflows: 0,
        };
        let mut app = App::with_audio(Box::new(FakeAudio::ready().with_telemetry(telemetry)));

        app.tick();

        assert_eq!(app.meter_levels(), (0.0, 1.0));
        let snapshot = render_lines(80, 24, &app).join("\n");
        for expected in [
            "Voices 02",
            "Late 1",
            "Invalid 2",
            "Overflow 3",
            "Frame 512",
        ] {
            assert!(snapshot.contains(expected), "missing {expected}");
        }

        app.tick();
        assert!((app.meter_levels().1 - 0.85).abs() < f32::EPSILON);
    }

    #[test]
    fn selected_held_loading_and_error_cells_have_stable_modifiers() {
        let app = populated_app();
        let selected_held = render_style(80, 24, &app, 2, 6);
        let loading = render_style(80, 24, &app, 2, 7);
        let error = render_style(80, 24, &app, 14, 7);

        assert!(selected_held.add_modifier.contains(Modifier::REVERSED));
        assert!(selected_held.add_modifier.contains(Modifier::UNDERLINED));
        assert!(loading.add_modifier.contains(Modifier::DIM));
        assert!(error.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn active_and_held_are_independent_pad_states() {
        let telemetry = Telemetry {
            active_pads: [1, 0, 0],
            rendered_frame: 64,
            last_triggered_frame: Some(0),
            peak_left: 0.0,
            peak_right: 0.0,
            active_voices: 1,
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
        };
        let mut active_only = loaded_states_app(FakeAudio::ready().with_telemetry(telemetry));
        active_only.apply(InputAction::PadPress(0));
        active_only.apply(InputAction::PadRelease(0));
        active_only.tick();

        assert!(active_only.pad(pad(0)).active);
        assert!(!active_only.is_pad_held(0));
        let active_snapshot = render_lines(80, 24, &active_only).join("\n");
        assert!(active_snapshot.contains("KICK▶ "));

        let held_snapshot = render_lines(80, 24, &populated_app()).join("\n");
        assert!(held_snapshot.contains("KICK●!"));
        assert!(!held_snapshot.contains("KICK▶!"));

        let mut both = populated_app_with_audio(
            FakeAudio::ready()
                .with_stop_error("keep held")
                .with_telemetry(telemetry),
        );
        both.tick();
        assert!(both.is_pad_held(0));
        assert!(render_lines(80, 24, &both).join("\n").contains("KICK▶!"));
    }

    #[test]
    fn render_reads_only_cached_app_view_state() {
        let reads = Rc::new(Cell::new(0));
        let app = App::with_audio(Box::new(
            FakeAudio::ready().with_format_reads(Rc::clone(&reads)),
        ));
        let before_render = reads.get();

        let _ = render_lines(80, 24, &app);

        assert_eq!(reads.get(), before_render);
    }

    #[test]
    fn wide_layout_preserves_the_complete_interaction_model() {
        let snapshot = render_lines(110, 24, &populated_app()).join("\n");

        for expected in [
            "[1 KICK",
            "[Q HAT",
            "[A ----",
            "[Z ----",
            "PERFORMANCE",
            "Overflow",
            "Ctrl+Q quit",
        ] {
            assert!(snapshot.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn wider_terminals_never_shrink_the_performance_column() {
        let app = ready_app();
        let mut previous = 0usize;
        let mut at_109 = 0;
        for width in 80..=140 {
            let row = &render_lines(width, 24, &app)[4];
            let performance_x = row
                .chars()
                .enumerate()
                .filter_map(|(index, character)| (character == '┌').then_some(index))
                .nth(1)
                .expect("performance block starts on row four");
            let performance_width = usize::from(width).saturating_sub(performance_x + 1);
            assert!(
                performance_width >= previous,
                "performance shrank at width {width}: {previous} -> {performance_width}"
            );
            if width == 109 {
                at_109 = performance_width;
            }
            if width == 110 {
                assert!(performance_width > at_109);
            }
            previous = performance_width;
        }
    }

    #[test]
    fn truncation_is_utf8_safe_and_uses_terminal_display_width() {
        let fitted = super::fit("긴이름의샘플.wav", 8);
        assert_eq!(super::display_width(&fitted), 8);
        assert!(fitted.is_char_boundary(fitted.len()));

        let mut picker = ready_app();
        picker.open_picker_at("/아주/긴/다중바이트/샘플/디렉터리/경로");
        let snapshot = render_lines(80, 24, &picker);
        assert!(snapshot.iter().any(|line| line.contains("LOAD SAMPLE")));
    }

    #[test]
    fn palette_cursor_tracks_multibyte_middle_and_long_horizontal_windows() {
        let mut unicode = ready_app();
        unicode.open_palette();
        unicode.apply_terminal_event(Event::Paste("가나다".to_owned()));
        unicode.apply_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(
            super::palette_window(unicode.palette_text(), unicode.palette_cursor(), 60),
            ":가나▏다"
        );
        let snapshot = render_lines(80, 24, &unicode).join("\n");
        assert!(snapshot.contains('▏'));

        let mut long = ready_app();
        long.open_palette();
        long.apply_terminal_event(Event::Paste("x".repeat(120)));
        long.apply_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        for _ in 0..60 {
            long.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        }
        let snapshot = render_lines(80, 24, &long).join("\n");
        assert!(snapshot.contains("x▏x"));
        assert_eq!(snapshot.matches('▏').count(), 1);
    }

    #[test]
    fn picker_viewport_always_contains_the_selected_entry() {
        for cursor in [0, 14, 15, 19] {
            let snapshot = render_lines(80, 24, &picker_at(cursor)).join("\n");
            assert!(
                snapshot.contains(&format!("> sample-{cursor:02}.wav")),
                "cursor {cursor} was outside viewport"
            );
        }
    }

    #[test]
    fn picker_distinguishes_loading_empty_and_failed_scans_with_old_entries() {
        let mut initial_loading = ready_app();
        initial_loading.open_picker_at("/initial-target");
        let snapshot = render_lines(80, 24, &initial_loading).join("\n");
        assert!(snapshot.contains("Loading /initial-target…"));
        assert!(!snapshot.contains("(empty directory)"));

        let mut loading = ready_app();
        loading.open_picker_at("/samples");
        complete_picker_scan(&mut loading, 2);
        loading.open_picker_at("/slow-target");
        let snapshot = render_lines(80, 24, &loading).join("\n");
        assert!(snapshot.contains("sample-00.wav"));
        assert!(snapshot.contains("Loading /slow-target…"));

        let requests = loading.take_worker_requests();
        let [
            crate::WorkerRequest::ScanDirectory {
                request_id, path, ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected pending picker scan")
        };
        assert!(loading.apply_worker_result(WorkerResult::Scanned {
            request_id: *request_id,
            path: path.clone(),
            result: Err(
                "permission denied because this diagnostic is intentionally very long".to_owned()
            ),
        }));
        let snapshot = render_lines(80, 24, &loading).join("\n");
        assert!(snapshot.contains("sample-00.wav"));
        assert!(snapshot.contains("× /slow-target: permission denied"));
        let error_line = snapshot
            .lines()
            .find(|line| line.contains("× /slow-target"))
            .unwrap();
        assert!(error_line.contains('…'));
        assert!(!error_line.contains("intentionally very long"));

        let mut empty = ready_app();
        empty.open_picker_at("/empty");
        complete_picker_scan(&mut empty, 0);
        let snapshot = render_lines(80, 24, &empty).join("\n");
        assert!(snapshot.contains("(empty directory)"));
        assert!(!snapshot.contains("Loading"));
    }

    #[test]
    fn perform_header_renders_untitled_project_save_truth() {
        let snapshot = render_lines(80, 24, &ready_app()).join("\n");
        assert!(snapshot.contains("UNTITLED · SAVED"));
    }

    fn status_for(
        source: CaptureSource,
        frames: usize,
        max_frames: usize,
        peak: f32,
        hard_limit: bool,
    ) -> CaptureStatus {
        CaptureStatus {
            token: 1,
            source,
            target: pad(0),
            state: CaptureState::Recording,
            frames,
            max_frames,
            peak,
            hard_limit,
        }
    }

    fn recording_app(status: CaptureStatus) -> App {
        let mut app = App::with_audio(Box::new(FakeAudio::ready().with_capture_status(status)));
        app.request_capture_with_limit_for_test(status.source, status.max_frames)
            .unwrap();
        let _ = app.maintain_capture();
        app
    }

    #[test]
    fn capture_recording_status_is_bounded_and_honest_for_both_sources_at_eighty_and_wide() {
        let input = recording_app(status_for(
            CaptureSource::Input,
            66_150,
            88_200,
            0.75,
            false,
        ));
        let resample = recording_app(status_for(
            CaptureSource::Resample,
            60_000,
            96_000,
            1.125,
            false,
        ));

        for (app, source, elapsed, maximum, peak) in [
            (&input, "INPUT", "00:01.500", "00:02.000", "0.750"),
            (&resample, "RESAMPLE", "00:01.250", "00:02.000", "1.125"),
        ] {
            for width in [80, 118] {
                let screen = render_lines(width, 24, app).join("\n");
                for expected in [source, "A01", elapsed, maximum, peak] {
                    assert!(
                        screen.contains(expected),
                        "missing {expected:?} at {width} columns:\n{screen}"
                    );
                }
            }
        }
    }

    #[test]
    fn capture_max_replacement_discard_finalizing_and_device_loss_are_explicit_overlays() {
        let max = recording_app(status_for(
            CaptureSource::Resample,
            96_000,
            96_000,
            0.999,
            true,
        ));
        let max_screen = render_lines(80, 24, &max).join("\n");
        assert!(max_screen.contains("MAX"));
        assert!(max_screen.contains("00:02.000"));

        let status = status_for(CaptureSource::Resample, 24_000, 96_000, 0.5, false);
        let mut replacement = loaded_states_app(FakeAudio::ready().with_capture_status(status));
        let hat_path = std::path::PathBuf::from("/samples/HAT.wav");
        let crate::WorkerRequest::LoadSample { generation, .. } = replacement
            .begin_load(pad(4), hat_path.clone())
            .expect("replace the pending fixture load")
        else {
            panic!("expected sample load")
        };
        assert!(replacement.apply_worker_result(WorkerResult::Loaded {
            pad: pad(4),
            generation,
            purpose: LoadPurpose::User,
            path: hat_path,
            result: Err(LoadSampleError::Decode("fixture load cancelled".to_owned())),
        }));
        let long_target = std::path::PathBuf::from(
            "/samples/capture-target-with-an-intentionally-long-portable-source-name.wav",
        );
        let crate::WorkerRequest::LoadSample { generation, .. } = replacement
            .begin_load(pad(0), long_target.clone())
            .expect("replace the selected fixture sample")
        else {
            panic!("expected sample load")
        };
        assert!(
            replacement.apply_worker_result(WorkerResult::Loaded {
                pad: pad(0),
                generation,
                purpose: LoadPurpose::User,
                path: long_target,
                result: Ok(LoadedSample {
                    fingerprint: crate::SourceFingerprint::from_encoded_bytes(
                        std::path::Path::new("long-target.wav"),
                        &[],
                    )
                    .unwrap(),
                    base: Arc::new(SampleBuffer::new(48_000, vec![0.0; 256]).unwrap()),
                    base_preview: Arc::new([PreviewColumn::default(); EDIT_PREVIEW_COLUMNS]),
                    rendered: Arc::new(SampleBuffer::new(48_000, vec![0.0; 256]).unwrap()),
                    rendered_preview: Arc::new([PreviewColumn::default(); EDIT_PREVIEW_COLUMNS]),
                    recipe: sampler_core::SampleEditRecipe::identity(),
                    source_rate: 48_000,
                    source_frames: 128,
                    duration: Duration::from_secs_f64(128.0 / 48_000.0),
                }),
            })
        );
        replacement
            .request_capture_with_limit_for_test(CaptureSource::Resample, 96_000)
            .unwrap();
        let replacement_screen = render_lines(80, 24, &replacement).join("\n");
        assert!(replacement_screen.contains("REPLACE PAD A01"));
        assert!(replacement_screen.contains("CAPTURE-TARGET-WITH-AN-INTENTIONALLY-LONG"));
        assert!(replacement_screen.contains('…'));
        assert!(replacement_screen.contains("Enter"));
        assert!(replacement_screen.contains("Esc"));

        replacement.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        replacement.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let discard = render_lines(80, 24, &replacement).join("\n");
        assert!(discard.contains("DISCARD CAPTURE"));
        assert!(discard.contains("old pad remains unchanged"));

        replacement.apply_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        replacement.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let finalizing = render_lines(80, 24, &replacement).join("\n");
        assert!(finalizing.contains("FINALIZING CAPTURE"));
        assert!(finalizing.contains("exact callback, worker, and audio result"));

        let error = "default input device disappeared while recording a path/label that is intentionally very long";
        let mut failed = App::with_audio(Box::new(
            FakeAudio::ready()
                .with_capture_status(status_for(CaptureSource::Input, 4_410, 88_200, 0.25, false))
                .with_capture_error(crate::CaptureError::InputRuntime(error.to_owned())),
        ));
        failed
            .request_capture_with_limit_for_test(CaptureSource::Input, 88_200)
            .unwrap();
        assert!(failed.maintain_capture());
        let failed_screen = render_lines(80, 24, &failed).join("\n");
        assert!(failed_screen.contains("CAPTURE FAILED"));
        assert!(failed_screen.contains("default input device disappeared"));
        assert!(failed_screen.contains("Discard"));
        assert!(failed_screen.contains("Cancel"));
        assert!(!failed_screen.contains("Retry finalization"));
    }

    #[test]
    fn capture_overlays_clear_stale_long_cells_and_safety_phrases_fit_seventy_columns() {
        let error = "input device loss diagnostic with a deliberately long unique stale tail ZYXWVUTSRQPONMLK";
        let mut app = App::with_audio(Box::new(
            FakeAudio::ready()
                .with_capture_status(status_for(CaptureSource::Input, 100, 88_200, 0.2, false))
                .with_capture_error(crate::CaptureError::InputRuntime(error.to_owned())),
        ));
        app.request_capture_with_limit_for_test(CaptureSource::Input, 88_200)
            .unwrap();
        assert!(app.maintain_capture());

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        app.cancel_capture().unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut after = String::new();
        for y in 0..30 {
            for x in 0..100 {
                after.push_str(buffer[(x, y)].symbol());
            }
        }
        assert!(!after.contains("ZYXWVUTSRQPONMLK"));

        for phrase in [
            "Enter starts · Esc cancels without changing the old pad.",
            "Enter discards · Esc keeps the capture.",
            "Waiting for the exact callback, worker, and audio result.",
            "Stop all and held-pad releases remain available.",
            "Finalize · Discard · Cancel are explicit choices.",
        ] {
            assert!(super::display_width(phrase) <= 70, "too wide: {phrase}");
        }
    }
}
