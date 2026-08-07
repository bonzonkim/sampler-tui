use std::array;
use std::mem;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use sampler_audio::SampleBuffer;
use sampler_core::pad::{BANK_COUNT, PADS_PER_BANK};
use sampler_core::{BankId, PadId, PadSettings};

use crate::audio::AudioPort;
use crate::file_picker::FilePicker;
use crate::input::{InputAction, KeyboardCapabilities, map_key};
use crate::loader::{WorkerRequest, WorkerResult};
use crate::palette::{LineEditor, PaletteCommand, parse_palette};

pub const PAD_VIEW_COUNT: usize = 160;
pub const PREVIEW_COLUMNS: usize = 64;
const LIVE_SCHEDULE_AHEAD_FRAMES: u64 = 64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviewColumn {
    pub min: i8,
    pub max: i8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PadLoadState {
    Empty,
    WaitingForDevice,
    Loading,
    Ready,
    Error(String),
}

pub struct PadView {
    pub source: Option<PathBuf>,
    pub label: String,
    pub settings: PadSettings,
    pub generation: u64,
    pub state: PadLoadState,
    pub sample: Option<Arc<SampleBuffer>>,
    pub preview: [PreviewColumn; PREVIEW_COLUMNS],
}

impl Default for PadView {
    fn default() -> Self {
        Self {
            source: None,
            label: String::new(),
            settings: PadSettings::default(),
            generation: 0,
            state: PadLoadState::Empty,
            sample: None,
            preview: [PreviewColumn::default(); PREVIEW_COLUMNS],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Help,
    Palette,
    FilePicker,
    ConfirmQuit,
    DeviceError(String),
}

pub struct App {
    active_bank: BankId,
    selected_pad: usize,
    pads: [PadView; PAD_VIEW_COUNT],
    audio: Option<Box<dyn AudioPort>>,
    held_pad_by_key: [Option<PadId>; PADS_PER_BANK as usize],
    overlay: Option<Overlay>,
    palette: LineEditor,
    palette_error: Option<String>,
    current_dir: PathBuf,
    file_picker: FilePicker,
    pending_worker_requests: Vec<WorkerRequest>,
    device_retry_requests: usize,
    keyboard_capabilities: KeyboardCapabilities,
    status: String,
    audio_unavailable_message: Option<String>,
    should_quit: bool,
}

impl App {
    pub fn with_audio(audio: Box<dyn AudioPort>) -> Self {
        Self::new(Some(audio), None)
    }

    pub fn without_audio(error: impl Into<String>) -> Self {
        let error = error.into();
        Self::new(None, Some(error))
    }

    fn new(audio: Option<Box<dyn AudioPort>>, audio_error: Option<String>) -> Self {
        let overlay = audio_error.clone().map(Overlay::DeviceError);
        let current_dir = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from(std::path::MAIN_SEPARATOR_STR));
        Self {
            active_bank: BankId::new(0).expect("bank zero is valid"),
            selected_pad: 0,
            pads: array::from_fn(|_| PadView::default()),
            audio,
            held_pad_by_key: [None; PADS_PER_BANK as usize],
            overlay,
            palette: LineEditor::default(),
            palette_error: None,
            file_picker: FilePicker::new(current_dir.clone()),
            current_dir,
            pending_worker_requests: Vec::new(),
            device_retry_requests: 0,
            keyboard_capabilities: KeyboardCapabilities::default(),
            status: audio_error.clone().unwrap_or_default(),
            audio_unavailable_message: audio_error,
            should_quit: false,
        }
    }

    pub fn apply(&mut self, action: InputAction) {
        match action {
            InputAction::PadPress(index) => self.press_pad(index),
            InputAction::PadRelease(index) => self.release_pad(index),
            InputAction::PadStop(index) => self.stop_pad(index),
            InputAction::BankDelta(delta) => self.change_bank(delta),
            InputAction::StopAll => self.stop_all(),
            InputAction::Quit => self.should_quit = true,
        }
    }

    pub fn apply_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.apply_key(key),
            Event::Paste(text) if self.overlay == Some(Overlay::Palette) && !text.is_empty() => {
                self.palette.insert_str(&text);
                self.palette_error = None;
            }
            _ => {}
        }
    }

    pub fn apply_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Repeat {
            return;
        }
        if key.kind == KeyEventKind::Press
            && key.code == KeyCode::Esc
            && key.modifiers == KeyModifiers::NONE
            && self.overlay.is_some()
        {
            self.close_overlay();
            return;
        }

        match self.overlay.as_ref() {
            Some(Overlay::DeviceError(_)) => self.apply_device_error_key(key),
            Some(Overlay::ConfirmQuit) => self.apply_confirmation_key(key),
            Some(Overlay::Palette) => self.apply_palette_key(key),
            Some(Overlay::FilePicker) => self.apply_picker_key(key),
            Some(Overlay::Help) => self.apply_help_key(key),
            None => self.apply_perform_key(key),
        }
    }

    pub fn active_bank(&self) -> BankId {
        self.active_bank
    }

    pub fn selected_pad(&self) -> usize {
        self.selected_pad
    }

    pub fn pads(&self) -> &[PadView; PAD_VIEW_COUNT] {
        &self.pads
    }

    pub fn pad(&self, pad: PadId) -> &PadView {
        &self.pads[pad_offset(pad)]
    }

    pub fn overlay(&self) -> Option<&Overlay> {
        self.overlay.as_ref()
    }

    pub fn open_help(&mut self) {
        self.overlay = Some(Overlay::Help);
    }

    pub fn open_palette(&mut self) {
        self.palette.clear();
        self.palette_error = None;
        self.overlay = Some(Overlay::Palette);
    }

    pub fn open_picker(&mut self) {
        let source_parent = self
            .selected_pad_id()
            .and_then(|pad| self.pad(pad).source.as_deref())
            .and_then(|path| path.parent())
            .filter(|path| !path.as_os_str().is_empty())
            .map(ToOwned::to_owned);
        let directory = source_parent.unwrap_or_else(|| self.current_dir.clone());
        self.open_picker_at(directory);
    }

    pub fn open_picker_at(&mut self, directory: impl Into<PathBuf>) {
        let directory = resolve_picker_directory(&self.current_dir, directory.into());
        let request_id = self.file_picker.begin_scan(directory.clone());
        self.pending_worker_requests
            .push(WorkerRequest::ScanDirectory {
                request_id,
                path: directory,
                show_hidden: self.file_picker.show_hidden(),
            });
        self.overlay = Some(Overlay::FilePicker);
    }

    pub fn open_quit_confirmation(&mut self) {
        self.overlay = Some(Overlay::ConfirmQuit);
    }

    pub fn close_overlay(&mut self) {
        if self.overlay == Some(Overlay::Palette) {
            self.palette_error = None;
        }
        self.overlay = None;
    }

    pub fn palette_text(&self) -> &str {
        self.palette.text()
    }

    pub fn palette_cursor(&self) -> usize {
        self.palette.cursor()
    }

    pub fn palette_error(&self) -> Option<&str> {
        self.palette_error.as_deref()
    }

    pub fn file_picker(&self) -> &FilePicker {
        &self.file_picker
    }

    pub fn take_worker_requests(&mut self) -> Vec<WorkerRequest> {
        mem::take(&mut self.pending_worker_requests)
    }

    pub fn device_retry_requests(&self) -> usize {
        self.device_retry_requests
    }

    pub fn take_device_retry_requests(&mut self) -> usize {
        mem::take(&mut self.device_retry_requests)
    }

    pub fn set_keyboard_capabilities(&mut self, capabilities: KeyboardCapabilities) {
        self.keyboard_capabilities = capabilities;
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn begin_load(&mut self, pad: PadId, path: impl Into<PathBuf>) -> Option<WorkerRequest> {
        let path = path.into();
        let engine_rate = self.audio.as_ref().map(|audio| audio.sample_rate());
        let view = &mut self.pads[pad_offset(pad)];
        view.generation = view.generation.wrapping_add(1);
        view.source = Some(path.clone());
        view.state = if engine_rate.is_some() {
            PadLoadState::Loading
        } else {
            PadLoadState::WaitingForDevice
        };

        engine_rate.map(|engine_rate| WorkerRequest::LoadSample {
            pad,
            generation: view.generation,
            path,
            engine_rate,
        })
    }

    pub fn apply_worker_result(&mut self, result: WorkerResult) -> bool {
        let WorkerResult::Loaded {
            pad,
            generation,
            path,
            result,
        } = result
        else {
            let WorkerResult::Scanned {
                request_id,
                path,
                result,
            } = result
            else {
                unreachable!()
            };
            if path != self.file_picker.directory() {
                return false;
            }
            let error = result.as_ref().err().cloned();
            let applied = self.file_picker.apply_scan(request_id, result);
            if applied && let Some(error) = error {
                self.status = error;
            }
            return applied;
        };
        let offset = pad_offset(pad);
        if self.pads[offset].generation != generation
            || self.pads[offset].source.as_deref() != Some(path.as_path())
        {
            return false;
        }

        let loaded = match result {
            Ok(loaded) => loaded,
            Err(error) => {
                self.pads[offset].state = PadLoadState::Error(error.clone());
                self.status = error;
                return true;
            }
        };
        let Some(audio) = self.audio.as_mut() else {
            self.pads[offset].state = PadLoadState::WaitingForDevice;
            return true;
        };
        let settings = self.pads[offset].settings;
        if let Err(error) = audio.install(pad, Arc::clone(&loaded.buffer), settings) {
            self.pads[offset].state = PadLoadState::Error(error.clone());
            self.status = error;
            return true;
        }

        let view = &mut self.pads[offset];
        view.label = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        view.source = Some(path);
        view.sample = Some(loaded.buffer);
        view.preview = loaded.preview;
        view.state = PadLoadState::Ready;
        true
    }

    fn press_pad(&mut self, index: usize) {
        if self.held_pad_by_key.get(index).is_some_and(Option::is_some) {
            return;
        }
        let Some(pad) = self.pad_in_active_bank(index) else {
            return;
        };
        self.selected_pad = index;
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        let at = audio
            .render_horizon()
            .saturating_add(LIVE_SCHEDULE_AHEAD_FRAMES);
        match audio.trigger(pad, at, 1.0) {
            Ok(()) => self.held_pad_by_key[index] = Some(pad),
            Err(error) => self.status = error,
        }
    }

    fn apply_device_error_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press
            && key.modifiers == KeyModifiers::NONE
            && matches!(key.code, KeyCode::Char('r' | 'R'))
        {
            self.device_retry_requests = self.device_retry_requests.saturating_add(1);
        }
    }

    fn apply_confirmation_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                self.should_quit = true;
                self.overlay = None;
            }
            KeyCode::Char('n' | 'N') => self.overlay = None,
            _ => {}
        }
    }

    fn apply_palette_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        let text_changed = match key.code {
            KeyCode::Enter => {
                self.execute_palette();
                false
            }
            KeyCode::Left => {
                self.palette.move_left();
                false
            }
            KeyCode::Right => {
                self.palette.move_right();
                false
            }
            KeyCode::Home => {
                self.palette.move_home();
                false
            }
            KeyCode::End => {
                self.palette.move_end();
                false
            }
            KeyCode::Backspace => {
                let prior_len = self.palette.text().len();
                self.palette.backspace();
                self.palette.text().len() != prior_len
            }
            KeyCode::Delete => {
                let prior_len = self.palette.text().len();
                self.palette.delete();
                self.palette.text().len() != prior_len
            }
            KeyCode::Char(character)
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.palette.insert(character);
                true
            }
            _ => return,
        };
        if text_changed {
            self.palette_error = None;
        }
    }

    fn execute_palette(&mut self) {
        let command = match parse_palette(self.palette.text()) {
            Ok(command) => command,
            Err(error) => {
                self.palette_error = Some(error);
                return;
            }
        };
        self.palette_error = None;
        match command {
            PaletteCommand::OpenPicker => self.open_picker(),
            PaletteCommand::LoadPath(path) => {
                self.begin_selected_load(path);
                self.overlay = None;
            }
            PaletteCommand::Bank(bank) => {
                self.active_bank = bank;
                self.overlay = None;
            }
            PaletteCommand::Select(index) => {
                self.selected_pad = index;
                self.overlay = None;
            }
            PaletteCommand::StopAll => {
                self.stop_all();
                self.overlay = None;
            }
            PaletteCommand::Help => self.open_help(),
            PaletteCommand::Quit => {
                self.should_quit = true;
                self.overlay = None;
            }
        }
    }

    fn apply_picker_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Up => self.file_picker.move_cursor(-1),
            KeyCode::Down => self.file_picker.move_cursor(1),
            KeyCode::Home => self.file_picker.select_first(),
            KeyCode::End => self.file_picker.select_last(),
            KeyCode::Backspace => self.open_picker_parent(),
            KeyCode::Char('.') if key.modifiers == KeyModifiers::NONE => {
                let request_id = self.file_picker.toggle_hidden();
                self.queue_current_picker_scan(request_id);
            }
            KeyCode::Enter => self.open_picker_selection(),
            _ => {}
        }
    }

    fn apply_help_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press
            && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
            && key.code == KeyCode::Char('?')
        {
            self.overlay = None;
        }
    }

    fn apply_perform_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Press {
            match key.code {
                KeyCode::Char('?')
                    if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
                {
                    self.open_help();
                }
                KeyCode::Char(':')
                    if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
                {
                    self.open_palette();
                }
                KeyCode::Char('l') if key.modifiers == KeyModifiers::NONE => {
                    self.open_picker();
                }
                KeyCode::Left => self.move_selection(-1, 0),
                KeyCode::Right => self.move_selection(1, 0),
                KeyCode::Up => self.move_selection(0, -1),
                KeyCode::Down => self.move_selection(0, 1),
                _ => {
                    if let Some(action) = map_key(key, self.keyboard_capabilities) {
                        self.apply(action);
                    }
                }
            }
        } else if let Some(action) = map_key(key, self.keyboard_capabilities) {
            self.apply(action);
        }
    }

    fn move_selection(&mut self, horizontal: isize, vertical: isize) {
        let row = self.selected_pad / 4;
        let column = self.selected_pad % 4;
        let row = row.saturating_add_signed(vertical).min(3);
        let column = column.saturating_add_signed(horizontal).min(3);
        self.selected_pad = row * 4 + column;
    }

    fn selected_pad_id(&self) -> Option<PadId> {
        let index = u8::try_from(self.selected_pad).ok()?;
        PadId::new(self.active_bank, index).ok()
    }

    fn begin_selected_load(&mut self, path: PathBuf) {
        let Some(pad) = self.selected_pad_id() else {
            return;
        };
        if let Some(request) = self.begin_load(pad, path) {
            self.pending_worker_requests.push(request);
        }
    }

    fn open_picker_parent(&mut self) {
        let Some(parent) = self.file_picker.directory().parent().map(ToOwned::to_owned) else {
            self.status = "already at filesystem root".to_owned();
            return;
        };
        self.open_picker_at(parent);
    }

    fn open_picker_selection(&mut self) {
        let Some(entry) = self.file_picker.selected().cloned() else {
            return;
        };
        if entry.is_directory() {
            self.open_picker_at(entry.path);
        } else if entry.is_selectable_file() {
            self.begin_selected_load(entry.path);
            self.overlay = None;
        } else {
            self.status = "entry is not a supported audio file".to_owned();
        }
    }

    fn queue_current_picker_scan(&mut self, request_id: u64) {
        self.pending_worker_requests
            .push(WorkerRequest::ScanDirectory {
                request_id,
                path: self.file_picker.directory().to_owned(),
                show_hidden: self.file_picker.show_hidden(),
            });
    }

    fn release_pad(&mut self, index: usize) {
        if !self.validate_pad_index(index) {
            return;
        }
        let Some(pad) = self.held_pad_by_key[index] else {
            return;
        };
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        let at = audio
            .render_horizon()
            .saturating_add(LIVE_SCHEDULE_AHEAD_FRAMES);
        match audio.release(pad, at) {
            Ok(()) => self.held_pad_by_key[index] = None,
            Err(error) => self.status = error,
        }
    }

    fn stop_pad(&mut self, index: usize) {
        let Some(pad) = self.pad_in_active_bank(index) else {
            return;
        };
        self.selected_pad = index;
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        match audio.stop_pad(pad) {
            Ok(()) if self.held_pad_by_key[index] == Some(pad) => {
                self.held_pad_by_key[index] = None;
            }
            Ok(()) => {}
            Err(error) => self.status = error,
        }
    }

    fn stop_all(&mut self) {
        let Some(audio) = self.audio.as_mut() else {
            self.report_audio_unavailable();
            return;
        };
        match audio.stop_all() {
            Ok(()) => self.held_pad_by_key.fill(None),
            Err(error) => self.status = error,
        }
    }

    fn change_bank(&mut self, delta: i8) {
        let current = i16::from(u8::from(self.active_bank));
        let requested = current + i16::from(delta);
        if requested < 0 {
            self.status = "already at first bank (A)".to_owned();
            return;
        }
        if requested >= i16::from(BANK_COUNT) {
            self.status = "already at last bank (J)".to_owned();
            return;
        }
        let value = u8::try_from(requested).expect("bounded bank fits in u8");
        self.active_bank = BankId::new(value).expect("bounded bank is valid");
    }

    fn pad_in_active_bank(&mut self, index: usize) -> Option<PadId> {
        if !self.validate_pad_index(index) {
            return None;
        }
        let index = u8::try_from(index).expect("validated pad index fits in u8");
        Some(PadId::new(self.active_bank, index).expect("validated pad index is valid"))
    }

    fn validate_pad_index(&mut self, index: usize) -> bool {
        if index < usize::from(PADS_PER_BANK) {
            true
        } else {
            self.status = format!("pad {index} is outside 0..16");
            false
        }
    }

    fn report_audio_unavailable(&mut self) {
        self.status = self
            .audio_unavailable_message
            .clone()
            .unwrap_or_else(|| "audio device is unavailable".to_owned());
    }
}

fn resolve_picker_directory(current_dir: &Path, directory: PathBuf) -> PathBuf {
    let absolute = if directory.as_os_str().is_empty() {
        current_dir.to_owned()
    } else if directory.is_absolute() {
        directory
    } else {
        current_dir.join(directory)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    normalized.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    normalized.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn pad_offset(pad: PadId) -> usize {
    usize::from(u8::from(pad.bank())) * usize::from(PADS_PER_BANK) + usize::from(pad.index())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use sampler_audio::{Frame, SampleBuffer, SampleSlot, Telemetry};
    use sampler_core::{BankId, PadId, PadSettings};

    use crate::audio::AudioPort;
    use crate::input::InputAction;

    use crate::loader::{LoadedSample, WorkerRequest, WorkerResult};

    use super::{App, PadLoadState, PreviewColumn};

    #[derive(Debug, Clone, PartialEq)]
    enum AudioCall {
        Install(PadId),
        Trigger(PadId, Frame, f32),
        Release(PadId, Frame),
        StopPad(PadId),
        StopAll,
    }

    #[derive(Clone)]
    struct CallLog(Rc<RefCell<Vec<AudioCall>>>);

    impl CallLog {
        fn snapshot(&self) -> Vec<AudioCall> {
            self.0.borrow().clone()
        }
    }

    struct FakeAudio {
        sample_rate: u32,
        channels: u16,
        horizon: Frame,
        trigger_error: Option<String>,
        release_error: Option<String>,
        stop_pad_error: Option<String>,
        stop_all_error: Option<String>,
        install_error: Option<String>,
        calls: CallLog,
    }

    impl FakeAudio {
        fn ready(sample_rate: u32, channels: u16) -> Self {
            Self {
                sample_rate,
                channels,
                horizon: 0,
                trigger_error: None,
                release_error: None,
                stop_pad_error: None,
                stop_all_error: None,
                install_error: None,
                calls: CallLog(Rc::new(RefCell::new(Vec::new()))),
            }
        }

        fn with_horizon(mut self, horizon: Frame) -> Self {
            self.horizon = horizon;
            self
        }

        fn failing_trigger(mut self, error: &str) -> Self {
            self.trigger_error = Some(error.to_owned());
            self
        }

        fn failing_release_once(mut self, error: &str) -> Self {
            self.release_error = Some(error.to_owned());
            self
        }

        fn failing_stop_pad_once(mut self, error: &str) -> Self {
            self.stop_pad_error = Some(error.to_owned());
            self
        }

        fn failing_stop_all_once(mut self, error: &str) -> Self {
            self.stop_all_error = Some(error.to_owned());
            self
        }

        fn call_log(&self) -> CallLog {
            self.calls.clone()
        }

        fn failing_install(mut self, error: &str) -> Self {
            self.install_error = Some(error.to_owned());
            self
        }
    }

    impl AudioPort for FakeAudio {
        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        fn channels(&self) -> u16 {
            self.channels
        }

        fn render_horizon(&self) -> Frame {
            self.horizon
        }

        fn install(
            &mut self,
            pad: PadId,
            _sample: Arc<SampleBuffer>,
            _settings: PadSettings,
        ) -> Result<SampleSlot, String> {
            if let Some(error) = self.install_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::Install(pad));
            SampleSlot::new(0).map_err(|error| error.to_string())
        }

        fn trigger(&mut self, pad: PadId, at: Frame, velocity: f32) -> Result<(), String> {
            if let Some(error) = &self.trigger_error {
                return Err(error.clone());
            }
            self.calls
                .0
                .borrow_mut()
                .push(AudioCall::Trigger(pad, at, velocity));
            Ok(())
        }

        fn release(&mut self, pad: PadId, at: Frame) -> Result<(), String> {
            if let Some(error) = self.release_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::Release(pad, at));
            Ok(())
        }

        fn stop_pad(&mut self, pad: PadId) -> Result<(), String> {
            if let Some(error) = self.stop_pad_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::StopPad(pad));
            Ok(())
        }

        fn stop_all(&mut self) -> Result<(), String> {
            if let Some(error) = self.stop_all_error.take() {
                return Err(error);
            }
            self.calls.0.borrow_mut().push(AudioCall::StopAll);
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
    }

    fn pad(bank: u8, index: u8) -> PadId {
        PadId::new(BankId::new(bank).unwrap(), index).unwrap()
    }

    fn path(value: &str) -> &std::path::Path {
        std::path::Path::new(value)
    }

    fn loaded(pad: PadId, generation: u64, source: &str) -> WorkerResult {
        let buffer = Arc::new(SampleBuffer::new(48_000, vec![0.25, -0.25]).unwrap());
        WorkerResult::Loaded {
            pad,
            generation,
            path: source.into(),
            result: Ok(LoadedSample {
                buffer,
                source_rate: 48_000,
                source_frames: 1,
                duration: std::time::Duration::from_nanos(20_833),
                preview: [PreviewColumn { min: -2, max: 2 }; 64],
            }),
        }
    }

    #[test]
    fn app_discards_a_superseded_load_generation() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.begin_load(pad(0, 0), path("old.wav"));
        let old_generation = app.pad(pad(0, 0)).generation;
        app.begin_load(pad(0, 0), path("new.wav"));

        app.apply_worker_result(loaded(pad(0, 0), old_generation, "old.wav"));

        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("new.wav")));
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Loading);
    }

    #[test]
    fn matching_load_is_installed_before_replacing_the_pad_sample() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        let request = app.begin_load(pad(0, 0), path("new.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };

        app.apply_worker_result(loaded(pad(0, 0), generation, "new.wav"));

        assert_eq!(calls.snapshot(), [AudioCall::Install(pad(0, 0))]);
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::Ready);
        assert!(app.pad(pad(0, 0)).sample.is_some());
    }

    #[test]
    fn install_failure_preserves_the_prior_ready_sample() {
        let fake = FakeAudio::ready(48_000, 2).failing_install("install queue is full");
        let mut app = App::with_audio(Box::new(fake));
        let first = Arc::new(SampleBuffer::new(48_000, vec![0.0, 0.0]).unwrap());
        app.pads[0].sample = Some(Arc::clone(&first));
        app.pads[0].state = PadLoadState::Ready;
        let request = app.begin_load(pad(0, 0), path("new.wav")).unwrap();
        let WorkerRequest::LoadSample { generation, .. } = request else {
            panic!("wrong request")
        };

        app.apply_worker_result(loaded(pad(0, 0), generation, "new.wav"));

        assert!(Arc::ptr_eq(
            app.pad(pad(0, 0)).sample.as_ref().unwrap(),
            &first
        ));
        assert!(matches!(app.pad(pad(0, 0)).state, PadLoadState::Error(_)));
    }

    #[test]
    fn no_device_retains_the_path_without_creating_a_load_request() {
        let mut app = App::without_audio("no output device");

        let request = app.begin_load(pad(0, 0), path("kick.wav"));

        assert!(request.is_none());
        assert_eq!(app.pad(pad(0, 0)).source.as_deref(), Some(path("kick.wav")));
        assert_eq!(app.pad(pad(0, 0)).state, PadLoadState::WaitingForDevice);
    }

    #[test]
    fn pad_press_uses_render_horizon_plus_sixty_four_frames() {
        let fake = FakeAudio::ready(48_000, 2).with_horizon(10_000);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(5));

        assert_eq!(
            calls.snapshot(),
            [AudioCall::Trigger(pad(0, 5), 10_064, 1.0)]
        );
    }

    #[test]
    fn bank_navigation_is_bounded_and_release_targets_the_original_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::BankDelta(1));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(app.active_bank(), BankId::new(1).unwrap());
        assert_eq!(
            calls.snapshot().last(),
            Some(&AudioCall::Release(pad(0, 0), 64))
        );
    }

    #[test]
    fn duplicate_press_does_not_retrigger_or_replace_the_held_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::BankDelta(1));
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::Release(pad(0, 0), 64),
            ]
        );
    }

    #[test]
    fn failed_release_keeps_the_held_pad_for_retry() {
        let fake = FakeAudio::ready(48_000, 2).failing_release_once("release queue is full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadRelease(0));
        assert!(app.status().contains("release queue is full"));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::Release(pad(0, 0), 64),
            ]
        );
    }

    #[test]
    fn failed_stop_pad_keeps_the_slot_held_until_stop_retry_succeeds() {
        let fake = FakeAudio::ready(48_000, 2).failing_stop_pad_once("stop queue is full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadStop(0));
        assert!(app.status().contains("stop queue is full"));
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::PadStop(0));
        app.apply(InputAction::PadPress(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::StopPad(pad(0, 0)),
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
    }

    #[test]
    fn bank_switched_stop_does_not_forget_the_original_held_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::BankDelta(1));
        app.apply(InputAction::PadStop(0));
        app.apply(InputAction::PadRelease(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::StopPad(pad(1, 0)),
                AudioCall::Release(pad(0, 0), 64),
            ]
        );
    }

    #[test]
    fn failed_stop_all_keeps_slots_held_until_stop_retry_succeeds() {
        let fake = FakeAudio::ready(48_000, 2).failing_stop_all_once("stop-all queue is full");
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::StopAll);
        assert!(app.status().contains("stop-all queue is full"));
        app.apply(InputAction::PadPress(0));
        app.apply(InputAction::StopAll);
        app.apply(InputAction::PadPress(0));

        assert_eq!(
            calls.snapshot(),
            [
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
                AudioCall::StopAll,
                AudioCall::Trigger(pad(0, 0), 64, 1.0),
            ]
        );
    }

    #[test]
    fn controller_overflow_is_visible_and_nonfatal() {
        let fake = FakeAudio::ready(48_000, 2).failing_trigger("audio command queue is full");
        let mut app = App::with_audio(Box::new(fake));
        app.apply(InputAction::PadPress(0));
        assert!(app.status().contains("queue is full"));
        assert!(!app.should_quit());
    }

    #[test]
    fn scheduling_saturates_at_the_frame_limit() {
        let fake = FakeAudio::ready(48_000, 2).with_horizon(Frame::MAX);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(15));

        assert_eq!(
            calls.snapshot(),
            [AudioCall::Trigger(pad(0, 15), Frame::MAX, 1.0)]
        );
    }

    #[test]
    fn bank_navigation_does_not_wrap_and_reports_both_edges() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.apply(InputAction::BankDelta(-1));
        assert_eq!(app.active_bank(), BankId::new(0).unwrap());
        assert!(app.status().contains("first bank"));

        app.apply(InputAction::BankDelta(9));
        assert_eq!(app.active_bank(), BankId::new(9).unwrap());
        app.apply(InputAction::BankDelta(1));
        assert_eq!(app.active_bank(), BankId::new(9).unwrap());
        assert!(app.status().contains("last bank"));
        assert!(!app.should_quit());
    }

    #[test]
    fn invalid_pad_positions_are_visible_and_nonfatal() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply(InputAction::PadPress(16));
        app.apply(InputAction::PadRelease(usize::MAX));

        assert!(calls.snapshot().is_empty());
        assert!(app.status().contains("outside 0..16"));
        assert!(!app.should_quit());
    }

    #[test]
    fn missing_audio_keeps_a_complete_browsable_model() {
        let mut app = App::without_audio("no output device");

        assert_eq!(app.active_bank(), BankId::new(0).unwrap());
        assert_eq!(app.pads().len(), super::PAD_VIEW_COUNT);
        assert_eq!(
            app.overlay(),
            Some(&super::Overlay::DeviceError("no output device".to_owned()))
        );
        app.apply(InputAction::PadPress(0));
        assert!(app.status().contains("no output device"));
        assert!(!app.should_quit());
    }

    fn key(character: char, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Char(character), modifiers, kind)
    }

    #[test]
    fn device_modal_retry_wins_over_the_r_pad_key() {
        let mut app = App::without_audio("no output device");

        app.apply_key(key('r', KeyModifiers::NONE, KeyEventKind::Press));

        assert_eq!(app.device_retry_requests(), 1);
        assert_eq!(app.selected_pad(), 0);
    }

    #[test]
    fn pasted_text_only_changes_the_open_palette() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply_terminal_event(Event::Paste("stop-all".into()));
        assert!(calls.snapshot().is_empty());
        app.open_palette();
        app.apply_terminal_event(Event::Paste("stop-all".into()));

        assert_eq!(app.palette_text(), "stop-all");
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn help_and_picker_keys_do_not_fall_through_to_pads() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.open_help();
        app.apply_key(key('q', KeyModifiers::NONE, KeyEventKind::Press));
        app.close_overlay();
        app.open_picker();
        app.apply_key(key('q', KeyModifiers::NONE, KeyEventKind::Press));

        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn invalid_palette_command_stays_open_with_inline_error() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("select 0".into()));

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));

        assert_eq!(app.overlay(), Some(&super::Overlay::Palette));
        assert_eq!(app.palette_error(), Some("select expects 1..=16"));
    }

    #[test]
    fn palette_error_survives_multibyte_and_no_op_cursor_navigation() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("wat한".into()));
        let press = |code| KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Press);
        app.apply_key(press(KeyCode::Enter));
        let error = Some("unknown command: wat한");
        assert_eq!(app.palette_error(), error);

        app.apply_key(press(KeyCode::Left));
        assert_eq!(app.palette_cursor(), 3);
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::Right));
        assert_eq!(app.palette_cursor(), 6);
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::End));
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::Home));
        app.apply_key(press(KeyCode::Home));
        app.apply_key(press(KeyCode::Left));
        app.apply_key(press(KeyCode::Backspace));
        assert_eq!(app.palette_cursor(), 0);
        assert_eq!(app.palette_error(), error);
        app.apply_key(press(KeyCode::End));
        app.apply_key(press(KeyCode::Delete));
        app.apply_terminal_event(Event::Paste(String::new()));
        assert_eq!(app.palette_cursor(), 6);
        assert_eq!(app.palette_error(), error);

        app.apply_key(key('x', KeyModifiers::NONE, KeyEventKind::Press));
        assert_eq!(app.palette_error(), None);
    }

    #[test]
    fn closing_the_palette_clears_its_inline_error() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_palette();
        app.apply_terminal_event(Event::Paste("wat".into()));
        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        assert_eq!(app.palette_error(), Some("unknown command: wat"));

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));

        assert_eq!(app.overlay(), None);
        assert_eq!(app.palette_error(), None);
    }

    #[test]
    fn shifted_question_mark_opens_help_without_triggering_a_pad() {
        let fake = FakeAudio::ready(48_000, 2);
        let calls = fake.call_log();
        let mut app = App::with_audio(Box::new(fake));

        app.apply_key(key('?', KeyModifiers::SHIFT, KeyEventKind::Press));

        assert_eq!(app.overlay(), Some(&super::Overlay::Help));
        assert!(calls.snapshot().is_empty());
    }

    #[test]
    fn picker_for_a_relative_filename_starts_in_the_current_directory() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.begin_load(pad(0, 0), path("kick.wav"));

        app.open_picker();

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one scan request")
        };
        assert_eq!(path, &std::env::current_dir().unwrap());
    }

    #[test]
    fn picker_resolves_a_nested_relative_source_and_backs_up_to_current_directory() {
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.begin_load(pad(0, 0), path("samples/kick.wav"));

        app.open_picker();

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one nested scan request")
        };
        assert!(path.is_absolute());
        assert_eq!(path, &current_dir.join("samples"));

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one parent scan request")
        };
        assert_eq!(path, &current_dir);
    }

    #[test]
    fn empty_relative_picker_directory_maps_to_current_directory_before_parent_navigation() {
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.open_picker_at("");

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one normalized scan request")
        };
        assert_eq!(path, &current_dir);

        app.apply_key(KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ));
        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one normalized parent scan request")
        };
        assert_eq!(path, current_dir.parent().unwrap());
        assert!(!path.as_os_str().is_empty());
    }

    #[test]
    fn relative_picker_directory_is_lexically_normalized() {
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.open_picker_at("samples/../drums/.");

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one normalized scan request")
        };
        assert_eq!(path, &current_dir.join("drums"));
    }

    #[cfg(unix)]
    #[test]
    fn relative_picker_normalization_preserves_non_unicode_components() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        let relative = PathBuf::from(OsString::from_vec(vec![b's', 0x80, b'm', b'p']));
        let current_dir = std::env::current_dir().unwrap();
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));

        app.open_picker_at(relative.clone());

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one lossless scan request")
        };
        assert_eq!(path, &current_dir.join(relative));
    }

    #[test]
    fn picker_without_a_source_reopens_at_the_current_directory() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/other");
        app.close_overlay();
        app.take_worker_requests();

        app.open_picker();

        let requests = app.take_worker_requests();
        let [WorkerRequest::ScanDirectory { path, .. }] = requests.as_slice() else {
            panic!("expected one scan request")
        };
        assert_eq!(path, &std::env::current_dir().unwrap());
    }

    #[test]
    fn stale_picker_error_for_the_same_directory_is_silent() {
        let mut app = App::with_audio(Box::new(FakeAudio::ready(48_000, 2)));
        app.open_picker_at("/samples");
        let requests = app.take_worker_requests();
        let [
            WorkerRequest::ScanDirectory {
                request_id: stale_id,
                ..
            },
        ] = requests.as_slice()
        else {
            panic!("expected one stale scan request")
        };
        let stale_id = *stale_id;
        app.open_picker_at("/samples");
        app.take_worker_requests();

        let applied = app.apply_worker_result(WorkerResult::Scanned {
            request_id: stale_id,
            path: "/samples".into(),
            result: Err("stale failure".to_owned()),
        });

        assert!(!applied);
        assert_eq!(app.status(), "");
    }
}
