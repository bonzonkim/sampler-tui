pub mod app;
pub mod audio;
pub mod cli;
pub mod diagnostic;
pub mod file_picker;
pub mod input;
pub mod loader;
pub mod palette;
pub mod pattern;
pub mod sample_editor;
pub mod terminal;
pub mod ui;
mod ui_pattern;
mod ui_sample;

pub use app::{
    App, EDIT_PREVIEW_COLUMNS, Overlay, PAD_VIEW_COUNT, PREVIEW_COLUMNS, PadLoadState, PadView,
    PreviewColumn, SampleEditRequestError, SampleEditStatus,
};
pub use audio::AudioPort;
pub use file_picker::{DirectoryEntry, DirectoryEntryKind, DirectoryScan, FilePicker};
pub use input::{InputAction, KeyboardCapabilities, PAD_KEYS, map_key};
pub use loader::{
    EditPreview, LoadSampleError, LoadedSample, MAX_DECODED_BYTES, MAX_DECODED_FRAMES,
    MAX_DIRECTORY_ENTRIES, MAX_ENCODED_FILE_BYTES, MAX_PREPARED_FRAMES, RenderedSample,
    WorkerHandle, WorkerPanicked, WorkerRequest, WorkerResult, WorkerSendError, downsample_preview,
};
pub use palette::{LineEditor, PaletteCommand, parse_palette};
pub use pattern::{
    MAX_ACKS_PER_MAINTENANCE, MAX_RECORDING_KEYS, PatternCaptureState, PatternCursor,
    PatternMaintenance, PatternStatus, PatternWorkspace, WorkspaceView,
};
pub use sample_editor::{
    OffscreenDirection, SampleEditor, SampleEditorContext, SampleEditorError, SampleEditorIntent,
    SampleEditorStatus as WorkspaceSampleEditorStatus, SampleMarker, SampleProjection,
    SampleViewport,
};
pub use sampler_audio::{LiveAck, LiveCommandId, PatternSnapshotSlot, PatternSwitch};
pub use sampler_core::{PatternSlotId, PatternSnapshot};
pub use terminal::{
    CrosstermEventSource, EventSource, KeyboardEnhancementGuard, MAX_EVENTS_PER_ITERATION,
    run_event_loop, run_tui,
};
