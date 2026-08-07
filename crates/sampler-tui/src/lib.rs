pub mod app;
pub mod audio;
pub mod cli;
pub mod diagnostic;
pub mod file_picker;
pub mod input;
pub mod loader;
pub mod palette;
pub mod terminal;
pub mod ui;

pub use app::{
    App, Overlay, PAD_VIEW_COUNT, PREVIEW_COLUMNS, PadLoadState, PadView, PreviewColumn,
};
pub use audio::AudioPort;
pub use file_picker::{DirectoryEntry, DirectoryEntryKind, DirectoryScan, FilePicker};
pub use input::{InputAction, KeyboardCapabilities, PAD_KEYS, map_key};
pub use loader::{
    LoadSampleError, LoadedSample, MAX_DECODED_BYTES, MAX_DECODED_FRAMES, MAX_DIRECTORY_ENTRIES,
    MAX_ENCODED_FILE_BYTES, MAX_PREPARED_FRAMES, WorkerHandle, WorkerPanicked, WorkerRequest,
    WorkerResult, WorkerSendError,
};
pub use palette::{LineEditor, PaletteCommand, parse_palette};
pub use terminal::{
    CrosstermEventSource, EventSource, KeyboardEnhancementGuard, MAX_EVENTS_PER_ITERATION,
    run_event_loop, run_tui,
};
