use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph};

use crate::input::PAD_KEYS;
use crate::{App, Overlay, PREVIEW_COLUMNS, PadLoadState, PadView};

const MIN_WIDTH: u16 = 80;
const MIN_HEIGHT: u16 = 24;
const WAVE_CHARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

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
    let required = format!("Required: {MIN_WIDTH}x{MIN_HEIGHT}");
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
    let bank = char::from(b'A'.saturating_add(u8::from(app.active_bank())));
    let (format, state) = match app.audio_format() {
        Some((rate, channels)) => (format!("{}kHz/{channels}ch", rate / 1_000), "RUN"),
        None => ("--kHz/--ch".to_owned(), "NO AUDIO"),
    };
    let outer = Block::new()
        .borders(Borders::ALL)
        .title(Line::from(format!(" BANK {bank} · sampler-tui ")))
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
    let rows = [
        format!("Voices {:02}", telemetry.active_voices),
        format!("Late {}", telemetry.late_commands),
        format!("Invalid {}", telemetry.invalid_commands),
        format!("Overflow {}", telemetry.command_overflows),
        format!("Frame {}", telemetry.rendered_frame),
        format!("Release keys: {release}"),
        format!("Device {rate}Hz/{channels}ch"),
        "Pattern: next slice".to_owned(),
    ];
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
        Overlay::Help => render_list_overlay(
            frame,
            area,
            " HELP ",
            58,
            15,
            [
                "PERFORM",
                "1 2 3 4 / Q W E R / A S D F / Z X C V  play pads",
                "Shift + pad                                stop pad",
                "[ / ]                                      previous / next bank",
                "Arrow keys                                 select pad",
                "Enter                                      trigger selected pad",
                "l                                          load sample",
                ":                                          command palette",
                "Shift+Esc                                  stop all",
                "Ctrl+Q / Ctrl+C                            quit",
                "Esc or ?                                   close help",
            ],
        ),
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
    use sampler_audio::{Frame, SampleBuffer, SampleSlot, Telemetry};
    use sampler_core::{PadId, PadSettings};

    use crate::audio::AudioPort;
    use crate::input::InputAction;
    use crate::loader::{LoadSampleError, LoadedSample, WorkerResult};
    use crate::{App, DirectoryEntry, DirectoryEntryKind, Overlay, PREVIEW_COLUMNS, PreviewColumn};

    use super::render;

    struct FakeAudio {
        stop_error: Option<String>,
        telemetry: Option<Telemetry>,
        format_reads: Option<Rc<Cell<usize>>>,
    }

    impl FakeAudio {
        fn ready() -> Self {
            Self {
                stop_error: None,
                telemetry: None,
                format_reads: None,
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

        fn record_format_read(&self) {
            if let Some(reads) = &self.format_reads {
                reads.set(reads.get().saturating_add(1));
            }
        }
    }

    impl AudioPort for FakeAudio {
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
            crate::WorkerRequest::ScanDirectory { .. } | crate::WorkerRequest::Shutdown => {
                unreachable!()
            }
        };
        let mut preview = [PreviewColumn::default(); PREVIEW_COLUMNS];
        for (index, column) in preview.iter_mut().enumerate() {
            let height = i8::try_from(index % 9).unwrap();
            *column = PreviewColumn {
                min: -height,
                max: height,
            };
        }
        app.apply_worker_result(WorkerResult::Loaded {
            pad: kick,
            generation,
            path,
            result: Ok(LoadedSample {
                buffer: Arc::new(SampleBuffer::new(48_000, vec![0.0; 256]).unwrap()),
                source_rate: 48_000,
                source_frames: 128,
                duration: Duration::from_secs_f64(128.0 / 48_000.0),
                preview,
            }),
        });
        app.begin_load(pad(4), "/samples/HAT.wav");
        let error_path = std::path::PathBuf::from("/samples/CLAP.wav");
        let request = app.begin_load(pad(5), error_path.clone()).unwrap();
        let generation = match request {
            crate::WorkerRequest::LoadSample { generation, .. } => generation,
            crate::WorkerRequest::ScanDirectory { .. } | crate::WorkerRequest::Shutdown => {
                unreachable!()
            }
        };
        app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(5),
            generation,
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
    fn minimum_layout_keeps_grid_meters_and_status_visible() {
        let snapshot = render_lines(80, 24, &populated_app());
        assert_eq!(snapshot[0].chars().count(), 80);
        assert!(snapshot[0].starts_with("┌ BANK A · sampler-tui"));
        assert!(snapshot[0].ends_with("48kHz/2ch · RUN ┐"));
        assert!(snapshot[4].contains("PADS"));
        assert!(snapshot[4].contains("PERFORMANCE"));
        assert!(snapshot[2].contains("WAVE"));
        assert!(snapshot[2].contains('▇'));
        assert!(snapshot[6].contains("[1 KICK"));
        assert!(snapshot[7].contains("[Q HAT"));
        assert!(snapshot[8].contains("[A ----"));
        assert!(snapshot[9].contains("[Z ----"));
        assert!(snapshot.iter().any(|line| line.contains("Overflow 3")));
        assert!(snapshot[22].contains("Ctrl+Q quit"));
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
            (help, (21, 7, 58, 15)),
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
        assert!(app.apply_worker_result(WorkerResult::Loaded {
            pad: pad(0),
            generation,
            path: sample_path,
            result: Ok(LoadedSample {
                buffer: Arc::new(SampleBuffer::new(48_000, vec![0.5; 128]).unwrap()),
                source_rate: 48_000,
                source_frames: 64,
                duration: Duration::from_secs_f64(64.0 / 48_000.0),
                preview: [PreviewColumn { min: -8, max: 8 }; PREVIEW_COLUMNS],
            }),
        }));

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
}
