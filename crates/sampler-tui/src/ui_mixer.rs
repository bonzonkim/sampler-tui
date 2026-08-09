use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use sampler_core::PlaybackMode;

use crate::App;
use crate::mixer::{DelayField, MixerSection, PadField, ReverbField};
use crate::ui::{load_state_name, pad_label, safe_meter_ratio, truncate};

pub(crate) fn render_mixer(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
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
            " BANK {bank} · MIXER / FX · {} ",
            app.project_header()
        )))
        .title(Line::from(format!(" {format} · {state} ")).right_aligned());
    let inner = outer.inner(area);
    frame.render_widget(outer, area);
    if inner.is_empty() {
        return;
    }

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(4),
        ])
        .split(inner);
    render_pad_header(frame, sections[0], app);
    render_sections(frame, sections[1], app);
    render_mixer_status(frame, sections[2], app);
}

fn render_mixer_status(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let block = Block::new().borders(Borders::TOP).title(" STATUS ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let mut context = focused_context(app);
    if !app.status().is_empty() {
        context.push_str(" · ");
        context.push_str(app.status());
    }
    let rows = [
        context,
        "Ctrl+←/→ section · ↑/↓ field · ←/→ edit".to_owned(),
        "Enter toggle · Backspace reset · Esc perform · ? help · : cmd".to_owned(),
    ];
    render_lines(frame, inner, &rows);
}

fn focused_context(app: &App) -> String {
    let selected = app.selected_pad().min(15);
    let offset = usize::from(u8::from(app.active_bank())) * 16 + selected;
    let pad = &app.pads()[offset];
    let Some(pad_id) = app.selected_pad_id() else {
        return format!("PAD {:02} · unavailable", selected + 1);
    };
    let mix = app.pad_mix(pad_id);
    let master = app.master_mix();
    let cursor = app.mixer_cursor();
    let (section, field, value) = match cursor.section() {
        MixerSection::Pad => match cursor.pad_field() {
            PadField::Level => ("PAD MIX", "Level", db(pad.settings.gain_db)),
            PadField::Pan => ("PAD MIX", "Pan", pan(pad.settings.pan)),
            PadField::Mute => ("PAD MIX", "Mute", on_off(mix.muted).to_owned()),
            PadField::Choke => (
                "PAD MIX",
                "Choke",
                pad.settings
                    .choke_group
                    .map_or_else(|| "--".to_owned(), |group| group.get().to_string()),
            ),
            PadField::DelaySend => ("PAD MIX", "Delay", percent(mix.delay_send)),
            PadField::ReverbSend => ("PAD MIX", "Reverb", percent(mix.reverb_send)),
        },
        MixerSection::Master => ("MASTER", "Level", db(master.gain_db)),
        MixerSection::Delay => match cursor.delay_field() {
            DelayField::Enabled => ("DELAY", "Enabled", on_off(master.delay.enabled).to_owned()),
            DelayField::Time => ("DELAY", "Time", format!("{} ms", master.delay.time_ms)),
            DelayField::Feedback => ("DELAY", "Feedback", percent(master.delay.feedback)),
            DelayField::Return => ("DELAY", "Return", db(master.delay.return_db)),
        },
        MixerSection::Reverb => match cursor.reverb_field() {
            ReverbField::Enabled => (
                "REVERB",
                "Enabled",
                on_off(master.reverb.enabled).to_owned(),
            ),
            ReverbField::Room => ("REVERB", "Room", percent(master.reverb.room_size)),
            ReverbField::Damping => ("REVERB", "Damping", percent(master.reverb.damping)),
            ReverbField::Return => ("REVERB", "Return", db(master.reverb.return_db)),
        },
    };
    format!("PAD {:02} · {section} · {field} · {value}", selected + 1)
}

fn render_pad_header(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let selected = app.selected_pad().min(15);
    let offset = usize::from(u8::from(app.active_bank())) * 16 + selected;
    let pad = &app.pads()[offset];
    let label = pad_label(pad, app.pad_display_source(offset)).to_uppercase();
    let lines = [
        format!(
            " PAD {:02} · {} · {}",
            selected + 1,
            label,
            load_state_name(&pad.state)
        ),
        " Ctrl+←/→ section · ↑/↓ field · ←/→ edit · Enter toggle · Backspace reset · Esc perform"
            .to_owned(),
    ];
    for (index, line) in lines.into_iter().enumerate() {
        let y = area.y.saturating_add(u16::try_from(index).unwrap_or(0));
        if y >= area.bottom() {
            break;
        }
        frame.render_widget(
            Paragraph::new(truncate(&line, usize::from(area.width))),
            Rect::new(area.x, y, area.width, 1),
        );
    }
}

fn render_sections(frame: &mut Frame, area: Rect, app: &App) {
    if area.is_empty() {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(1)])
        .split(area);
    render_pad_section(frame, rows[0], app);
    let effects = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(rows[1]);
    render_master_section(frame, effects[0], app);
    render_delay_section(frame, effects[1], app);
    render_reverb_section(frame, effects[2], app);
}

fn section_block(title: &str, focused: bool) -> Block<'static> {
    let title = if focused {
        format!(" {title} [FOCUS] ")
    } else {
        format!(" {title} ")
    };
    let style = if focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Block::new().borders(Borders::ALL).title(title).style(style)
}

fn render_pad_section(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.mixer_cursor().section() == MixerSection::Pad;
    let block = section_block("PAD MIX", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let selected = app.selected_pad().min(15);
    let offset = usize::from(u8::from(app.active_bank())) * 16 + selected;
    let pad = &app.pads()[offset];
    let Some(pad_id) = app.selected_pad_id() else {
        return;
    };
    let mix = app.pad_mix(pad_id);
    let cursor = app.mixer_cursor();
    let marker = |field| {
        if focused && cursor.pad_field() == field {
            ">"
        } else {
            " "
        }
    };
    let mode = match pad.settings.mode {
        PlaybackMode::Gate => "GATE",
        PlaybackMode::OneShot => "ONE SHOT",
        PlaybackMode::Loop => "LOOP",
    };
    let choke = pad
        .settings
        .choke_group
        .map_or_else(|| "--".to_owned(), |group| group.get().to_string());
    let values = [
        (
            format!("  Mode {mode}"),
            format!(
                "{} Level {}",
                marker(PadField::Level),
                db(pad.settings.gain_db)
            ),
        ),
        (
            format!("{} Pan {}", marker(PadField::Pan), pan(pad.settings.pan)),
            format!("{} Mute {}", marker(PadField::Mute), on_off(mix.muted)),
        ),
        (
            format!("{} Choke {choke}", marker(PadField::Choke)),
            format!(
                "{} Delay {}",
                marker(PadField::DelaySend),
                percent(mix.delay_send)
            ),
        ),
        (
            format!(
                "{} Reverb {}",
                marker(PadField::ReverbSend),
                percent(mix.reverb_send)
            ),
            String::new(),
        ),
    ];
    render_pairs(frame, inner, &values);
}

fn render_master_section(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.mixer_cursor().section() == MixerSection::Master;
    let block = section_block("MASTER", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let marker = if focused { ">" } else { " " };
    let (left, right) = app.meter_levels();
    let lines = [
        format!("{marker} Level {}", db(app.master_mix().gain_db)),
        meter("L", left, inner.width),
        meter("R", right, inner.width),
    ];
    render_lines(frame, inner, &lines);
}

fn render_delay_section(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.mixer_cursor().section() == MixerSection::Delay;
    let block = section_block("DELAY", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let cursor = app.mixer_cursor();
    let marker = |field| {
        if focused && cursor.delay_field() == field {
            ">"
        } else {
            " "
        }
    };
    let delay = app.master_mix().delay;
    let lines = [
        format!(
            "{} Enabled {}",
            marker(DelayField::Enabled),
            on_off(delay.enabled)
        ),
        format!("{} Time {} ms", marker(DelayField::Time), delay.time_ms),
        format!(
            "{} Feedback {}",
            marker(DelayField::Feedback),
            percent(delay.feedback)
        ),
        format!(
            "{} Return {}",
            marker(DelayField::Return),
            db(delay.return_db)
        ),
    ];
    render_lines(frame, inner, &lines);
}

fn render_reverb_section(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.mixer_cursor().section() == MixerSection::Reverb;
    let block = section_block("REVERB", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let cursor = app.mixer_cursor();
    let marker = |field| {
        if focused && cursor.reverb_field() == field {
            ">"
        } else {
            " "
        }
    };
    let reverb = app.master_mix().reverb;
    let lines = [
        format!(
            "{} Enabled {}",
            marker(ReverbField::Enabled),
            on_off(reverb.enabled)
        ),
        format!(
            "{} Room {}",
            marker(ReverbField::Room),
            percent(reverb.room_size)
        ),
        format!(
            "{} Damping {}",
            marker(ReverbField::Damping),
            percent(reverb.damping)
        ),
        format!(
            "{} Return {}",
            marker(ReverbField::Return),
            db(reverb.return_db)
        ),
    ];
    render_lines(frame, inner, &lines);
}

fn render_pairs(frame: &mut Frame, area: Rect, rows: &[(String, String)]) {
    let column_width = usize::from(area.width) / 2;
    for (index, (left, right)) in rows.iter().enumerate() {
        let y = area.y.saturating_add(u16::try_from(index).unwrap_or(0));
        if y >= area.bottom() {
            break;
        }
        let line = format!(
            "{:<width$}{}",
            truncate(left, column_width),
            truncate(right, usize::from(area.width).saturating_sub(column_width)),
            width = column_width
        );
        frame.render_widget(
            Paragraph::new(truncate(&line, usize::from(area.width))),
            Rect::new(area.x, y, area.width, 1),
        );
    }
}

fn render_lines(frame: &mut Frame, area: Rect, rows: &[String]) {
    for (index, row) in rows.iter().enumerate() {
        let y = area.y.saturating_add(u16::try_from(index).unwrap_or(0));
        if y >= area.bottom() {
            break;
        }
        frame.render_widget(
            Paragraph::new(truncate(row, usize::from(area.width))),
            Rect::new(area.x, y, area.width, 1),
        );
    }
}

fn db(value: f32) -> String {
    if value > 0.0 {
        format!("+{value:.1} dB")
    } else {
        format!("{value:.1} dB")
    }
}

fn pan(value: f32) -> String {
    if value.abs() < f32::EPSILON {
        "C".to_owned()
    } else if value < 0.0 {
        format!("L{}", (value.abs() * 100.0).round() as u8)
    } else {
        format!("R{}", (value * 100.0).round() as u8)
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "ON" } else { "OFF" }
}

fn percent(value: f32) -> String {
    format!("{}%", (safe_meter_ratio(value) * 100.0).round() as u8)
}

fn meter(label: &str, value: f32, width: u16) -> String {
    let ratio = safe_meter_ratio(value);
    let summary = format!("{label} {}%", (ratio * 100.0).round() as u8);
    if width < 30 {
        return summary;
    }
    let bar_width = usize::from(width).saturating_sub(summary.len() + 3);
    let filled = (ratio * bar_width as f64).round() as usize;
    format!(
        "{summary} [{}{}]",
        "█".repeat(filled.min(bar_width)),
        "·".repeat(bar_width.saturating_sub(filled))
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::{Line, Span};
    use sampler_audio::{Frame, SampleBuffer, SampleSlot, Telemetry};
    use sampler_core::{
        ChokeGroup, DelaySettings, MasterMixSettings, PadId, PadMixSettings, PadSettings,
        PlaybackMode, ReverbSettings,
    };

    use crate::audio::{AudioPort, CaptureSupport};
    use crate::loader::{LoadPurpose, LoadedSample, WorkerRequest, WorkerResult};
    use crate::{App, EDIT_PREVIEW_COLUMNS, PreviewColumn, WorkspaceView};

    struct FakeAudio {
        telemetry: Option<Telemetry>,
        fail_mix: bool,
    }

    impl FakeAudio {
        fn ready() -> Self {
            Self {
                telemetry: None,
                fail_mix: false,
            }
        }

        fn with_telemetry(telemetry: Telemetry) -> Self {
            Self {
                telemetry: Some(telemetry),
                fail_mix: false,
            }
        }

        fn failing_mix() -> Self {
            Self {
                telemetry: None,
                fail_mix: true,
            }
        }
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
            _mix: PadMixSettings,
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
            Ok(())
        }

        fn update_pad(&mut self, _pad: PadId, _settings: PadSettings) -> Result<(), String> {
            Ok(())
        }

        fn update_pad_mix(&mut self, _pad: PadId, _settings: PadMixSettings) -> Result<(), String> {
            Ok(())
        }

        fn update_master_mix(&mut self, settings: MasterMixSettings) -> Result<(), String> {
            if self.fail_mix && settings != MasterMixSettings::default() {
                Err("mixer command queue full".to_owned())
            } else {
                Ok(())
            }
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

        fn capture_support(&self) -> CaptureSupport {
            CaptureSupport::Unsupported
        }
    }

    fn pad(index: u8) -> PadId {
        PadId::new(sampler_core::BankId::new(0).unwrap(), index).unwrap()
    }

    fn mixer_app(audio: FakeAudio) -> App {
        let mut app = App::with_audio(Box::new(audio));
        for _ in 0..3 {
            app.apply_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        app
    }

    fn render_lines(width: u16, height: u16, app: &App) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render_mixer(frame, frame.area(), app))
            .unwrap();
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

    fn render_full_lines(width: u16, height: u16, app: &App) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, app))
            .unwrap();
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

    fn display_width(value: &str) -> usize {
        Line::from(Span::raw(value)).width()
    }

    fn assert_frame_fits(lines: &[String], width: u16, height: u16) {
        assert_eq!(lines.len(), usize::from(height));
        for (row, line) in lines.iter().enumerate() {
            assert!(
                display_width(line) <= usize::from(width),
                "row {row} exceeds {width} columns: {line:?}"
            );
        }
    }

    fn load_selected_pad(app: &mut App) {
        let target = pad(0);
        let path = std::path::PathBuf::from("/samples/CHOKED-HAT.wav");
        let request = app.begin_load(target, path.clone()).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("expected sample load request")
        };
        let preview = Arc::new([PreviewColumn::default(); EDIT_PREVIEW_COLUMNS]);
        let sample = Arc::new(SampleBuffer::new(48_000, vec![0.0; 128]).unwrap());
        assert!(
            app.apply_worker_result(WorkerResult::Loaded {
                pad: target,
                generation,
                purpose: LoadPurpose::User,
                path,
                result: Ok(LoadedSample {
                    fingerprint: crate::SourceFingerprint::from_encoded_bytes(
                        std::path::Path::new("CHOKED-HAT.wav"),
                        &[],
                    )
                    .unwrap(),
                    base: Arc::clone(&sample),
                    base_preview: Arc::clone(&preview),
                    rendered: sample,
                    rendered_preview: preview,
                    recipe: sampler_core::SampleEditRecipe::identity(),
                    source_rate: 48_000,
                    source_frames: 128,
                    duration: Duration::from_secs_f64(128.0 / 48_000.0),
                }),
            })
        );
    }

    fn telemetry(peak_left: f32, peak_right: f32) -> Telemetry {
        Telemetry {
            active_pads: [0; 3],
            rendered_frame: 64,
            last_triggered_frame: None,
            peak_left,
            peak_right,
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

    #[test]
    fn exact_80x24_and_wide_default_mixer_keep_the_same_empty_pad_semantics() {
        let app = mixer_app(FakeAudio::ready());

        for (width, height) in [(80, 24), (124, 30)] {
            let lines = render_lines(width, height, &app);
            assert_frame_fits(&lines, width, height);
            let screen = lines.join("\n");
            for expected in [
                "MIXER / FX",
                "PAD 01",
                "EMPTY",
                "PAD MIX",
                "MASTER",
                "DELAY",
                "REVERB",
                "> Level",
                "Mute OFF",
                "Choke --",
            ] {
                assert!(
                    screen.contains(expected),
                    "missing {expected} at {width}x{height}"
                );
            }
        }
    }

    #[test]
    fn loaded_muted_choked_pad_renders_committed_playback_and_send_values() {
        let mut app = mixer_app(FakeAudio::ready());
        load_selected_pad(&mut app);
        app.update_pad_settings(
            pad(0),
            PadSettings::new(
                PlaybackMode::Gate,
                -6.0,
                -1.0,
                0.0,
                Some(ChokeGroup::new(3).unwrap()),
            )
            .unwrap(),
        )
        .unwrap();
        app.update_pad_mix(pad(0), PadMixSettings::new(true, 0.25, 1.0).unwrap())
            .unwrap();

        let lines = render_lines(80, 24, &app);
        assert_frame_fits(&lines, 80, 24);
        let screen = lines.join("\n");
        for expected in [
            "CHOKED-HAT",
            "READY",
            "Mode GATE",
            "Level -6.0 dB",
            "Pan L100",
            "Mute ON",
            "Choke 3",
            "Delay 25%",
            "Reverb 100%",
        ] {
            assert!(screen.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn master_meters_sanitize_non_finite_values_before_rendering() {
        let mut app = mixer_app(FakeAudio::with_telemetry(telemetry(0.75, 0.25)));
        app.tick();
        let finite = render_lines(124, 30, &app).join("\n");
        assert!(finite.contains("L 75%"));
        assert!(finite.contains("R 25%"));
        assert!(
            finite.contains("L 75% ["),
            "wide mode must add meter detail"
        );

        let mut invalid = mixer_app(FakeAudio::with_telemetry(telemetry(
            f32::NAN,
            f32::INFINITY,
        )));
        invalid.tick();
        let lines = render_lines(80, 24, &invalid);
        assert_frame_fits(&lines, 80, 24);
        let screen = lines.join("\n");
        assert!(screen.contains("L 0%"));
        assert!(screen.contains("R 0%"));
        assert!(!screen.contains("L 0% ["), "compact mode must retain room");
        assert!(!screen.contains("NaN"));
        assert!(!screen.to_ascii_lowercase().contains("inf"));
    }

    #[test]
    fn enabled_delay_and_reverb_render_valid_boundary_values() {
        let mut app = mixer_app(FakeAudio::ready());
        app.update_master_mix(
            MasterMixSettings::new(
                6.0,
                DelaySettings::new(true, 2_000, 0.95, 6.0).unwrap(),
                ReverbSettings::new(true, 1.0, 0.0, -60.0).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();

        let lines = render_lines(80, 24, &app);
        assert_frame_fits(&lines, 80, 24);
        let screen = lines.join("\n");
        for expected in [
            "Level +6.0 dB",
            "Enabled ON",
            "Time 2000 ms",
            "Feedback 95%",
            "Return +6.0 dB",
            "Room 100%",
            "Damping 0%",
            "Return -60.0 dB",
        ] {
            assert!(screen.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn active_field_focus_is_visible_and_deterministic() {
        let mut app = mixer_app(FakeAudio::ready());
        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        app.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        app.apply_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

        let screen = render_lines(80, 24, &app).join("\n");
        assert!(screen.contains("DELAY [FOCUS]"));
        assert!(screen.contains("> Time 250 ms"));
        assert!(!screen.contains("> Level 0.0 dB"));
    }

    #[test]
    fn default_mixer_status_names_committed_focus_and_uses_local_controls() {
        let app = mixer_app(FakeAudio::ready());
        let lines = render_lines(80, 24, &app);
        let footer = lines[19..].join("\n");

        assert!(footer.contains("PAD 01 · PAD MIX · Level · 0.0 dB"));
        assert!(footer.contains("Enter toggle"));
        assert!(footer.contains("? help · : cmd"));
        assert!(!footer.contains("Ready"));
        assert!(!footer.contains("Enter trigger"));
    }

    #[test]
    fn mixer_status_reads_the_committed_value_after_a_successful_change() {
        let mut app = mixer_app(FakeAudio::ready());
        app.apply_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.pad_mix(pad(0)).muted);

        let lines = render_lines(80, 24, &app);
        let footer = lines[19..].join("\n");
        assert!(footer.contains("PAD 01 · PAD MIX · Mute · ON"));
        assert!(!footer.contains("Enter trigger"));
    }

    #[test]
    fn mixer_status_audio_error_and_help_overlay_remain_visible() {
        let mut failed_edit = mixer_app(FakeAudio::failing_mix());
        failed_edit.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        failed_edit.apply_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        let status = render_lines(80, 24, &failed_edit).join("\n");
        assert!(status.contains("mixer command queue full"));
        assert!(status.contains(": cmd"));

        let mut unavailable = App::without_audio("device disconnected");
        unavailable
            .patterns_mut_for_test()
            .set_view(WorkspaceView::Mixer);
        let audio_error = render_full_lines(80, 24, &unavailable).join("\n");
        assert!(audio_error.contains("MIXER / FX"));
        assert!(audio_error.contains("AUDIO DEVICE ERROR"));
        assert!(audio_error.contains("device disconnected"));

        let mut help = mixer_app(FakeAudio::ready());
        help.open_help();
        let lines = render_full_lines(80, 24, &help);
        assert_frame_fits(&lines, 80, 24);
        let screen = lines.join("\n");
        assert!(screen.contains("Tab / Shift+Tab: cycle Perform / Pattern / Sample / Mixer"));
        assert!(screen.contains("MIXER: Ctrl+Left/Right section"));
        assert!(screen.contains("unsupported edits: use : command palette"));
    }

    #[test]
    fn mixer_below_minimum_terminal_renders_only_the_resize_message() {
        let app = mixer_app(FakeAudio::ready());
        let lines = render_full_lines(79, 23, &app);
        assert_frame_fits(&lines, 79, 23);
        let screen = lines.join("\n");
        assert!(screen.contains("Terminal too small: 79x23"));
        assert!(screen.contains("Required: 80x24"));
        assert!(!screen.contains("MIXER / FX"));
    }
}
