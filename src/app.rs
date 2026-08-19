//! Application state and key handling. Deliberately contains no rendering so
//! the state machine can be exercised without a terminal.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::delete::{self, DeleteMode, DeleteMsg, DeleteReport, DeleteState};
use crate::model::{
    DupeGroup, KeepStrategy, Phase, ScanError, ScanMsg, ScanOptions, ScanState, SortKey,
};
use crate::scan;

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Picker,
    Scanning,
    Results,
    Confirm,
    Deleting,
    Done,
}

/// Which pane has keyboard focus on the results screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Groups,
    Files,
}

/// One row in the directory browser.
#[derive(Debug, Clone)]
pub struct DirEntryRow {
    pub name: String,
    pub path: PathBuf,
    pub is_parent: bool,
    /// Files are listed for context but are not scan targets: you scan a tree.
    pub is_dir: bool,
    /// Size in bytes, for files only.
    pub size: Option<u64>,
    /// A Windows drive root such as `D:\`, offered so the user can move
    /// between disks. Never set on Unix, which has a single root.
    pub is_drive: bool,
}

/// Everything the UI draws and the keys mutate.
pub struct App {
    pub screen: Screen,
    pub should_quit: bool,
    pub options: ScanOptions,
    pub delete_mode: DeleteMode,

    // Directory browser
    pub cwd: PathBuf,
    pub entries: Vec<DirEntryRow>,
    pub picker_selected: usize,
    pub show_hidden_in_picker: bool,
    pub picker_error: Option<String>,

    // Scan
    pub scan_state: Arc<ScanState>,
    pub scan_rx: Option<Receiver<ScanMsg>>,
    pub phase: Phase,
    pub scan_started: Option<Instant>,
    pub scan_elapsed: Option<Duration>,
    pub scan_root: Option<PathBuf>,
    pub errors: Vec<ScanError>,

    // Results
    pub groups: Vec<DupeGroup>,
    pub group_selected: usize,
    pub file_selected: usize,
    pub pane: Pane,
    pub sort: SortKey,
    pub status: Option<String>,

    // Delete
    pub delete_state: Arc<DeleteState>,
    pub delete_rx: Option<Receiver<DeleteMsg>>,
    pub report: Option<DeleteReport>,
    pub failure_scroll: usize,
}

impl App {
    pub fn new(start_dir: PathBuf, options: ScanOptions, delete_mode: DeleteMode) -> Self {
        let mut app = Self {
            screen: Screen::Picker,
            should_quit: false,
            options,
            delete_mode,
            cwd: start_dir,
            entries: Vec::new(),
            picker_selected: 0,
            show_hidden_in_picker: false,
            picker_error: None,
            scan_state: Arc::new(ScanState::default()),
            scan_rx: None,
            phase: Phase::Walking,
            scan_started: None,
            scan_elapsed: None,
            scan_root: None,
            errors: Vec::new(),
            groups: Vec::new(),
            group_selected: 0,
            file_selected: 0,
            pane: Pane::Groups,
            sort: SortKey::Wasted,
            status: None,
            delete_state: Arc::new(DeleteState::default()),
            delete_rx: None,
            report: None,
            failure_scroll: 0,
        };
        app.refresh_entries();
        app
    }

    /// True while a background worker is running, so the event loop can poll
    /// faster and keep the live counters smooth.
    pub fn is_busy(&self) -> bool {
        matches!(self.screen, Screen::Scanning | Screen::Deleting)
    }

    // ---------------------------------------------------------------- picker

    /// Build rows for the drives the user could switch to.
    ///
    /// Kept separate from enumeration so the ordering and filtering can be
    /// tested on any platform.
    pub(crate) fn drive_rows(drives: &[PathBuf], cwd: &Path) -> Vec<DirEntryRow> {
        drives
            .iter()
            .filter(|d| d.as_path() != cwd)
            .map(|d| DirEntryRow {
                name: d.to_string_lossy().into_owned(),
                path: d.clone(),
                is_parent: false,
                is_dir: true,
                size: None,
                is_drive: true,
            })
            .collect()
    }

    /// Re-read the current directory. Unreadable directories surface as an
    /// in-screen error rather than crashing or silently showing nothing.
    pub fn refresh_entries(&mut self) {
        self.entries.clear();
        self.picker_error = None;

        if let Some(parent) = self.cwd.parent() {
            self.entries.push(DirEntryRow {
                name: "..".to_string(),
                path: parent.to_path_buf(),
                is_parent: true,
                is_dir: true,
                size: None,
                is_drive: false,
            });
        } else {
            // Top of this drive: `..` leads nowhere, so the sibling drives take
            // its place. This is how you get from C:\ to D:\ on Windows.
            self.entries
                .extend(Self::drive_rows(&available_drives(), &self.cwd));
        }

        match std::fs::read_dir(&self.cwd) {
            Ok(reader) => {
                let mut dirs: Vec<DirEntryRow> = Vec::new();
                let mut files: Vec<DirEntryRow> = Vec::new();
                for entry in reader.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !self.show_hidden_in_picker && name.starts_with('.') {
                        continue;
                    }
                    let file_type = entry.file_type().ok();
                    let is_dir = file_type.map(|t| t.is_dir()).unwrap_or(false);
                    let row = DirEntryRow {
                        name,
                        path: entry.path(),
                        is_parent: false,
                        is_dir,
                        // Only files show a size; a directory's own size is
                        // meaningless here and stat-ing the tree would be slow.
                        size: if is_dir {
                            None
                        } else {
                            entry.metadata().ok().map(|m| m.len())
                        },
                        is_drive: false,
                    };
                    if is_dir {
                        dirs.push(row);
                    } else {
                        files.push(row);
                    }
                }
                // Directories first so that adding files for context never puts
                // them further from the cursor.
                dirs.sort_by_key(|d| d.name.to_lowercase());
                files.sort_by_key(|f| f.name.to_lowercase());
                self.entries.extend(dirs);
                self.entries.extend(files);
            }
            Err(err) => self.picker_error = Some(err.to_string()),
        }

        self.picker_selected = self
            .picker_selected
            .min(self.entries.len().saturating_sub(1));
    }

    /// The directory `s` would scan: the highlighted one, or the current
    /// directory when the highlight is on the parent row, on a file, or nowhere.
    /// A file is never a scan target -- dupefind scans trees.
    pub fn scan_target(&self) -> PathBuf {
        match self.entries.get(self.picker_selected) {
            Some(row) if row.is_dir && !row.is_parent => row.path.clone(),
            _ => self.cwd.clone(),
        }
    }

    /// Short name of the scan target, for the footer hint.
    pub fn scan_target_label(&self) -> String {
        let target = self.scan_target();
        target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| target.to_string_lossy().into_owned())
    }

    fn enter_selected_dir(&mut self) {
        match self.entries.get(self.picker_selected) {
            // Files are shown for context only; there is nothing to open.
            Some(row) if row.is_dir => {
                self.cwd = row.path.clone();
                self.picker_selected = 0;
                self.refresh_entries();
            }
            _ => {}
        }
    }

    fn go_to_parent(&mut self) {
        if let Some(parent) = self.cwd.parent() {
            self.cwd = parent.to_path_buf();
            self.picker_selected = 0;
            self.refresh_entries();
        }
    }

    // ------------------------------------------------------------------ scan

    /// Spawn the scanner on the current directory.
    pub fn start_scan(&mut self, root: PathBuf) {
        let (tx, rx): (Sender<ScanMsg>, Receiver<ScanMsg>) = crossbeam_channel::unbounded();
        self.scan_state = Arc::new(ScanState::default());
        self.scan_rx = Some(rx);
        self.errors.clear();
        self.groups.clear();
        self.group_selected = 0;
        self.file_selected = 0;
        self.phase = Phase::Walking;
        self.scan_started = Some(Instant::now());
        self.scan_elapsed = None;
        self.scan_root = Some(root.clone());
        self.screen = Screen::Scanning;
        self.status = None;

        let state = Arc::clone(&self.scan_state);
        let options = self.options.clone();
        std::thread::Builder::new()
            .name("dupefind-scan".into())
            .spawn(move || scan::run(root, options, state, tx))
            .expect("failed to spawn scan thread");
    }

    pub fn handle_scan_msg(&mut self, msg: ScanMsg) {
        match msg {
            ScanMsg::Phase(p) => self.phase = p,
            ScanMsg::Error(e) => {
                // Bound the list so a tree full of permission errors cannot
                // grow memory without limit.
                if self.errors.len() < 1_000 {
                    self.errors.push(e);
                }
            }
            ScanMsg::Done(groups) => {
                self.groups = groups;
                self.scan_elapsed = self.scan_started.map(|s| s.elapsed());
                self.scan_rx = None;
                self.sort_groups();
                self.group_selected = 0;
                self.file_selected = 0;
                self.pane = Pane::Groups;
                self.screen = Screen::Results;
            }
            ScanMsg::Cancelled => {
                self.scan_elapsed = self.scan_started.map(|s| s.elapsed());
                self.scan_rx = None;
                self.screen = Screen::Picker;
                self.status = Some("Scan cancelled".to_string());
            }
        }
    }

    // --------------------------------------------------------------- results

    pub fn sort_groups(&mut self) {
        // Remember the selected group so sorting does not move the cursor to an
        // unrelated row underneath the user.
        let anchor = self.groups.get(self.group_selected).map(|g| g.hash);

        match self.sort {
            SortKey::Wasted => self.groups.sort_by(|a, b| {
                b.wasted()
                    .cmp(&a.wasted())
                    .then_with(|| a.label().cmp(&b.label()))
            }),
            SortKey::Size => self
                .groups
                .sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.label().cmp(&b.label()))),
            SortKey::Count => self.groups.sort_by(|a, b| {
                b.files
                    .len()
                    .cmp(&a.files.len())
                    .then_with(|| b.wasted().cmp(&a.wasted()))
            }),
            SortKey::Name => self.groups.sort_by(|a, b| {
                a.label()
                    .to_lowercase()
                    .cmp(&b.label().to_lowercase())
                    .then_with(|| b.wasted().cmp(&a.wasted()))
            }),
        }

        if let Some(hash) = anchor
            && let Some(idx) = self.groups.iter().position(|g| g.hash == hash)
        {
            self.group_selected = idx;
        }
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.groups.is_empty() {
            self.group_selected = 0;
            self.file_selected = 0;
            return;
        }
        self.group_selected = self.group_selected.min(self.groups.len() - 1);
        let files = self.groups[self.group_selected].files.len();
        self.file_selected = self.file_selected.min(files.saturating_sub(1));
    }

    pub fn selected_group(&self) -> Option<&DupeGroup> {
        self.groups.get(self.group_selected)
    }

    pub fn total_reclaimable(&self) -> u64 {
        delete::pending_bytes(&self.groups)
    }

    pub fn total_marked(&self) -> usize {
        delete::pending_count(&self.groups)
    }

    /// Number of redundant copies across all groups, ignoring the marks.
    pub fn total_duplicates(&self) -> usize {
        self.groups
            .iter()
            .map(|g| g.files.len().saturating_sub(1))
            .sum()
    }

    fn apply_strategy_to_all(&mut self, strategy: KeepStrategy) {
        for group in &mut self.groups {
            group.apply_strategy(strategy);
        }
        self.status = Some(format!("Keeping the {} in every group", strategy.label()));
    }

    // ---------------------------------------------------------------- delete

    fn start_delete(&mut self) {
        let targets = delete::pending(&self.groups);
        if targets.is_empty() {
            self.screen = Screen::Results;
            self.status = Some("Nothing marked for deletion".to_string());
            return;
        }

        let (tx, rx) = crossbeam_channel::unbounded();
        self.delete_state = Arc::new(DeleteState::default());
        self.delete_rx = Some(rx);
        self.screen = Screen::Deleting;

        let state = Arc::clone(&self.delete_state);
        let mode = self.delete_mode;
        std::thread::Builder::new()
            .name("dupefind-delete".into())
            .spawn(move || delete::run(targets, mode, state, tx))
            .expect("failed to spawn delete thread");
    }

    pub fn handle_delete_msg(&mut self, msg: DeleteMsg) {
        match msg {
            DeleteMsg::Done(report) => {
                // Drop the entries that are gone, so a rescan is not required to
                // see an accurate picture. Files that failed to delete are still
                // on disk and must stay listed.
                let failed: HashSet<PathBuf> =
                    report.failures.iter().map(|f| f.path.clone()).collect();
                for group in &mut self.groups {
                    let skipped = group.skipped;
                    group
                        .files
                        .retain(|f| f.keep || skipped || failed.contains(&f.path));
                }
                self.groups.retain(|g| g.files.len() > 1);
                self.clamp_selection();

                self.report = Some(report);
                self.delete_rx = None;
                self.failure_scroll = 0;
                self.screen = Screen::Done;
            }
        }
    }

    // ------------------------------------------------------------------ keys

    pub fn on_key(&mut self, key: KeyEvent) {
        // Ctrl-C always quits, from any screen.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            self.cancel_workers();
            self.should_quit = true;
            return;
        }

        match self.screen {
            Screen::Picker => self.on_key_picker(key),
            Screen::Scanning => self.on_key_scanning(key),
            Screen::Results => self.on_key_results(key),
            Screen::Confirm => self.on_key_confirm(key),
            Screen::Deleting => self.on_key_deleting(key),
            Screen::Done => self.on_key_done(key),
        }
    }

    fn cancel_workers(&self) {
        self.scan_state.request_cancel();
        self.delete_state.request_cancel();
    }

    fn on_key_picker(&mut self, key: KeyEvent) {
        let last = self.entries.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Down | KeyCode::Char('j') => {
                self.picker_selected = (self.picker_selected + 1).min(last);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.picker_selected = self.picker_selected.saturating_sub(1);
            }
            KeyCode::Home | KeyCode::Char('g') => self.picker_selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.picker_selected = last,
            KeyCode::PageDown => self.picker_selected = (self.picker_selected + 10).min(last),
            KeyCode::PageUp => self.picker_selected = self.picker_selected.saturating_sub(10),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if self
                    .entries
                    .get(self.picker_selected)
                    .is_some_and(|r| r.is_parent)
                {
                    self.go_to_parent();
                } else {
                    self.enter_selected_dir();
                }
            }
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.go_to_parent(),
            KeyCode::Char('~') => {
                if let Some(home) = home_dir() {
                    self.cwd = home;
                    self.picker_selected = 0;
                    self.refresh_entries();
                }
            }
            KeyCode::Char('s') => {
                let root = self.scan_target();
                self.start_scan(root);
            }
            // Jump to the top of the current drive, which is where the other
            // drives are listed.
            KeyCode::Char('d') => {
                if let Some(root) = self.cwd.ancestors().last() {
                    self.cwd = root.to_path_buf();
                    self.picker_selected = 0;
                    self.refresh_entries();
                }
            }
            KeyCode::Char('.') => {
                self.show_hidden_in_picker = !self.show_hidden_in_picker;
                self.refresh_entries();
            }
            // Filter toggles, mirroring the CLI flags.
            KeyCode::Char('1') => self.options.skip_hidden = !self.options.skip_hidden,
            KeyCode::Char('2') => {
                self.options.respect_gitignore = !self.options.respect_gitignore;
            }
            KeyCode::Char('3') => self.options.skip_empty = !self.options.skip_empty,
            KeyCode::Char('4') => {
                self.options.collapse_hardlinks = !self.options.collapse_hardlinks;
            }
            _ => {}
        }
    }

    fn on_key_scanning(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.scan_state.request_cancel();
            self.status = Some("Cancelling…".to_string());
        }
    }

    fn on_key_results(&mut self, key: KeyEvent) {
        self.status = None;
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => {
                self.screen = Screen::Picker;
                self.refresh_entries();
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.pane = match self.pane {
                    Pane::Groups => Pane::Files,
                    Pane::Files => Pane::Groups,
                };
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Home => self.set_selection(0),
            KeyCode::End => self.set_selection(usize::MAX),
            KeyCode::Right | KeyCode::Char('l') => self.pane = Pane::Files,
            KeyCode::Left | KeyCode::Char('h') => self.pane = Pane::Groups,

            KeyCode::Char(' ') => {
                let idx = self.file_selected;
                if let Some(group) = self.groups.get_mut(self.group_selected) {
                    group.keep_only(idx);
                    group.skipped = false;
                }
                self.pane = Pane::Files;
            }
            KeyCode::Char('d') => {
                let idx = self.file_selected;
                let refused = match self.groups.get_mut(self.group_selected) {
                    Some(group) => !group.toggle_mark(idx),
                    None => false,
                };
                if refused {
                    self.status = Some("At least one copy in a group must be kept".to_string());
                }
                self.pane = Pane::Files;
            }
            KeyCode::Char('x') => {
                if let Some(group) = self.groups.get_mut(self.group_selected) {
                    group.skipped = !group.skipped;
                }
            }
            KeyCode::Char('1') => self.apply_strategy_to_all(KeepStrategy::First),
            KeyCode::Char('2') => self.apply_strategy_to_all(KeepStrategy::Newest),
            KeyCode::Char('3') => self.apply_strategy_to_all(KeepStrategy::Oldest),
            KeyCode::Char('4') => self.apply_strategy_to_all(KeepStrategy::ShortestPath),
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.sort_groups();
                self.status = Some(format!("Sorted by {}", self.sort.label()));
            }
            KeyCode::Char('t') => {
                self.delete_mode = self.delete_mode.toggled();
                self.status = Some(format!("Delete mode: {}", self.delete_mode.label()));
            }
            KeyCode::Char('D') => {
                if self.total_marked() == 0 {
                    self.status = Some("Nothing marked for deletion".to_string());
                } else {
                    self.screen = Screen::Confirm;
                }
            }
            KeyCode::Char('r') => {
                if let Some(root) = self.scan_root.clone() {
                    self.start_scan(root);
                }
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = match self.pane {
            Pane::Groups => self.groups.len(),
            Pane::Files => self.selected_group().map(|g| g.files.len()).unwrap_or(0),
        };
        if len == 0 {
            return;
        }
        let current = match self.pane {
            Pane::Groups => self.group_selected,
            Pane::Files => self.file_selected,
        } as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        match self.pane {
            Pane::Groups => {
                if next != self.group_selected {
                    self.group_selected = next;
                    self.file_selected = 0;
                }
            }
            Pane::Files => self.file_selected = next,
        }
    }

    fn set_selection(&mut self, target: usize) {
        let len = match self.pane {
            Pane::Groups => self.groups.len(),
            Pane::Files => self.selected_group().map(|g| g.files.len()).unwrap_or(0),
        };
        if len == 0 {
            return;
        }
        let idx = target.min(len - 1);
        match self.pane {
            Pane::Groups => {
                self.group_selected = idx;
                self.file_selected = 0;
            }
            Pane::Files => self.file_selected = idx,
        }
    }

    fn on_key_confirm(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.start_delete(),
            KeyCode::Char('t') => self.delete_mode = self.delete_mode.toggled(),
            _ => self.screen = Screen::Results,
        }
    }

    fn on_key_deleting(&mut self, key: KeyEvent) {
        if matches!(key.code, KeyCode::Esc) {
            self.delete_state.request_cancel();
        }
    }

    fn on_key_done(&mut self, key: KeyEvent) {
        let failures = self.report.as_ref().map(|r| r.failures.len()).unwrap_or(0);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('r') => {
                if let Some(root) = self.scan_root.clone() {
                    self.start_scan(root);
                }
            }
            KeyCode::Enter => {
                self.report = None;
                self.screen = Screen::Results;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.failure_scroll = (self.failure_scroll + 1).min(failures.saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.failure_scroll = self.failure_scroll.saturating_sub(1);
            }
            _ => {}
        }
    }

    // --------------------------------------------------------------- getters

    pub fn elapsed(&self) -> Duration {
        self.scan_elapsed
            .or_else(|| self.scan_started.map(|s| s.elapsed()))
            .unwrap_or_default()
    }

    pub fn hash_progress(&self) -> (u64, u64) {
        scan::hashed_of(&self.scan_state)
    }

    pub fn scanned_bytes(&self) -> u64 {
        self.scan_state.bytes_seen.load(Ordering::Relaxed)
    }

    pub fn scanned_files(&self) -> u64 {
        self.scan_state.files_seen.load(Ordering::Relaxed)
    }
}

/// Drive roots the user can switch to.
///
/// Windows has no parent above `C:\`, so drives have to be enumerated to let
/// the user reach another disk. Probing `A:\`..`Z:\` avoids a winapi
/// dependency for what is 26 cheap stat calls, done only at a drive root.
#[cfg(windows)]
pub fn available_drives() -> Vec<PathBuf> {
    (b'A'..=b'Z')
        .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
        .filter(|p| p.is_dir())
        .collect()
}

/// Unix has a single root, so there is nothing to switch between.
#[cfg(not(windows))]
pub fn available_drives() -> Vec<PathBuf> {
    Vec::new()
}

/// Home directory without pulling in a crate for one lookup.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let key = "USERPROFILE";
    #[cfg(not(windows))]
    let key = "HOME";
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delete::{DeleteFailure, DeleteReport};
    use crate::model::FileEntry;
    use ratatui::crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(key(code));
    }

    fn entry(path: &str, size: u64, mtime: Option<u64>) -> FileEntry {
        FileEntry::new(
            PathBuf::from(path),
            size,
            mtime.map(|s| std::time::UNIX_EPOCH + Duration::from_secs(s)),
        )
    }

    /// Two groups: a three-copy group and a two-copy group.
    fn app_with_results() -> App {
        let mut app = App::new(
            std::env::temp_dir(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.scan_root = Some(PathBuf::from("/scan/root"));
        app.groups = vec![
            DupeGroup::new(
                [1u8; 32],
                1000,
                vec![
                    entry("/aaa/first.jpg", 1000, Some(100)),
                    entry("/b.jpg", 1000, Some(300)),
                    entry("/aaa/bbb/third.jpg", 1000, Some(200)),
                ],
            ),
            DupeGroup::new(
                [2u8; 32],
                50,
                vec![
                    entry("/x/doc.pdf", 50, Some(10)),
                    entry("/y/doc.pdf", 50, Some(20)),
                ],
            ),
        ];
        app.screen = Screen::Results;
        app.sort = SortKey::Wasted;
        app.sort_groups();
        app
    }

    #[test]
    fn a_fresh_app_starts_in_the_picker() {
        let app = App::new(
            std::env::temp_dir(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        assert_eq!(app.screen, Screen::Picker);
        assert!(!app.should_quit);
    }

    #[test]
    fn quick_select_keeps_the_first_copy_in_every_group() {
        let mut app = app_with_results();
        // Disturb the marks first so the assertion is meaningful.
        app.groups[0].keep_only(2);
        app.groups[1].keep_only(1);

        press(&mut app, KeyCode::Char('1'));

        for group in &app.groups {
            assert_eq!(group.keeper_count(), 1);
            assert!(group.files[0].keep, "the first copy should be the keeper");
        }
    }

    #[test]
    fn every_bulk_strategy_leaves_one_keeper_per_group() {
        for code in ['1', '2', '3', '4'] {
            let mut app = app_with_results();
            press(&mut app, KeyCode::Char(code));
            for group in &app.groups {
                assert_eq!(
                    group.keeper_count(),
                    1,
                    "strategy key {code} broke the one-keeper invariant"
                );
            }
        }
    }

    #[test]
    fn keep_newest_selects_by_mtime() {
        let mut app = app_with_results();
        press(&mut app, KeyCode::Char('2'));
        let kept = app.groups[0].files.iter().find(|f| f.keep).unwrap();
        assert_eq!(kept.file_name(), "b.jpg", "b.jpg has the latest mtime");
    }

    #[test]
    fn space_makes_the_highlighted_file_the_keeper() {
        let mut app = app_with_results();
        app.pane = Pane::Files;
        app.file_selected = 2;
        press(&mut app, KeyCode::Char(' '));
        assert!(app.groups[0].files[2].keep);
        assert_eq!(app.groups[0].keeper_count(), 1);
    }

    #[test]
    fn space_un_skips_a_group_so_the_choice_takes_effect() {
        let mut app = app_with_results();
        app.groups[0].skipped = true;
        app.pane = Pane::Files;
        app.file_selected = 1;
        press(&mut app, KeyCode::Char(' '));
        assert!(!app.groups[0].skipped);
        assert!(app.groups[0].files[1].keep);
    }

    #[test]
    fn toggling_the_last_keeper_is_refused_with_an_explanation() {
        let mut app = app_with_results();
        app.pane = Pane::Files;
        // Index 0 is the sole keeper after the default strategy.
        app.file_selected = 0;
        press(&mut app, KeyCode::Char('d'));

        assert_eq!(app.groups[0].keeper_count(), 1);
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("must be kept"),
            "the user should be told why nothing happened"
        );
    }

    #[test]
    fn no_sequence_of_keys_can_mark_a_whole_group_for_deletion() {
        let mut app = app_with_results();
        app.pane = Pane::Files;
        // Walk the pane marking everything, several times over.
        for _ in 0..5 {
            for i in 0..app.groups[0].files.len() {
                app.file_selected = i;
                press(&mut app, KeyCode::Char('d'));
                assert!(app.groups[0].keeper_count() >= 1);
            }
        }
    }

    #[test]
    fn skipping_a_group_excludes_it_from_the_plan() {
        let mut app = app_with_results();
        let before = app.total_marked();
        press(&mut app, KeyCode::Char('x'));
        assert!(app.groups[app.group_selected].skipped);
        assert!(app.total_marked() < before);
        // And toggling back restores it.
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.total_marked(), before);
    }

    #[test]
    fn tab_switches_panes() {
        let mut app = app_with_results();
        assert_eq!(app.pane, Pane::Groups);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.pane, Pane::Files);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.pane, Pane::Groups);
    }

    #[test]
    fn navigation_stays_within_bounds() {
        let mut app = app_with_results();
        for _ in 0..50 {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(app.group_selected, app.groups.len() - 1);
        for _ in 0..50 {
            press(&mut app, KeyCode::Up);
        }
        assert_eq!(app.group_selected, 0);
    }

    #[test]
    fn moving_between_groups_resets_the_file_cursor() {
        let mut app = app_with_results();
        app.pane = Pane::Files;
        app.file_selected = 2;
        app.pane = Pane::Groups;
        press(&mut app, KeyCode::Down);
        assert_eq!(app.file_selected, 0, "a new group starts at its first file");
    }

    #[test]
    fn the_file_cursor_never_exceeds_the_smaller_group() {
        let mut app = app_with_results();
        // Group 0 has three files, group 1 has two.
        app.group_selected = 0;
        app.pane = Pane::Files;
        app.file_selected = 2;
        app.group_selected = 1;
        app.clamp_selection();
        assert!(app.file_selected < app.groups[1].files.len());
    }

    #[test]
    fn navigating_an_empty_result_set_is_harmless() {
        let mut app = app_with_results();
        app.groups.clear();
        for code in [KeyCode::Down, KeyCode::Up, KeyCode::Home, KeyCode::End] {
            press(&mut app, code);
        }
        assert_eq!(app.group_selected, 0);
        assert_eq!(app.total_marked(), 0);
    }

    #[test]
    fn sorting_keeps_the_cursor_on_the_same_group() {
        let mut app = app_with_results();
        app.group_selected = 1;
        let hash_before = app.groups[1].hash;
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(
            app.groups[app.group_selected].hash, hash_before,
            "the highlighted group should follow the sort"
        );
    }

    #[test]
    fn sorting_cycles_through_every_key() {
        let mut app = app_with_results();
        let mut seen = vec![app.sort];
        for _ in 0..3 {
            press(&mut app, KeyCode::Char('s'));
            seen.push(app.sort);
        }
        assert_eq!(seen.len(), 4);
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.sort, seen[0], "sorting should cycle back around");
    }

    #[test]
    fn t_toggles_the_delete_mode() {
        let mut app = app_with_results();
        assert_eq!(app.delete_mode, DeleteMode::Trash);
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.delete_mode, DeleteMode::Permanent);
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.delete_mode, DeleteMode::Trash);
    }

    #[test]
    fn delete_needs_something_marked() {
        let mut app = app_with_results();
        // Skip every group so nothing is marked.
        for group in &mut app.groups {
            group.skipped = true;
        }
        press(&mut app, KeyCode::Char('D'));
        assert_eq!(
            app.screen,
            Screen::Results,
            "should not reach the confirmation"
        );
        assert!(app.status.is_some(), "the user should be told why");
    }

    #[test]
    fn delete_opens_the_confirmation_when_files_are_marked() {
        let mut app = app_with_results();
        assert!(app.total_marked() > 0);
        press(&mut app, KeyCode::Char('D'));
        assert_eq!(app.screen, Screen::Confirm);
    }

    #[test]
    fn escape_backs_out_of_the_confirmation_without_deleting() {
        let mut app = app_with_results();
        press(&mut app, KeyCode::Char('D'));
        assert_eq!(app.screen, Screen::Confirm);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.screen, Screen::Results);
    }

    #[test]
    fn the_confirmation_can_switch_delete_mode_in_place() {
        let mut app = app_with_results();
        press(&mut app, KeyCode::Char('D'));
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.screen, Screen::Confirm, "still confirming");
        assert_eq!(app.delete_mode, DeleteMode::Permanent);
    }

    #[test]
    fn ctrl_c_quits_from_any_screen() {
        for screen in [
            Screen::Picker,
            Screen::Scanning,
            Screen::Results,
            Screen::Confirm,
            Screen::Deleting,
            Screen::Done,
        ] {
            let mut app = app_with_results();
            app.screen = screen;
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(app.should_quit, "Ctrl-C should quit from {screen:?}");
        }
    }

    #[test]
    fn escape_during_a_scan_requests_cancellation() {
        let mut app = app_with_results();
        app.screen = Screen::Scanning;
        press(&mut app, KeyCode::Esc);
        assert!(app.scan_state.is_cancelled());
        assert!(
            !app.should_quit,
            "cancelling a scan should not quit the app"
        );
    }

    #[test]
    fn reclaimable_totals_reflect_the_current_marks() {
        let mut app = app_with_results();
        // Group 0: 3 copies of 1000 bytes -> 2000 reclaimable.
        // Group 1: 2 copies of 50 bytes   ->   50 reclaimable.
        assert_eq!(app.total_reclaimable(), 2_050);
        assert_eq!(app.total_marked(), 3);
        assert_eq!(app.total_duplicates(), 3);

        press(&mut app, KeyCode::Char('x')); // skip the selected group
        assert!(app.total_reclaimable() < 2_050);
    }

    #[test]
    fn a_successful_delete_removes_the_entries_from_the_dashboard() {
        let mut app = app_with_results();
        app.handle_delete_msg(DeleteMsg::Done(DeleteReport {
            deleted: 3,
            bytes_freed: 2_050,
            failures: Vec::new(),
            mode_label: "Trash".into(),
        }));

        assert_eq!(app.screen, Screen::Done);
        assert!(
            app.groups.is_empty(),
            "with one copy left per group there are no duplicates to show"
        );
    }

    #[test]
    fn files_that_failed_to_delete_stay_listed() {
        let mut app = app_with_results();
        // The second copy of group 0 could not be removed.
        let stuck = app.groups[0].files[1].path.clone();
        app.handle_delete_msg(DeleteMsg::Done(DeleteReport {
            deleted: 2,
            bytes_freed: 1_050,
            failures: vec![DeleteFailure {
                path: stuck.clone(),
                message: "permission denied".into(),
            }],
            mode_label: "Trash".into(),
        }));

        let still_there = app
            .groups
            .iter()
            .flat_map(|g| g.files.iter())
            .any(|f| f.path == stuck);
        assert!(
            still_there,
            "a file that is still on disk must still be shown"
        );
    }

    #[test]
    fn a_skipped_group_survives_deletion_intact() {
        let mut app = app_with_results();
        app.groups[0].skipped = true;
        let before = app.groups[0].files.len();
        app.handle_delete_msg(DeleteMsg::Done(DeleteReport {
            deleted: 1,
            bytes_freed: 50,
            failures: Vec::new(),
            mode_label: "Trash".into(),
        }));
        let group = app
            .groups
            .iter()
            .find(|g| g.hash == [1u8; 32])
            .expect("the skipped group should still be listed");
        assert_eq!(group.files.len(), before);
    }

    #[test]
    fn the_picker_reads_real_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("beta")).unwrap();
        std::fs::create_dir(dir.path().join("alpha")).unwrap();
        std::fs::write(dir.path().join("a-file.txt"), b"x").unwrap();

        let app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        let names: Vec<&str> = app.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["..", "alpha", "beta", "a-file.txt"],
            "parent first, then directories, then files -- each group sorted"
        );
    }

    #[test]
    fn files_are_listed_with_their_size() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.bin"), vec![0u8; 4096]).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        let file = app
            .entries
            .iter()
            .find(|e| e.name == "data.bin")
            .expect("the file should be listed");
        assert!(!file.is_dir);
        assert_eq!(file.size, Some(4096));

        let sub = app.entries.iter().find(|e| e.name == "sub").unwrap();
        assert!(sub.is_dir);
        assert_eq!(sub.size, None, "directories carry no size");
    }

    #[test]
    fn directories_always_sort_before_files() {
        let dir = tempfile::tempdir().unwrap();
        // Names chosen so alphabetical order alone would interleave them.
        std::fs::write(dir.path().join("aaa.txt"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("zzz")).unwrap();

        let app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        let names: Vec<&str> = app.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["..", "zzz", "aaa.txt"]);
    }

    #[test]
    fn a_highlighted_file_is_never_the_scan_target() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), b"x").unwrap();

        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        let idx = app
            .entries
            .iter()
            .position(|e| e.name == "note.txt")
            .unwrap();
        app.picker_selected = idx;

        // A file cannot be scanned, so the target falls back to its directory.
        assert_eq!(app.scan_target(), dir.path());
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.scan_root.as_deref(), Some(dir.path()));
    }

    #[test]
    fn enter_on_a_file_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), b"x").unwrap();

        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        let idx = app
            .entries
            .iter()
            .position(|e| e.name == "note.txt")
            .unwrap();
        app.picker_selected = idx;

        press(&mut app, KeyCode::Enter);
        assert_eq!(app.cwd, dir.path(), "a file is not navigable");
        assert_eq!(app.screen, Screen::Picker);
        assert_eq!(app.picker_selected, idx, "the highlight should not move");
    }

    #[test]
    fn hidden_files_respect_the_same_toggle_as_hidden_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".secret.txt"), b"x").unwrap();

        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        assert!(!app.entries.iter().any(|e| e.name == ".secret.txt"));
        press(&mut app, KeyCode::Char('.'));
        assert!(app.entries.iter().any(|e| e.name == ".secret.txt"));
    }

    #[test]
    fn the_picker_toggles_hidden_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".secret")).unwrap();

        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        assert!(!app.entries.iter().any(|e| e.name == ".secret"));
        press(&mut app, KeyCode::Char('.'));
        assert!(app.entries.iter().any(|e| e.name == ".secret"));
    }

    #[test]
    fn the_picker_navigates_into_and_back_out_of_directories() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir(&child).unwrap();

        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        // Row 0 is "..", row 1 is "child".
        app.picker_selected = 1;
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.cwd, child);

        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.cwd, dir.path());
    }

    /// The highlight is the scan target, which is what the highlight looks like
    /// it means. Pressing `s` on a subdirectory must not scan its parent.
    #[test]
    fn s_scans_the_highlighted_directory() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join("alpha");
        let beta = dir.path().join("beta");
        std::fs::create_dir(&alpha).unwrap();
        std::fs::create_dir(&beta).unwrap();

        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        // Rows: 0 = "..", 1 = alpha, 2 = beta.
        app.picker_selected = 2;
        assert_eq!(app.scan_target(), beta);
        assert_eq!(app.scan_target_label(), "beta");

        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.screen, Screen::Scanning);
        assert_eq!(
            app.scan_root.as_deref(),
            Some(beta.as_path()),
            "s must scan the highlighted directory, not its parent"
        );
    }

    #[test]
    fn s_on_the_parent_row_scans_the_current_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("child")).unwrap();

        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        // Row 0 is the ".." row.
        app.picker_selected = 0;
        assert_eq!(app.scan_target(), dir.path());

        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.scan_root.as_deref(), Some(dir.path()));
    }

    #[test]
    fn the_scan_target_falls_back_to_the_current_directory() {
        let mut app = App::new(
            std::env::temp_dir(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        // An unreadable directory leaves no navigable rows.
        app.cwd = PathBuf::from("/definitely/does/not/exist");
        app.refresh_entries();
        app.entries.clear();
        assert_eq!(app.scan_target(), app.cwd);
    }

    #[test]
    fn the_scan_target_follows_the_highlight() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["one", "two"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        press(&mut app, KeyCode::Down); // -> "one"
        assert_eq!(app.scan_target_label(), "one");
        press(&mut app, KeyCode::Down); // -> "two"
        assert_eq!(app.scan_target_label(), "two");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.scan_target_label(), "one");
    }

    // Windows has no parent above C:\, so drives must be reachable some other
    // way. These exercise the pure row builder, so they run on any platform.
    #[test]
    fn drive_rows_offer_the_other_drives() {
        let drives = [
            PathBuf::from("C:\\"),
            PathBuf::from("D:\\"),
            PathBuf::from("E:\\"),
        ];
        let rows = App::drive_rows(&drives, Path::new("C:\\"));
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["D:\\", "E:\\"],
            "the current drive is not offered"
        );
        assert!(rows.iter().all(|r| r.is_drive && r.is_dir && !r.is_parent));
    }

    #[test]
    fn drive_rows_are_empty_when_there_is_one_drive() {
        let drives = [PathBuf::from("C:\\")];
        assert!(App::drive_rows(&drives, Path::new("C:\\")).is_empty());
    }

    #[test]
    fn drive_rows_are_empty_on_a_system_without_drives() {
        // available_drives() returns nothing on Unix.
        assert!(App::drive_rows(&[], Path::new("/")).is_empty());
    }

    #[test]
    fn a_drive_row_is_a_valid_scan_target() {
        let mut app = App::new(
            std::env::temp_dir(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.entries = App::drive_rows(
            &[PathBuf::from("C:\\"), PathBuf::from("D:\\")],
            Path::new("C:\\"),
        );
        app.picker_selected = 0;
        // Drives are directories, so `s` scans the whole disk.
        assert_eq!(app.scan_target(), PathBuf::from("D:\\"));
    }

    #[test]
    fn d_jumps_to_the_top_of_the_current_tree() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        let mut app = App::new(nested.clone(), ScanOptions::default(), DeleteMode::Trash);
        press(&mut app, KeyCode::Char('d'));
        // On Unix the top is `/`; on Windows it would be the drive root, which
        // is where the drive rows are listed.
        assert_eq!(app.cwd, PathBuf::from(std::path::MAIN_SEPARATOR_STR));
        assert_eq!(app.picker_selected, 0);
    }

    #[test]
    fn available_drives_is_empty_on_unix() {
        #[cfg(not(windows))]
        assert!(available_drives().is_empty());
        // On Windows there is always at least the system drive.
        #[cfg(windows)]
        assert!(!available_drives().is_empty());
    }

    #[test]
    fn the_filesystem_root_lists_no_parent_row() {
        let mut app = App::new(
            std::env::temp_dir(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.cwd = PathBuf::from(std::path::MAIN_SEPARATOR_STR);
        app.refresh_entries();
        assert!(
            !app.entries.iter().any(|e| e.is_parent),
            "the root has no parent to offer"
        );
    }

    #[test]
    fn the_picker_toggles_scan_filters() {
        let mut app = App::new(
            std::env::temp_dir(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        assert!(app.options.skip_hidden);
        press(&mut app, KeyCode::Char('1'));
        assert!(!app.options.skip_hidden);
        assert!(app.options.respect_gitignore);
        press(&mut app, KeyCode::Char('2'));
        assert!(!app.options.respect_gitignore);
    }

    #[test]
    fn an_unreadable_directory_surfaces_an_error_instead_of_panicking() {
        let mut app = App::new(
            std::env::temp_dir(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.cwd = PathBuf::from("/definitely/does/not/exist");
        app.refresh_entries();
        assert!(app.picker_error.is_some());
        // The parent row is still offered so the user is not stranded.
        assert!(app.entries.iter().any(|e| e.is_parent));
    }

    #[test]
    fn busy_only_while_a_worker_runs() {
        let mut app = app_with_results();
        assert!(!app.is_busy());
        app.screen = Screen::Scanning;
        assert!(app.is_busy());
        app.screen = Screen::Deleting;
        assert!(app.is_busy());
        app.screen = Screen::Done;
        assert!(!app.is_busy());
    }
}
