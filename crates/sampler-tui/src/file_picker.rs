use std::cmp::Ordering;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryEntryKind {
    Directory,
    File,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub path: PathBuf,
    pub kind: DirectoryEntryKind,
}

impl DirectoryEntry {
    pub fn display_name(&self) -> String {
        self.path
            .file_name()
            .unwrap_or(self.path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    pub fn is_directory(&self) -> bool {
        self.kind == DirectoryEntryKind::Directory
    }

    pub fn is_selectable_file(&self) -> bool {
        self.kind != DirectoryEntryKind::Directory && supported_audio_path(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePicker {
    directory: PathBuf,
    pending_directory: Option<PathBuf>,
    failed_directory: Option<PathBuf>,
    show_hidden: bool,
    entries: Vec<DirectoryEntry>,
    cursor: usize,
    request_id: u64,
    error: Option<String>,
}

impl FilePicker {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            directory: path.into(),
            pending_directory: None,
            failed_directory: None,
            show_hidden: false,
            entries: Vec::new(),
            cursor: 0,
            request_id: 0,
            error: None,
        }
    }

    pub fn from_scan(
        path: impl Into<PathBuf>,
        show_hidden: bool,
        entries: Vec<DirectoryEntry>,
    ) -> Self {
        let mut picker = Self::new(path);
        picker.show_hidden = show_hidden;
        picker.replace_entries(entries);
        picker
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn pending_directory(&self) -> Option<&Path> {
        self.pending_directory.as_deref()
    }

    pub fn failed_directory(&self) -> Option<&Path> {
        self.failed_directory.as_deref()
    }

    pub fn is_scanning(&self) -> bool {
        self.pending_directory.is_some()
    }

    pub fn show_hidden(&self) -> bool {
        self.show_hidden
    }

    pub fn entries(&self) -> &[DirectoryEntry] {
        &self.entries
    }

    pub fn visible_names(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(DirectoryEntry::display_name)
            .collect()
    }

    pub fn selected(&self) -> Option<&DirectoryEntry> {
        self.entries.get(self.cursor)
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn begin_scan(&mut self, path: impl Into<PathBuf>) -> u64 {
        self.pending_directory = Some(path.into());
        self.failed_directory = None;
        self.request_id = self.request_id.wrapping_add(1);
        self.error = None;
        self.request_id
    }

    pub fn toggle_hidden(&mut self) -> u64 {
        self.show_hidden = !self.show_hidden;
        let target = self
            .pending_directory
            .clone()
            .unwrap_or_else(|| self.directory.clone());
        self.begin_scan(target)
    }

    pub fn apply_scan(
        &mut self,
        request_id: u64,
        result: Result<Vec<DirectoryEntry>, String>,
    ) -> bool {
        if request_id != self.request_id {
            return false;
        }
        let Some(target) = self.pending_directory.take() else {
            return false;
        };
        match result {
            Ok(entries) => {
                self.directory = target;
                self.cursor = 0;
                self.replace_entries(entries);
                self.error = None;
                self.failed_directory = None;
            }
            Err(error) => {
                self.failed_directory = Some(target);
                self.error = Some(error);
            }
        }
        true
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let last = self.entries.len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add_signed(delta).min(last);
    }

    pub fn select_first(&mut self) {
        self.cursor = 0;
    }

    pub fn select_last(&mut self) {
        self.cursor = self.entries.len().saturating_sub(1);
    }

    fn replace_entries(&mut self, entries: Vec<DirectoryEntry>) {
        self.entries = entries
            .into_iter()
            .filter(|entry| {
                (self.show_hidden || !is_hidden(&entry.path))
                    && match entry.kind {
                        DirectoryEntryKind::Directory | DirectoryEntryKind::Symlink => true,
                        DirectoryEntryKind::File => supported_audio_path(&entry.path),
                    }
            })
            .collect();
        self.entries.sort_by(compare_entries);
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
    }
}

fn supported_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["wav", "aiff", "aif", "flac", "mp3"]
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        })
}

fn compare_entries(left: &DirectoryEntry, right: &DirectoryEntry) -> Ordering {
    entry_group(left)
        .cmp(&entry_group(right))
        .then_with(|| {
            left.display_name()
                .to_lowercase()
                .cmp(&right.display_name().to_lowercase())
        })
        .then_with(|| left.display_name().cmp(&right.display_name()))
        .then_with(|| left.path.cmp(&right.path))
}

fn entry_group(entry: &DirectoryEntry) -> u8 {
    match entry.kind {
        DirectoryEntryKind::Directory => 0,
        DirectoryEntryKind::File => 1,
        DirectoryEntryKind::Symlink if entry.is_selectable_file() => 1,
        DirectoryEntryKind::Symlink => 2,
    }
}

fn is_hidden(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().first() == Some(&b'.')
    }
    #[cfg(not(unix))]
    {
        name.to_string_lossy().starts_with('.')
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{DirectoryEntry, DirectoryEntryKind, FilePicker};

    use DirectoryEntryKind::{Directory, File};

    fn path(value: &str) -> &Path {
        Path::new(value)
    }

    fn entry(name: &str, kind: DirectoryEntryKind) -> DirectoryEntry {
        DirectoryEntry {
            path: PathBuf::from("/samples").join(name),
            kind,
        }
    }

    #[test]
    fn scan_filters_supported_files_and_sorts_directories_first() {
        let entries = vec![
            entry("z.mp3", File),
            entry("notes.txt", File),
            entry("beats", Directory),
            entry("A.WAV", File),
        ];
        let picker = FilePicker::from_scan(path("/samples"), false, entries);

        assert_eq!(picker.visible_names(), ["beats", "A.WAV", "z.mp3"]);
    }

    #[test]
    fn stale_scan_result_does_not_replace_the_current_directory() {
        let mut picker = FilePicker::new(path("/one"));
        let old = picker.request_id();
        picker.begin_scan(path("/two"));

        assert!(!picker.apply_scan(old, Ok(vec![entry("wrong.wav", File)])));
        assert_eq!(picker.directory(), path("/one"));
        assert_eq!(picker.pending_directory(), Some(path("/two")));
    }

    #[test]
    fn failed_matching_scan_keeps_the_previous_entries_visible() {
        let mut picker = FilePicker::from_scan(path("/one"), false, vec![entry("keep.wav", File)]);
        let request_id = picker.begin_scan(path("/two"));

        assert!(picker.apply_scan(request_id, Err("permission denied".to_owned())));
        assert_eq!(picker.directory(), path("/one"));
        assert_eq!(picker.visible_names(), ["keep.wav"]);
        assert_eq!(picker.error(), Some("permission denied"));
        assert_eq!(picker.failed_directory(), Some(path("/two")));
        assert!(!picker.is_scanning());
    }

    #[test]
    fn slow_scan_keeps_committed_directory_and_entries_visible() {
        let mut picker = FilePicker::from_scan(path("/one"), false, vec![entry("keep.wav", File)]);

        picker.begin_scan(path("/two"));

        assert_eq!(picker.directory(), path("/one"));
        assert_eq!(picker.visible_names(), ["keep.wav"]);
        assert_eq!(picker.pending_directory(), Some(path("/two")));
        assert!(picker.is_scanning());
    }

    #[test]
    fn successful_empty_scan_commits_an_empty_complete_directory() {
        let mut picker = FilePicker::from_scan(path("/one"), false, vec![entry("keep.wav", File)]);
        let request_id = picker.begin_scan(path("/empty"));

        assert!(picker.apply_scan(request_id, Ok(Vec::new())));

        assert_eq!(picker.directory(), path("/empty"));
        assert!(picker.entries().is_empty());
        assert!(!picker.is_scanning());
        assert_eq!(picker.pending_directory(), None);
        assert_eq!(picker.error(), None);
    }

    #[test]
    fn hidden_toggle_without_a_pending_scan_restarts_the_committed_directory() {
        let mut picker = FilePicker::from_scan(path("/one"), false, vec![entry("keep.wav", File)]);

        picker.toggle_hidden();

        assert!(picker.show_hidden());
        assert_eq!(picker.directory(), path("/one"));
        assert_eq!(picker.pending_directory(), Some(path("/one")));
        assert_eq!(picker.visible_names(), ["keep.wav"]);
    }

    #[test]
    fn repeated_hidden_toggles_restart_the_pending_target_and_reject_prior_ids() {
        let mut picker = FilePicker::from_scan(path("/one"), false, vec![entry("keep.wav", File)]);
        let first = picker.begin_scan(path("/two"));
        let second = picker.toggle_hidden();

        assert!(picker.show_hidden());
        assert_eq!(picker.directory(), path("/one"));
        assert_eq!(picker.pending_directory(), Some(path("/two")));
        assert!(!picker.apply_scan(first, Ok(Vec::new())));

        let third = picker.toggle_hidden();
        assert!(!picker.show_hidden());
        assert_eq!(picker.pending_directory(), Some(path("/two")));
        assert!(!picker.apply_scan(second, Ok(Vec::new())));
        assert!(picker.apply_scan(third, Ok(Vec::new())));
        assert_eq!(picker.directory(), path("/two"));
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_paths_remain_lossless_while_display_is_lossy() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let name = OsString::from_vec(vec![b'k', 0x80, b'.', b'w', b'a', b'v']);
        let source = PathBuf::from("/samples").join(name);
        let picker = FilePicker::from_scan(
            path("/samples"),
            false,
            vec![DirectoryEntry {
                path: source.clone(),
                kind: File,
            }],
        );

        assert_eq!(picker.entries()[0].path, source);
        assert!(picker.visible_names()[0].contains('\u{fffd}'));
    }
}
