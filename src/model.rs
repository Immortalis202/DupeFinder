//! Core data types shared by the scanner, the deleter and the UI.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

/// One file on disk belonging to a duplicate group.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// `false` means this copy is marked for deletion.
    pub keep: bool,
}

impl FileEntry {
    pub fn new(path: PathBuf, size: u64, modified: Option<SystemTime>) -> Self {
        Self {
            path,
            size,
            modified,
            keep: false,
        }
    }

    /// Lossy file name; paths are not guaranteed to be UTF-8 on either platform.
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }
}

/// Which copy to keep when applying a bulk strategy to every group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepStrategy {
    /// Keep the first entry as listed. This is the "quick select" default.
    First,
    Newest,
    Oldest,
    ShortestPath,
}

impl KeepStrategy {
    pub fn label(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::Newest => "newest",
            Self::Oldest => "oldest",
            Self::ShortestPath => "shortest path",
        }
    }
}

/// Two or more files with byte-identical content.
#[derive(Debug, Clone)]
pub struct DupeGroup {
    pub hash: [u8; 32],
    /// Size of a single file; identical across the whole group.
    pub size: u64,
    pub files: Vec<FileEntry>,
    /// When set, nothing in this group is deleted regardless of the marks.
    pub skipped: bool,
}

impl DupeGroup {
    pub fn new(hash: [u8; 32], size: u64, files: Vec<FileEntry>) -> Self {
        let mut group = Self {
            hash,
            size,
            files,
            skipped: false,
        };
        group.apply_strategy(KeepStrategy::First);
        group
    }

    /// Bytes that would be freed by keeping exactly one copy.
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len() as u64).saturating_sub(1)
    }

    /// Number of copies currently marked for deletion.
    pub fn marked(&self) -> usize {
        if self.skipped {
            return 0;
        }
        self.files.iter().filter(|f| !f.keep).count()
    }

    /// Bytes that would be freed by the current marks.
    pub fn reclaimable(&self) -> u64 {
        self.size * self.marked() as u64
    }

    pub fn keeper_count(&self) -> usize {
        self.files.iter().filter(|f| f.keep).count()
    }

    /// Make `idx` the sole keeper.
    pub fn keep_only(&mut self, idx: usize) {
        if idx >= self.files.len() {
            return;
        }
        for (i, f) in self.files.iter_mut().enumerate() {
            f.keep = i == idx;
        }
    }

    /// Flip the mark on one file.
    ///
    /// Refuses to clear the last keeper and returns `false` in that case, so
    /// "delete every copy of a file" is unrepresentable rather than merely
    /// discouraged.
    pub fn toggle_mark(&mut self, idx: usize) -> bool {
        let Some(file) = self.files.get(idx) else {
            return false;
        };
        if file.keep && self.keeper_count() <= 1 {
            return false;
        }
        self.files[idx].keep = !self.files[idx].keep;
        true
    }

    /// Choose the keeper for this group according to `strategy`.
    pub fn apply_strategy(&mut self, strategy: KeepStrategy) {
        if self.files.is_empty() {
            return;
        }
        let idx = match strategy {
            KeepStrategy::First => 0,
            KeepStrategy::Newest => self.extreme_by_time(true),
            KeepStrategy::Oldest => self.extreme_by_time(false),
            KeepStrategy::ShortestPath => self
                .files
                .iter()
                .enumerate()
                .min_by_key(|(i, f)| (f.path.as_os_str().len(), *i))
                .map(|(i, _)| i)
                .unwrap_or(0),
        };
        self.keep_only(idx);
    }

    /// Index of the newest (or oldest) file. Entries without a timestamp never
    /// win, so a group with no timestamps at all falls back to the first entry.
    fn extreme_by_time(&self, newest: bool) -> usize {
        let mut best: Option<(usize, SystemTime)> = None;
        for (i, f) in self.files.iter().enumerate() {
            let Some(t) = f.modified else { continue };
            let better = match best {
                None => true,
                Some((_, bt)) => {
                    if newest {
                        t > bt
                    } else {
                        t < bt
                    }
                }
            };
            if better {
                best = Some((i, t));
            }
        }
        best.map(|(i, _)| i).unwrap_or(0)
    }

    /// Short hex prefix of the content hash, for display.
    pub fn hash_prefix(&self) -> String {
        self.hash[..6].iter().fold(String::new(), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    /// A representative name for the group, used in the group list.
    pub fn label(&self) -> String {
        self.files
            .first()
            .map(FileEntry::file_name)
            .unwrap_or_default()
    }
}

/// How the results list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    /// Bytes wasted by the group: most impactful first.
    Wasted,
    Size,
    Count,
    Name,
}

impl SortKey {
    pub fn next(self) -> Self {
        match self {
            Self::Wasted => Self::Size,
            Self::Size => Self::Count,
            Self::Count => Self::Name,
            Self::Name => Self::Wasted,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Wasted => "wasted",
            Self::Size => "size",
            Self::Count => "count",
            Self::Name => "name",
        }
    }
}

/// Filters applied while walking the tree.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub skip_hidden: bool,
    pub respect_gitignore: bool,
    /// Every empty file is trivially identical to every other, which would
    /// produce one enormous useless group.
    pub skip_empty: bool,
    /// Report two paths pointing at the same physical file as one entry;
    /// deleting one of them would free nothing.
    pub collapse_hardlinks: bool,
    pub same_file_system: bool,
    pub follow_links: bool,
    pub min_size: u64,
    /// Extensions to leave out entirely, lower-case and without the dot.
    ///
    /// Duplicate shared libraries are usually deliberate -- two applications
    /// shipping the same DLL -- so removing them breaks things rather than
    /// reclaiming space. Excluding them keeps the results actionable instead of
    /// making you skip the same groups on every scan.
    pub exclude_exts: Vec<String>,
}

impl ScanOptions {
    /// Whether this path is excluded by extension. Case-insensitive, because
    /// `FOO.DLL` and `foo.dll` are the same file to Windows.
    pub fn excluded_by_extension(&self, path: &std::path::Path) -> bool {
        if self.exclude_exts.is_empty() {
            return false;
        }
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) => {
                let ext = ext.to_lowercase();
                self.exclude_exts.contains(&ext)
            }
            // No extension, or one that is not valid UTF-8: nothing to match.
            None => false,
        }
    }
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            skip_hidden: true,
            respect_gitignore: true,
            skip_empty: true,
            collapse_hardlinks: true,
            same_file_system: false,
            follow_links: false,
            min_size: 0,
            exclude_exts: Vec::new(),
        }
    }
}

/// Which stage the scanner is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Walking,
    Pruning,
    HeadHashing,
    FullHashing,
    Finalizing,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Walking => "Walking the tree",
            Self::Pruning => "Grouping by size",
            Self::HeadHashing => "Hashing file heads",
            Self::FullHashing => "Hashing full contents",
            Self::Finalizing => "Finalizing",
        }
    }
}

/// A non-fatal problem with one path. The scan always continues.
#[derive(Debug, Clone)]
pub struct ScanError {
    pub path: Option<PathBuf>,
    pub message: String,
}

impl ScanError {
    pub fn new(path: Option<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            path,
            message: message.into(),
        }
    }

    pub fn display(&self) -> String {
        match &self.path {
            Some(p) => format!("{}: {}", p.to_string_lossy(), self.message),
            None => self.message.clone(),
        }
    }
}

/// Messages the scanner thread sends to the UI. Only stage transitions and the
/// final result travel over the channel; the live counters live in [`ScanState`]
/// so that a fast walk cannot flood the channel.
#[derive(Debug)]
pub enum ScanMsg {
    Phase(Phase),
    Error(ScanError),
    Done(Vec<DupeGroup>),
    Cancelled,
}

/// Counters shared between the scanner thread and the UI, read once per frame.
#[derive(Debug, Default)]
pub struct ScanState {
    pub files_seen: AtomicU64,
    pub dirs_seen: AtomicU64,
    pub bytes_seen: AtomicU64,
    pub files_skipped: AtomicU64,
    pub candidates: AtomicU64,
    pub files_hashed: AtomicU64,
    pub bytes_hashed: AtomicU64,
    pub errors: AtomicU64,
    pub current: Mutex<String>,
    pub cancel: AtomicBool,
}

impl ScanState {
    pub fn bump(counter: &AtomicU64, by: u64) {
        counter.fetch_add(by, Ordering::Relaxed);
    }

    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn set_current(&self, path: &str) {
        if let Ok(mut guard) = self.current.lock() {
            guard.clear();
            guard.push_str(path);
        }
    }

    pub fn current_path(&self) -> String {
        self.current.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> Option<SystemTime> {
        Some(std::time::UNIX_EPOCH + Duration::from_secs(secs))
    }

    /// Three copies, 100 bytes each, with distinct mtimes and path lengths.
    fn group() -> DupeGroup {
        DupeGroup::new(
            [7u8; 32],
            100,
            vec![
                FileEntry::new(PathBuf::from("/aaa/bbb/middle.bin"), 100, at(2_000)),
                FileEntry::new(PathBuf::from("/z.bin"), 100, at(3_000)),
                FileEntry::new(PathBuf::from("/aaa/bbb/ccc/oldest.bin"), 100, at(1_000)),
            ],
        )
    }

    #[test]
    fn a_new_group_keeps_exactly_the_first_entry() {
        let g = group();
        assert_eq!(g.keeper_count(), 1);
        assert!(g.files[0].keep);
    }

    #[test]
    fn wasted_counts_every_copy_but_one() {
        let g = group();
        assert_eq!(g.wasted(), 200);
    }

    #[test]
    fn reclaimable_tracks_the_marks() {
        let mut g = group();
        assert_eq!(g.reclaimable(), 200);
        // Keeping a second copy reduces what deletion would free.
        assert!(g.toggle_mark(1));
        assert_eq!(g.reclaimable(), 100);
    }

    #[test]
    fn a_skipped_group_reclaims_nothing() {
        let mut g = group();
        g.skipped = true;
        assert_eq!(g.marked(), 0);
        assert_eq!(g.reclaimable(), 0);
    }

    #[test]
    fn the_last_keeper_cannot_be_cleared() {
        let mut g = group();
        assert_eq!(g.keeper_count(), 1);
        // Index 0 is the only keeper, so this must be refused.
        assert!(!g.toggle_mark(0));
        assert_eq!(g.keeper_count(), 1);
        assert!(g.files[0].keep);
    }

    #[test]
    fn marking_every_copy_for_deletion_is_impossible() {
        let mut g = group();
        // Hammer every index repeatedly; the invariant must always hold.
        for _ in 0..10 {
            for i in 0..g.files.len() {
                g.toggle_mark(i);
                assert!(
                    g.keeper_count() >= 1,
                    "a group must never reach zero keepers"
                );
            }
        }
    }

    #[test]
    fn keep_only_leaves_one_keeper() {
        let mut g = group();
        g.keep_only(2);
        assert_eq!(g.keeper_count(), 1);
        assert!(g.files[2].keep);
        assert!(!g.files[0].keep);
    }

    #[test]
    fn keep_only_ignores_out_of_range_indices() {
        let mut g = group();
        g.keep_only(99);
        assert_eq!(g.keeper_count(), 1, "state must be left untouched");
    }

    #[test]
    fn every_strategy_selects_the_intended_copy() {
        let cases = [
            (KeepStrategy::First, "middle.bin"),
            (KeepStrategy::Newest, "z.bin"),
            (KeepStrategy::Oldest, "oldest.bin"),
            (KeepStrategy::ShortestPath, "z.bin"),
        ];
        for (strategy, expected) in cases {
            let mut g = group();
            g.apply_strategy(strategy);
            assert_eq!(g.keeper_count(), 1, "{strategy:?} must keep exactly one");
            let kept = g.files.iter().find(|f| f.keep).unwrap();
            assert_eq!(kept.file_name(), expected, "for {strategy:?}");
        }
    }

    #[test]
    fn time_strategies_fall_back_when_no_mtime_is_available() {
        let mut g = DupeGroup::new(
            [0u8; 32],
            10,
            vec![
                FileEntry::new(PathBuf::from("/a.bin"), 10, None),
                FileEntry::new(PathBuf::from("/b.bin"), 10, None),
            ],
        );
        g.apply_strategy(KeepStrategy::Newest);
        assert_eq!(g.keeper_count(), 1);
        assert!(g.files[0].keep, "with no timestamps, keep the first entry");
    }

    #[test]
    fn a_group_with_one_untimed_copy_still_picks_a_timed_one() {
        let mut g = DupeGroup::new(
            [0u8; 32],
            10,
            vec![
                FileEntry::new(PathBuf::from("/untimed.bin"), 10, None),
                FileEntry::new(PathBuf::from("/timed.bin"), 10, at(500)),
            ],
        );
        g.apply_strategy(KeepStrategy::Newest);
        let kept = g.files.iter().find(|f| f.keep).unwrap();
        assert_eq!(kept.file_name(), "timed.bin");
    }

    #[test]
    fn sort_keys_cycle_back_around() {
        let mut key = SortKey::Wasted;
        for _ in 0..4 {
            key = key.next();
        }
        assert_eq!(key, SortKey::Wasted);
    }

    #[test]
    fn hash_prefix_is_stable_hex() {
        let g = DupeGroup::new([0xABu8; 32], 1, vec![]);
        assert_eq!(g.hash_prefix(), "ababababab ab".replace(' ', ""));
    }

    #[test]
    fn default_options_match_the_documented_defaults() {
        let o = ScanOptions::default();
        assert!(o.skip_hidden);
        assert!(o.respect_gitignore);
        assert!(o.skip_empty);
        assert!(o.collapse_hardlinks);
        assert!(!o.follow_links);
        assert_eq!(o.min_size, 0);
    }

    #[test]
    fn lossy_names_do_not_panic_on_odd_paths() {
        let f = FileEntry::new(PathBuf::from("/"), 0, None);
        // "/" has no file name; the display must still produce something.
        assert!(!f.file_name().is_empty());
    }
}
