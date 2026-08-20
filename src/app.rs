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
use crate::export;
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

/// What a confirmed deletion will remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletePlan {
    /// Every copy currently marked, across all groups.
    Marked,
    /// A single file picked out in the files pane.
    Single { group: usize, file: usize },
}

/// What a browser row stands for.
///
/// An enum rather than a set of booleans: the kinds are mutually exclusive, and
/// with flags a combination like parent-and-drive would be representable. It
/// also forces `scan_target` to handle every kind explicitly, which is the point
/// -- a catch-all arm there was what made a drive root unscannable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// The directory being listed. Always offered, so "scan where I am" is
    /// reachable even at a drive root, where there is no parent.
    Current,
    /// `..`
    Parent,
    /// A sibling drive root such as `D:\`. Windows only.
    Drive,
    Directory,
    /// Listed for context; a file is not a scan target.
    File,
}

/// One row in the directory browser.
#[derive(Debug, Clone)]
pub struct DirEntryRow {
    pub name: String,
    pub path: PathBuf,
    pub kind: RowKind,
    /// Size in bytes, for `RowKind::File` only.
    pub size: Option<u64>,
}

impl DirEntryRow {
    /// True for anything the user can descend into with Enter.
    pub fn is_navigable(&self) -> bool {
        matches!(
            self.kind,
            RowKind::Parent | RowKind::Drive | RowKind::Directory
        )
    }
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
    pub export_dir: PathBuf,
    pub plan: DeletePlan,
    /// Groups the user has picked out, keyed by content hash so the selection
    /// survives re-sorting. When non-empty it scopes every bulk action.
    pub selected: HashSet<[u8; 32]>,
    /// Where a Shift-extension began, and the selection as it was at that
    /// moment, so shrinking the range deselects again instead of only growing.
    shift_anchor: Option<usize>,
    shift_base: HashSet<[u8; 32]>,

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
            cwd: start_dir.clone(),
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
            export_dir: std::env::current_dir().unwrap_or_else(|_| start_dir.clone()),
            plan: DeletePlan::Marked,
            selected: HashSet::new(),
            shift_anchor: None,
            shift_base: HashSet::new(),
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
                kind: RowKind::Drive,
                size: None,
            })
            .collect()
    }

    /// Re-read the current directory. Unreadable directories surface as an
    /// in-screen error rather than crashing or silently showing nothing.
    pub fn refresh_entries(&mut self) {
        self.entries.clear();
        self.picker_error = None;

        // Always first, so "scan the directory I am in" is reachable from any
        // listing. Without it, a drive root holding no files offers no row that
        // denotes the drive, and the whole drive cannot be scanned.
        self.entries.push(DirEntryRow {
            name: self.cwd.to_string_lossy().into_owned(),
            path: self.cwd.clone(),
            kind: RowKind::Current,
            size: None,
        });

        if let Some(parent) = self.cwd.parent() {
            self.entries.push(DirEntryRow {
                name: "..".to_string(),
                path: parent.to_path_buf(),
                kind: RowKind::Parent,
                size: None,
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
                    if !self.show_hidden_in_picker && is_hidden(&name, &entry) {
                        continue;
                    }
                    let file_type = entry.file_type().ok();
                    let is_dir = file_type.map(|t| t.is_dir()).unwrap_or(false);
                    let row = DirEntryRow {
                        name,
                        path: entry.path(),
                        kind: if is_dir {
                            RowKind::Directory
                        } else {
                            RowKind::File
                        },
                        // Only files show a size; a directory's own size is
                        // meaningless here and stat-ing the tree would be slow.
                        size: if is_dir {
                            None
                        } else {
                            entry.metadata().ok().map(|m| m.len())
                        },
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
        let Some(row) = self.entries.get(self.picker_selected) else {
            return self.cwd.clone();
        };
        // Exhaustive on purpose. The previous catch-all arm is what made a drive
        // root unscannable: only `..` and file rows reached it, and at a drive
        // root neither need exist.
        match row.kind {
            // A file cannot be scanned; the directory holding it is the target.
            RowKind::Current | RowKind::File => self.cwd.clone(),
            RowKind::Parent | RowKind::Drive | RowKind::Directory => row.path.clone(),
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
            // Files are context only, and the current-directory row is where we
            // already are: neither is something to descend into.
            Some(row) if row.is_navigable() => {
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
        if let Some(reference) = self
            .options
            .reference_roots
            .iter()
            .find(|reference| root.starts_with(reference.as_path()))
        {
            self.status = Some(format!(
                "Reference {} contains the scan root; choose a nested or external reference",
                reference.display()
            ));
            self.screen = Screen::Picker;
            return;
        }
        self.options.reference_roots.sort();
        self.options.reference_roots.dedup();
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

    /// Bytes the current marks would free **within scope**.
    pub fn total_reclaimable(&self) -> u64 {
        self.marked_targets().iter().map(|(_, size)| size).sum()
    }

    /// Files the current marks would remove **within scope**.
    pub fn total_marked(&self) -> usize {
        self.marked_targets().len()
    }

    /// Every marked copy the selection scope covers.
    ///
    /// Scoped rather than delegating to `delete::pending`, so the number in the
    /// header is exactly what `D` will act on. An unscoped header beside a
    /// scoped delete would mislead about an irreversible action.
    fn marked_targets(&self) -> Vec<(PathBuf, u64)> {
        delete::pending_in(
            self.groups
                .iter()
                .enumerate()
                .filter(|(idx, _)| self.in_scope(*idx))
                .map(|(_, group)| group),
        )
    }

    /// Number of redundant copies across all groups, ignoring the marks.
    pub fn total_duplicates(&self) -> usize {
        self.groups
            .iter()
            .map(|g| g.files.len().saturating_sub(1))
            .sum()
    }

    // ------------------------------------------------------------ selection

    /// Whether the group at `idx` is in scope for a bulk action: the selection
    /// when there is one, otherwise every group.
    pub fn in_scope(&self, idx: usize) -> bool {
        match self.groups.get(idx) {
            Some(g) => self.selected.is_empty() || self.selected.contains(&g.hash),
            None => false,
        }
    }

    pub fn is_selected(&self, idx: usize) -> bool {
        self.groups
            .get(idx)
            .is_some_and(|g| self.selected.contains(&g.hash))
    }

    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    /// Toggle the highlighted group in or out of the selection.
    fn toggle_selection(&mut self) {
        self.end_extension();
        if let Some(group) = self.groups.get(self.group_selected) {
            let hash = group.hash;
            if !self.selected.remove(&hash) {
                self.selected.insert(hash);
            }
        }
    }

    /// Select every group, or clear the selection if everything is already in.
    fn toggle_select_all(&mut self) {
        self.end_extension();
        if self.selected.len() == self.groups.len() && !self.groups.is_empty() {
            self.selected.clear();
            self.status = Some("Selection cleared".to_string());
        } else {
            self.selected = self.groups.iter().map(|g| g.hash).collect();
            self.status = Some(format!("Selected all {} groups", self.groups.len()));
        }
    }

    /// Move the cursor and select the block between the anchor and the cursor.
    ///
    /// The selection is recomputed from the anchor each time rather than only
    /// added to, so reversing direction shrinks the block as a file manager
    /// would, instead of leaving stale rows selected.
    fn extend_selection(&mut self, delta: isize) {
        if self.groups.is_empty() {
            return;
        }
        if self.shift_anchor.is_none() {
            self.shift_anchor = Some(self.group_selected);
            self.shift_base = self.selected.clone();
        }

        let last = self.groups.len() as isize - 1;
        let next = (self.group_selected as isize + delta).clamp(0, last) as usize;
        self.group_selected = next;
        self.file_selected = 0;

        let anchor = self.shift_anchor.unwrap_or(next).min(self.groups.len() - 1);
        let (lo, hi) = if anchor <= next {
            (anchor, next)
        } else {
            (next, anchor)
        };

        self.selected = self.shift_base.clone();
        for group in &self.groups[lo..=hi] {
            self.selected.insert(group.hash);
        }
    }

    /// Any cursor move or toggle that is not a Shift-extension ends the block.
    fn end_extension(&mut self) {
        self.shift_anchor = None;
        self.shift_base.clear();
    }

    /// Indices the bulk actions apply to.
    fn scoped_indices(&self) -> Vec<usize> {
        (0..self.groups.len())
            .filter(|i| self.in_scope(*i))
            .collect()
    }

    fn apply_strategy_to_all(&mut self, strategy: KeepStrategy) {
        let scoped = self.scoped_indices();
        for idx in &scoped {
            if let Some(group) = self.groups.get_mut(*idx) {
                group.apply_strategy(strategy);
            }
        }
        self.status = Some(if self.selected.is_empty() {
            format!("Keeping the {} in every group", strategy.label())
        } else {
            format!(
                "Keeping the {} in {} selected group{}",
                strategy.label(),
                scoped.len(),
                if scoped.len() == 1 { "" } else { "s" }
            )
        });
    }

    /// Skip or unskip every group in scope, moving them all to one state rather
    /// than flipping each independently.
    fn toggle_skip_in_scope(&mut self) {
        if self.selected.is_empty() {
            if let Some(group) = self.groups.get_mut(self.group_selected) {
                group.skipped = !group.skipped;
            }
            return;
        }
        let scoped = self.scoped_indices();
        let skip = scoped
            .iter()
            .any(|i| self.groups.get(*i).is_some_and(|g| !g.skipped));
        for idx in &scoped {
            if let Some(group) = self.groups.get_mut(*idx) {
                group.skipped = skip;
            }
        }
        self.status = Some(format!(
            "{} {} selected group{}",
            if skip { "Skipped" } else { "Un-skipped" },
            scoped.len(),
            if scoped.len() == 1 { "" } else { "s" }
        ));
    }

    // ---------------------------------------------------------------- delete

    /// The files the current plan would remove.
    pub fn planned_targets(&self) -> Vec<(PathBuf, u64)> {
        match self.plan {
            DeletePlan::Marked => self.marked_targets(),
            DeletePlan::Single { group, file } => self
                .groups
                .get(group)
                .and_then(|g| g.files.get(file))
                .filter(|f| !f.protected)
                .map(|f| vec![(f.path.clone(), f.size)])
                .unwrap_or_default(),
        }
    }

    fn start_delete(&mut self) {
        let targets = self.planned_targets();
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
                // Prune exactly what left the disk, so a rescan is not needed
                // to see an accurate picture. Going by the marks instead would
                // drop the wrong rows after a single-file delete, where the
                // removed file may well have been the keeper.
                let gone: HashSet<&PathBuf> = report.deleted_paths.iter().collect();
                for group in &mut self.groups {
                    group.files.retain(|f| !gone.contains(&f.path));
                }
                // A lone remaining copy is not a duplicate any more.
                self.groups.retain(|g| g.files.len() > 1);
                // Deleting a keeper leaves the group with none, so restore the
                // one-keeper invariant rather than letting it drift.
                for group in &mut self.groups {
                    if group.keeper_count() == 0 {
                        group.apply_strategy(KeepStrategy::First);
                    }
                }
                self.clamp_selection();
                self.plan = DeletePlan::Marked;
                // Groups have disappeared; a stale selection would silently
                // scope the next action to whatever survived.
                self.selected.clear();
                self.end_extension();

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
                    .is_some_and(|r| r.kind == RowKind::Parent)
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
            KeyCode::Char('5') => self.options.use_cache = !self.options.use_cache,
            KeyCode::Char('R') => self.toggle_reference_target(),
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
                // Clearing a selection is the more likely intent than leaving.
                if self.selected.is_empty() {
                    self.screen = Screen::Picker;
                    self.refresh_entries();
                } else {
                    self.selected.clear();
                    self.end_extension();
                    self.status = Some("Selection cleared".to_string());
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.end_extension();
                self.pane = match self.pane {
                    Pane::Groups => Pane::Files,
                    Pane::Files => Pane::Groups,
                };
            }
            // Shift+arrow extends the block; a bare arrow ends it. K/J are a
            // fallback because not every terminal reports Shift+arrow.
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.extend_selection(1)
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => self.extend_selection(-1),
            KeyCode::Char('J') => self.extend_selection(1),
            KeyCode::Char('K') => self.extend_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.end_extension();
                self.move_selection(1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.end_extension();
                self.move_selection(-1)
            }
            KeyCode::PageDown => {
                self.end_extension();
                self.move_selection(10)
            }
            KeyCode::PageUp => {
                self.end_extension();
                self.move_selection(-10)
            }
            KeyCode::Home => {
                self.end_extension();
                self.set_selection(0)
            }
            KeyCode::End => {
                self.end_extension();
                self.set_selection(usize::MAX)
            }
            KeyCode::Char('m') => self.toggle_selection(),
            KeyCode::Char('a') => self.toggle_select_all(),
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
                let protected = self
                    .groups
                    .get(self.group_selected)
                    .and_then(|group| group.files.get(idx))
                    .is_some_and(|file| file.protected);
                let refused = if protected {
                    true
                } else {
                    match self.groups.get_mut(self.group_selected) {
                        Some(group) => !group.toggle_mark(idx),
                        None => false,
                    }
                };
                if protected {
                    self.status = Some("Reference files are protected from deletion".to_string());
                } else if refused {
                    self.status = Some("At least one copy in a group must be kept".to_string());
                }
                self.pane = Pane::Files;
            }
            KeyCode::Char('x') => self.toggle_skip_in_scope(),
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
                    self.plan = DeletePlan::Marked;
                    self.screen = Screen::Confirm;
                }
            }
            // Delete just the highlighted copy, whatever its mark. Useful when
            // one file is obviously junk and the rest need no decision.
            KeyCode::Delete => match self.groups.get(self.group_selected) {
                Some(group)
                    if group
                        .files
                        .get(self.file_selected)
                        .is_some_and(|file| file.protected) =>
                {
                    self.status = Some("Reference files are protected from deletion".to_string());
                }
                Some(group) if group.files.len() > 1 => {
                    self.pane = Pane::Files;
                    self.plan = DeletePlan::Single {
                        group: self.group_selected,
                        file: self.file_selected,
                    };
                    self.screen = Screen::Confirm;
                }
                Some(_) => {
                    self.status =
                        Some("This is the last copy of the file - nothing to delete".to_string());
                }
                None => {}
            },
            KeyCode::Char('r') => {
                if let Some(root) = self.scan_root.clone() {
                    self.start_scan(root);
                }
            }
            KeyCode::Char('e') => self.export_results(),
            _ => {}
        }
    }

    fn toggle_reference_target(&mut self) {
        let target = self.scan_target();
        let Ok(target) = target.canonicalize() else {
            self.status = Some(format!("Cannot resolve reference {}", target.display()));
            return;
        };
        if let Some(index) = self
            .options
            .reference_roots
            .iter()
            .position(|root| root == &target)
        {
            self.options.reference_roots.remove(index);
            self.status = Some(format!("Unprotected {}", target.display()));
            return;
        }
        if let Some(parent) = self
            .options
            .reference_roots
            .iter()
            .find(|root| target.starts_with(root.as_path()))
        {
            self.status = Some(format!("Already protected by {}", parent.display()));
            return;
        }
        self.options
            .reference_roots
            .retain(|root| !root.starts_with(&target));
        self.options.reference_roots.push(target.clone());
        self.options.reference_roots.sort();
        self.status = Some(format!("Protected {}", target.display()));
    }

    fn export_results(&mut self) {
        let Some(root) = self.scan_root.as_deref() else {
            self.status = Some("Nothing has been scanned yet".to_string());
            return;
        };
        let outcome = export::write_results(
            &self.export_dir,
            root,
            &self.options,
            self.elapsed(),
            &self.groups,
            &self.selected,
        );
        self.status = Some(match (outcome.json, outcome.text) {
            (Ok(json), Ok(text)) => format!(
                "Exported {} and {}",
                json.file_name().unwrap_or_default().to_string_lossy(),
                text.file_name().unwrap_or_default().to_string_lossy()
            ),
            (Ok(path), Err(err)) => format!(
                "Exported {}; text export failed: {err}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            (Err(err), Ok(path)) => format!(
                "JSON export failed: {err}; exported {}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ),
            (Err(json), Err(text)) => format!("Export failed: JSON: {json}; text: {text}"),
        });
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
            KeyCode::Char('e') => self.export_results(),
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

    /// Progress of the finalize phase, which has its own denominator: the files
    /// in surviving groups, not the candidates that were hashed.
    pub fn finalize_progress(&self) -> (u64, u64) {
        scan::checked_of(&self.scan_state)
    }

    pub fn scanned_bytes(&self) -> u64 {
        self.scan_state.bytes_seen.load(Ordering::Relaxed)
    }

    pub fn scanned_files(&self) -> u64 {
        self.scan_state.files_seen.load(Ordering::Relaxed)
    }
}

/// Whether the browser should hide this entry.
///
/// The dotfile convention covers Unix. On Windows it misses the hidden
/// attribute, which is how a drive root ends up listing `$Recycle.Bin`,
/// `System Volume Information`, `pagefile.sys` and `hiberfil.sys`.
fn is_hidden(name: &str, entry: &std::fs::DirEntry) -> bool {
    if name.starts_with('.') {
        return true;
    }
    hidden_by_attribute(entry)
}

/// Only `HIDDEN`, not `SYSTEM`: everything a drive root clutters the list with is
/// hidden, and testing `SYSTEM` too would suppress directories a user may want.
#[cfg(windows)]
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;

/// Split out from the metadata lookup so the bit test is unit-testable on any
/// platform, not only where the attribute exists.
#[cfg(windows)]
pub fn attrs_are_hidden(attrs: u32) -> bool {
    attrs & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(windows)]
fn hidden_by_attribute(entry: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    // Windows fills this in from the directory scan, so it costs no extra I/O.
    entry
        .metadata()
        .map(|m| attrs_are_hidden(m.file_attributes()))
        .unwrap_or(false)
}

/// Unix has no hidden attribute; the dotfile rule is the whole convention, so
/// this avoids a per-entry `stat` that would buy nothing.
#[cfg(not(windows))]
fn hidden_by_attribute(_entry: &std::fs::DirEntry) -> bool {
    false
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

    /// Index of the row with this name, so picker tests never hard-code a
    /// position. A wrong index would assert against the wrong row in silence.
    fn row(app: &App, name: &str) -> usize {
        app.entries
            .iter()
            .position(|r| r.name == name)
            .unwrap_or_else(|| panic!("no row named {name:?}"))
    }

    /// The names of the rows that are directory contents, skipping the
    /// current-directory, parent and drive rows, which are navigation.
    fn content_names(app: &App) -> Vec<&str> {
        app.entries
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Directory | RowKind::File))
            .map(|r| r.name.as_str())
            .collect()
    }

    /// `n` groups of two copies each, with distinct hashes and descending sizes
    /// so the default sort order is stable and predictable.
    fn app_with_results_of(n: usize) -> App {
        let mut app = App::new(
            std::env::temp_dir(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.scan_root = Some(PathBuf::from("/scan/root"));
        app.groups = (0..n)
            .map(|i| {
                let size = ((n - i) * 1000) as u64;
                DupeGroup::new(
                    [i as u8 + 1; 32],
                    size,
                    vec![
                        entry(&format!("/g{i}/a.bin"), size, Some(100)),
                        entry(&format!("/g{i}/b.bin"), size, Some(200)),
                    ],
                )
            })
            .collect();
        app.screen = Screen::Results;
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
        // Everything that was marked actually went.
        let gone: Vec<PathBuf> = delete::pending_in(app.groups.iter())
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        app.handle_delete_msg(DeleteMsg::Done(DeleteReport {
            deleted: gone.len() as u64,
            bytes_freed: 2_050,
            failures: Vec::new(),
            mode_label: "Trash".into(),
            deleted_paths: gone,
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
        // The second copy of group 0 could not be removed; the third could.
        let stuck = app.groups[0].files[1].path.clone();
        let went = app.groups[0].files[2].path.clone();
        app.handle_delete_msg(DeleteMsg::Done(DeleteReport {
            deleted: 1,
            bytes_freed: 1_000,
            failures: vec![DeleteFailure {
                path: stuck.clone(),
                message: "permission denied".into(),
            }],
            mode_label: "Trash".into(),
            deleted_paths: vec![went.clone()],
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
        let removed = app
            .groups
            .iter()
            .flat_map(|g| g.files.iter())
            .all(|f| f.path != went);
        assert!(
            removed,
            "the file that was deleted must be gone from the list"
        );
    }

    #[test]
    fn a_skipped_group_survives_deletion_intact() {
        let mut app = app_with_results();
        app.groups[0].skipped = true;
        let before = app.groups[0].files.len();
        // Only the other group's marked copy was deleted.
        let gone: Vec<PathBuf> = delete::pending_in(app.groups.iter())
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert!(
            gone.iter()
                .all(|p| !p.starts_with("/aaa") && p != &PathBuf::from("/b.jpg")),
            "a skipped group contributes no targets"
        );
        app.handle_delete_msg(DeleteMsg::Done(DeleteReport {
            deleted: gone.len() as u64,
            bytes_freed: 50,
            failures: Vec::new(),
            mode_label: "Trash".into(),
            deleted_paths: gone,
        }));
        let group = app
            .groups
            .iter()
            .find(|g| g.hash == [1u8; 32])
            .expect("the skipped group should still be listed");
        assert_eq!(group.files.len(), before);
    }

    // Single-file delete: the files pane needs a way to remove exactly the
    // highlighted copy, without unmarking everything else first.
    #[test]
    fn delete_key_targets_only_the_highlighted_file() {
        let mut app = app_with_results();
        app.pane = Pane::Files;
        app.file_selected = 2;
        let target = app.groups[0].files[2].path.clone();

        press(&mut app, KeyCode::Delete);
        assert_eq!(app.screen, Screen::Confirm);
        assert_eq!(
            app.plan,
            DeletePlan::Single { group: 0, file: 2 },
            "the plan should name the highlighted file"
        );

        let targets = app.planned_targets();
        assert_eq!(targets.len(), 1, "exactly one file");
        assert_eq!(targets[0].0, target);
    }

    #[test]
    fn delete_key_can_remove_a_copy_that_is_the_keeper() {
        let mut app = app_with_results();
        app.pane = Pane::Files;
        // Index 0 is the keeper after the default strategy.
        app.file_selected = 0;
        assert!(app.groups[0].files[0].keep);

        press(&mut app, KeyCode::Delete);
        let targets = app.planned_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].0, app.groups[0].files[0].path,
            "the user may delete the keeper if that is what they highlighted"
        );
    }

    #[test]
    fn deleting_the_keeper_leaves_the_group_with_a_new_one() {
        let mut app = app_with_results();
        let keeper = app.groups[0].files[0].path.clone();
        assert!(app.groups[0].files[0].keep);

        app.plan = DeletePlan::Single { group: 0, file: 0 };
        app.handle_delete_msg(DeleteMsg::Done(DeleteReport {
            deleted: 1,
            bytes_freed: 1_000,
            failures: Vec::new(),
            mode_label: "Trash".into(),
            deleted_paths: vec![keeper.clone()],
        }));

        let group = app
            .groups
            .iter()
            .find(|g| g.hash == [1u8; 32])
            .expect("two copies remain, so the group stands");
        assert!(
            group.files.iter().all(|f| f.path != keeper),
            "keeper removed"
        );
        assert_eq!(
            group.keeper_count(),
            1,
            "the one-keeper invariant must be restored, not left at zero"
        );
    }

    #[test]
    fn a_single_delete_that_empties_a_group_drops_the_group() {
        let mut app = app_with_results();
        // Group 1 has exactly two copies; removing one leaves no duplicate.
        let victim = app.groups[1].files[0].path.clone();
        app.handle_delete_msg(DeleteMsg::Done(DeleteReport {
            deleted: 1,
            bytes_freed: 50,
            failures: Vec::new(),
            mode_label: "Trash".into(),
            deleted_paths: vec![victim],
        }));
        assert!(
            !app.groups.iter().any(|g| g.hash == [2u8; 32]),
            "a lone remaining copy is not a duplicate any more"
        );
    }

    #[test]
    fn delete_key_refuses_when_only_one_copy_is_left() {
        let mut app = app_with_results();
        app.groups[0].files.truncate(1);
        app.group_selected = 0;
        app.pane = Pane::Files;
        app.file_selected = 0;

        press(&mut app, KeyCode::Delete);
        assert_eq!(app.screen, Screen::Results, "no confirmation should open");
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains("last copy"),
            "the user should be told why: {:?}",
            app.status
        );
    }

    #[test]
    fn delete_key_on_an_empty_result_set_is_harmless() {
        let mut app = app_with_results();
        app.groups.clear();
        press(&mut app, KeyCode::Delete);
        assert_eq!(app.screen, Screen::Results);
    }

    #[test]
    fn escaping_a_single_delete_confirmation_deletes_nothing() {
        let mut app = app_with_results();
        app.pane = Pane::Files;
        app.file_selected = 1;
        press(&mut app, KeyCode::Delete);
        assert_eq!(app.screen, Screen::Confirm);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.screen, Screen::Results);
        assert_eq!(app.groups[0].files.len(), 3, "nothing removed");
    }

    #[test]
    fn the_plan_resets_to_marked_after_a_single_delete() {
        let mut app = app_with_results();
        let victim = app.groups[0].files[1].path.clone();
        app.plan = DeletePlan::Single { group: 0, file: 1 };
        app.handle_delete_msg(DeleteMsg::Done(DeleteReport {
            deleted: 1,
            bytes_freed: 1_000,
            failures: Vec::new(),
            mode_label: "Trash".into(),
            deleted_paths: vec![victim],
        }));
        assert_eq!(
            app.plan,
            DeletePlan::Marked,
            "a later D must not reuse the stale single-file plan"
        );
    }

    #[test]
    fn bulk_delete_still_plans_every_marked_copy() {
        let mut app = app_with_results();
        press(&mut app, KeyCode::Char('D'));
        assert_eq!(app.plan, DeletePlan::Marked);
        assert_eq!(app.planned_targets().len(), app.total_marked());
    }

    // ------------------------------------------------------ group selection

    /// The selection is keyed by content hash, not row index: sorting reorders
    /// the list, and an index-keyed set would silently point at other groups.
    #[test]
    fn a_selection_survives_re_sorting() {
        let mut app = app_with_results();
        app.group_selected = 0;
        press(&mut app, KeyCode::Char('m'));
        let picked = app.groups[0].hash;
        assert!(app.selected.contains(&picked));

        // Cycle through every sort order; the same group stays selected.
        for _ in 0..4 {
            press(&mut app, KeyCode::Char('s'));
            assert_eq!(app.selected.len(), 1, "selection size must not change");
            assert!(
                app.selected.contains(&picked),
                "the same group must stay selected after sorting"
            );
            let idx = app.groups.iter().position(|g| g.hash == picked).unwrap();
            assert!(app.is_selected(idx));
        }
    }

    #[test]
    fn m_toggles_a_group_in_and_out() {
        let mut app = app_with_results();
        press(&mut app, KeyCode::Char('m'));
        assert_eq!(app.selected_count(), 1);
        press(&mut app, KeyCode::Char('m'));
        assert_eq!(app.selected_count(), 0);
    }

    #[test]
    fn a_selects_all_then_clears() {
        let mut app = app_with_results();
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.selected_count(), app.groups.len());
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.selected_count(), 0);
    }

    #[test]
    fn shift_down_extends_a_contiguous_block() {
        let mut app = app_with_results_of(5);
        app.group_selected = 1;
        for _ in 0..2 {
            app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        }
        assert_eq!(app.group_selected, 3);
        assert_eq!(app.selected_count(), 3, "rows 1..=3");
        for idx in 1..=3 {
            assert!(app.is_selected(idx), "row {idx} should be selected");
        }
        assert!(!app.is_selected(0) && !app.is_selected(4));
    }

    /// Reversing direction must shrink the block, not leave rows behind.
    #[test]
    fn reversing_a_shift_extension_shrinks_the_block() {
        let mut app = app_with_results_of(5);
        app.group_selected = 0;
        for _ in 0..3 {
            app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        }
        assert_eq!(app.selected_count(), 4, "rows 0..=3");

        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(
            app.selected_count(),
            3,
            "coming back up must deselect the row we left"
        );
        assert!(!app.is_selected(3), "row 3 should no longer be selected");
    }

    #[test]
    fn shift_extension_preserves_an_earlier_manual_selection() {
        let mut app = app_with_results_of(5);
        app.group_selected = 4;
        press(&mut app, KeyCode::Char('m')); // mark the last row
        let manual = app.groups[4].hash;

        app.group_selected = 0;
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        assert!(
            app.selected.contains(&manual),
            "an extension must not discard what was already marked"
        );
        assert_eq!(app.selected_count(), 3, "rows 0..=1 plus the manual row 4");
    }

    #[test]
    fn jk_extend_the_block_like_shift_arrows() {
        let mut app = app_with_results_of(5);
        app.group_selected = 0;
        press(&mut app, KeyCode::Char('J'));
        press(&mut app, KeyCode::Char('J'));
        assert_eq!(app.selected_count(), 3);
        press(&mut app, KeyCode::Char('K'));
        assert_eq!(app.selected_count(), 2, "K shrinks it again");
    }

    #[test]
    fn a_plain_arrow_ends_the_block() {
        let mut app = app_with_results_of(5);
        app.group_selected = 0;
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        assert_eq!(app.selected_count(), 2);

        press(&mut app, KeyCode::Down); // ends the extension
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        // A new anchor started at row 2, so rows 2..=3 join the existing two.
        assert!(app.is_selected(2) && app.is_selected(3));
        assert_eq!(app.selected_count(), 4);
    }

    #[test]
    fn escape_clears_a_selection_before_leaving_the_screen() {
        let mut app = app_with_results();
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.selected_count(), 0);
        assert_eq!(app.screen, Screen::Results, "first Esc only clears");

        press(&mut app, KeyCode::Esc);
        assert_eq!(app.screen, Screen::Picker, "second Esc leaves");
    }

    // --------------------------------------------------- selection scoping

    /// The whole point: a batch delete must not touch groups outside the
    /// selection. The user's case is a DLL shared by two applications.
    #[test]
    fn a_batch_delete_ignores_unselected_groups() {
        let mut app = app_with_results_of(3);
        // Select groups 0 and 2, leaving 1 alone.
        app.group_selected = 0;
        press(&mut app, KeyCode::Char('m'));
        app.group_selected = 2;
        press(&mut app, KeyCode::Char('m'));

        let protected: Vec<PathBuf> = app.groups[1].files.iter().map(|f| f.path.clone()).collect();
        let targets = app.planned_targets();

        assert!(!targets.is_empty(), "the selected groups still have marks");
        for (path, _) in &targets {
            assert!(
                !protected.contains(path),
                "{path:?} is in an unselected group and must be spared"
            );
        }
    }

    /// The header number must equal what D will act on, or it misleads about an
    /// irreversible action.
    #[test]
    fn the_totals_match_the_scoped_target_list() {
        let mut app = app_with_results_of(4);
        assert_eq!(app.total_marked(), app.planned_targets().len());

        app.group_selected = 1;
        press(&mut app, KeyCode::Char('m'));

        let targets = app.planned_targets();
        assert_eq!(app.total_marked(), targets.len());
        assert_eq!(
            app.total_reclaimable(),
            targets.iter().map(|(_, s)| s).sum::<u64>()
        );
        assert_eq!(targets.len(), 1, "one group of two copies, one marked");
    }

    #[test]
    fn an_empty_selection_still_covers_every_group() {
        let mut app = app_with_results_of(4);
        let all_marked = app.total_marked();
        let all_bytes = app.total_reclaimable();
        let all_targets = app.planned_targets().len();

        // Select then clear: the scope must return to everything.
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.selected_count(), 0);
        assert_eq!(app.total_marked(), all_marked);
        assert_eq!(app.total_reclaimable(), all_bytes);
        assert_eq!(app.planned_targets().len(), all_targets);
    }

    #[test]
    fn a_keeper_strategy_only_touches_selected_groups() {
        let mut app = app_with_results_of(3);
        app.group_selected = 0;
        press(&mut app, KeyCode::Char('m'));

        // Group 1 is unselected; note which copy it keeps.
        let untouched = app.groups[1]
            .files
            .iter()
            .position(|f| f.keep)
            .expect("a keeper exists");

        press(&mut app, KeyCode::Char('2')); // keep newest, in scope only

        assert_eq!(
            app.groups[1].files.iter().position(|f| f.keep),
            Some(untouched),
            "an unselected group must keep the copy it had"
        );
        // The selected group followed the strategy (b.bin is newer).
        assert_eq!(
            app.groups[0].files.iter().position(|f| f.keep),
            Some(1),
            "the selected group should now keep the newest copy"
        );
    }

    #[test]
    fn x_skips_every_selected_group_at_once() {
        let mut app = app_with_results_of(4);
        app.group_selected = 0;
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
        assert_eq!(app.selected_count(), 3);

        press(&mut app, KeyCode::Char('x'));
        for idx in 0..3 {
            assert!(app.groups[idx].skipped, "group {idx} should be skipped");
        }
        assert!(!app.groups[3].skipped, "the unselected group is untouched");

        // A second press moves them all back, rather than flipping each.
        press(&mut app, KeyCode::Char('x'));
        assert!(app.groups[..3].iter().all(|g| !g.skipped));
    }

    #[test]
    fn a_mixed_selection_skips_rather_than_flipping() {
        let mut app = app_with_results_of(3);
        app.groups[0].skipped = true;
        app.selected = app.groups.iter().map(|g| g.hash).collect();

        // One is already skipped: the whole selection should end up skipped,
        // not have that one toggled back on.
        press(&mut app, KeyCode::Char('x'));
        assert!(
            app.groups.iter().all(|g| g.skipped),
            "a mixed selection resolves to all-skipped"
        );
    }

    #[test]
    fn scoping_never_leaves_a_group_without_a_keeper() {
        let mut app = app_with_results_of(4);
        press(&mut app, KeyCode::Char('a'));
        for code in ['1', '2', '3', '4'] {
            press(&mut app, KeyCode::Char(code));
            for (i, g) in app.groups.iter().enumerate() {
                assert_eq!(g.keeper_count(), 1, "group {i} after {code}");
            }
        }
    }

    #[test]
    fn a_completed_delete_clears_the_selection() {
        let mut app = app_with_results_of(3);
        press(&mut app, KeyCode::Char('a'));
        assert!(app.selected_count() > 0);

        let gone = vec![app.groups[0].files[1].path.clone()];
        app.handle_delete_msg(DeleteMsg::Done(DeleteReport {
            deleted: 1,
            bytes_freed: 10,
            failures: Vec::new(),
            mode_label: "Trash".into(),
            deleted_paths: gone,
        }));
        assert_eq!(
            app.selected_count(),
            0,
            "a stale selection would silently scope the next action"
        );
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
        assert_eq!(
            content_names(&app),
            vec!["alpha", "beta", "a-file.txt"],
            "directories before files, each group sorted"
        );
        // Navigation rows lead the list: the scan-this-directory row, then `..`.
        assert_eq!(
            app.entries
                .iter()
                .map(|r| r.kind)
                .take(2)
                .collect::<Vec<_>>(),
            vec![RowKind::Current, RowKind::Parent]
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
        assert_eq!(file.kind, RowKind::File);
        assert_eq!(file.size, Some(4096));

        let sub = app.entries.iter().find(|e| e.name == "sub").unwrap();
        assert_eq!(sub.kind, RowKind::Directory);
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
        assert_eq!(content_names(&app), vec!["zzz", "aaa.txt"]);
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
        app.picker_selected = row(&app, "child");
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
        app.picker_selected = row(&app, "beta");
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
        // Rows: scan-this-directory, "..", then the two subdirectories.
        app.picker_selected = row(&app, "one");
        assert_eq!(app.scan_target_label(), "one");
        press(&mut app, KeyCode::Down);
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
        assert!(rows.iter().all(|r| r.kind == RowKind::Drive));
        assert!(rows.iter().all(|r| r.is_navigable()));
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
        // The top of a tree is `/` on Unix but a drive root such as `C:\` on
        // Windows, so assert the property rather than a literal separator.
        assert!(
            app.cwd.parent().is_none(),
            "d should land on a root, got {:?}",
            app.cwd
        );
        assert!(
            nested.starts_with(&app.cwd),
            "the root should be an ancestor of where we started"
        );
        assert_eq!(app.picker_selected, 0);
    }

    /// The Windows attribute lookup cannot run here, but the bit test can, so
    /// the predicate itself is covered rather than left to CI compilation alone.
    #[cfg(windows)]
    #[test]
    fn hidden_attribute_bits_are_read_correctly() {
        const NORMAL: u32 = 0x80;
        const HIDDEN: u32 = 0x2;
        const SYSTEM: u32 = 0x4;
        const DIRECTORY: u32 = 0x10;

        assert!(attrs_are_hidden(HIDDEN), "a hidden file is hidden");
        assert!(
            attrs_are_hidden(HIDDEN | SYSTEM | DIRECTORY),
            "$Recycle.Bin and System Volume Information are hidden+system dirs"
        );
        assert!(!attrs_are_hidden(NORMAL), "an ordinary file is not hidden");
        assert!(
            !attrs_are_hidden(SYSTEM | DIRECTORY),
            "system-only is deliberately NOT hidden: that would suppress \
             directories a user may want"
        );
        assert!(!attrs_are_hidden(0), "no attributes means not hidden");
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
        // Derive a genuine root: `/` on Unix, the drive root on Windows.
        app.cwd = std::env::temp_dir()
            .ancestors()
            .last()
            .expect("every path has a topmost ancestor")
            .to_path_buf();
        assert!(app.cwd.parent().is_none(), "precondition: {:?}", app.cwd);
        app.refresh_entries();
        assert!(
            !app.entries.iter().any(|e| e.kind == RowKind::Parent),
            "the root has no parent to offer"
        );
        // But it must still offer itself, or the drive is unscannable.
        assert_eq!(app.entries.first().map(|e| e.kind), Some(RowKind::Current));
    }

    // ------------------------------------------- scanning where you already are

    /// The reported bug: at a drive root there is no `..`, so if the directory
    /// holds no files nothing denoted the drive and it could not be scanned.
    /// Built by hand because a parentless directory only exists at `/` on Linux,
    /// which does contain files.
    #[test]
    fn a_parentless_directory_with_no_files_is_still_scannable() {
        let mut app = App::new(
            std::env::temp_dir(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        let drive = PathBuf::from("C:\\");
        app.cwd = drive.clone();
        // Exactly what a bare Windows drive root looks like: no parent row, no
        // files, only subdirectories.
        app.entries = vec![
            DirEntryRow {
                name: drive.to_string_lossy().into_owned(),
                path: drive.clone(),
                kind: RowKind::Current,
                size: None,
            },
            DirEntryRow {
                name: "Windows".into(),
                path: drive.join("Windows"),
                kind: RowKind::Directory,
                size: None,
            },
        ];

        assert!(
            !app.entries.iter().any(|r| r.kind == RowKind::Parent),
            "precondition: a drive root has no parent row"
        );
        assert!(
            !app.entries.iter().any(|r| r.kind == RowKind::File),
            "precondition: no file row to fall back on"
        );

        app.picker_selected = 0;
        assert_eq!(
            app.scan_target(),
            drive,
            "the first row must target the drive itself"
        );
    }

    /// Every kind spelled out, so the match cannot quietly regress into a
    /// catch-all -- which is what caused the bug above.
    #[test]
    fn scan_target_is_defined_for_every_row_kind() {
        let mut app = App::new(
            std::env::temp_dir(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        let cwd = PathBuf::from("/scan/here");
        let parent = PathBuf::from("/scan");
        app.cwd = cwd.clone();
        app.entries = vec![
            DirEntryRow {
                name: "cur".into(),
                path: cwd.clone(),
                kind: RowKind::Current,
                size: None,
            },
            DirEntryRow {
                name: "..".into(),
                path: parent.clone(),
                kind: RowKind::Parent,
                size: None,
            },
            DirEntryRow {
                name: "D:\\".into(),
                path: PathBuf::from("D:\\"),
                kind: RowKind::Drive,
                size: None,
            },
            DirEntryRow {
                name: "sub".into(),
                path: cwd.join("sub"),
                kind: RowKind::Directory,
                size: None,
            },
            DirEntryRow {
                name: "f.bin".into(),
                path: cwd.join("f.bin"),
                kind: RowKind::File,
                size: Some(1),
            },
        ];

        let expected = [
            (RowKind::Current, cwd.clone()),
            // The parent row denotes the parent, so that is what it scans.
            (RowKind::Parent, parent),
            (RowKind::Drive, PathBuf::from("D:\\")),
            (RowKind::Directory, cwd.join("sub")),
            // A file cannot be scanned; its containing directory is the target.
            (RowKind::File, cwd.clone()),
        ];
        for (idx, (kind, want)) in expected.iter().enumerate() {
            app.picker_selected = idx;
            assert_eq!(app.scan_target(), *want, "for {kind:?}");
        }
    }

    #[test]
    fn the_action_row_is_first_in_every_listing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        assert_eq!(app.entries.first().map(|r| r.kind), Some(RowKind::Current));
        assert_eq!(app.picker_selected, 0, "and starts highlighted");
        assert_eq!(app.scan_target(), dir.path());

        // Still first after descending.
        app.picker_selected = row(&app, "sub");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.entries.first().map(|r| r.kind), Some(RowKind::Current));
        assert_eq!(app.scan_target(), dir.path().join("sub"));
    }

    #[test]
    fn enter_on_the_action_row_goes_nowhere() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.picker_selected = 0;
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.cwd, dir.path(), "there is nothing to descend into");
        assert_eq!(app.screen, Screen::Picker);
    }

    #[test]
    fn s_on_the_action_row_scans_the_current_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.picker_selected = 0;
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.screen, Screen::Scanning);
        assert_eq!(app.scan_root.as_deref(), Some(dir.path()));
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
        assert!(!app.options.use_cache);
        press(&mut app, KeyCode::Char('5'));
        assert!(app.options.use_cache);
    }

    #[test]
    fn the_picker_can_toggle_a_reference_directory() {
        let dir = tempfile::tempdir().unwrap();
        let reference = dir.path().join("reference");
        std::fs::create_dir(&reference).unwrap();
        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.picker_selected = app
            .entries
            .iter()
            .position(|row| row.path == reference)
            .unwrap();

        press(&mut app, KeyCode::Char('R'));
        assert_eq!(
            app.options.reference_roots,
            vec![reference.canonicalize().unwrap()]
        );
        press(&mut app, KeyCode::Char('R'));
        assert!(app.options.reference_roots.is_empty());
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
        assert!(app.entries.iter().any(|e| e.kind == RowKind::Parent));
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
