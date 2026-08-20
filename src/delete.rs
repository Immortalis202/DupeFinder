//! Deletion of the copies the user did not keep.
//!
//! Runs on its own thread so the UI stays responsive, and reports every failure
//! individually: a file that cannot be removed must not abort the rest.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;

use crate::model::DupeGroup;

/// Where deleted files go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// Recycle Bin on Windows, XDG Trash on Linux. Recoverable.
    Trash,
    /// `fs::remove_file`. Not recoverable.
    Permanent,
}

impl DeleteMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Trash => "Trash",
            Self::Permanent => "Permanent",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Trash => Self::Permanent,
            Self::Permanent => Self::Trash,
        }
    }
}

/// One file that could not be removed.
#[derive(Debug, Clone)]
pub struct DeleteFailure {
    pub path: PathBuf,
    pub message: String,
}

/// Outcome of a deletion run.
#[derive(Debug, Clone, Default)]
pub struct DeleteReport {
    pub deleted: u64,
    pub bytes_freed: u64,
    pub failures: Vec<DeleteFailure>,
    pub mode_label: String,
    /// Exactly what left the disk. The dashboard prunes against this rather
    /// than re-deriving it from the marks, which would be wrong whenever the
    /// deleted file was a keeper -- as it is for a single-file delete.
    pub deleted_paths: Vec<PathBuf>,
}

/// Live counters for the deletion progress screen.
#[derive(Debug, Default)]
pub struct DeleteState {
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub bytes_freed: AtomicU64,
    pub failed: AtomicU64,
    pub current: Mutex<String>,
    pub cancel: AtomicBool,
}

impl DeleteState {
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn current_path(&self) -> String {
        self.current.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn set_current(&self, path: &str) {
        if let Ok(mut guard) = self.current.lock() {
            guard.clear();
            guard.push_str(path);
        }
    }
}

#[derive(Debug)]
pub enum DeleteMsg {
    Done(DeleteReport),
}

/// Every file the marks would remove across `groups`, paired with its size.
///
/// Takes an iterator so a caller can narrow the set of groups first -- the
/// dashboard passes only the groups a selection covers. Keeping the
/// "not skipped, not kept" rule in one place stops the scoped and unscoped
/// paths from drifting apart.
pub fn pending_in<'a>(groups: impl IntoIterator<Item = &'a DupeGroup>) -> Vec<(PathBuf, u64)> {
    groups
        .into_iter()
        .filter(|g| !g.skipped)
        .flat_map(|g| {
            g.files
                .iter()
                .filter(|f| !f.keep && !f.protected)
                .map(|f| (f.path.clone(), f.size))
        })
        .collect()
}

/// Delete `targets`, one at a time, reporting progress through `state`.
///
/// Deletion is deliberately sequential: it is I/O bound on metadata operations,
/// and a predictable order makes the failure list easier to act on.
pub fn run(
    targets: Vec<(PathBuf, u64)>,
    mode: DeleteMode,
    state: Arc<DeleteState>,
    tx: Sender<DeleteMsg>,
) {
    state.total.store(targets.len() as u64, Ordering::Relaxed);

    let mut report = DeleteReport {
        mode_label: mode.label().to_string(),
        ..Default::default()
    };

    for (path, size) in targets {
        if state.is_cancelled() {
            break;
        }
        state.set_current(&path.to_string_lossy());

        let outcome = match mode {
            DeleteMode::Trash => trash::delete(&path).map_err(|e| e.to_string()),
            DeleteMode::Permanent => std::fs::remove_file(&path).map_err(|e| e.to_string()),
        };

        match outcome {
            Ok(()) => {
                report.deleted += 1;
                report.bytes_freed += size;
                report.deleted_paths.push(path);
                state.done.fetch_add(1, Ordering::Relaxed);
                state.bytes_freed.fetch_add(size, Ordering::Relaxed);
            }
            Err(message) => {
                report.failures.push(DeleteFailure { path, message });
                state.done.fetch_add(1, Ordering::Relaxed);
                state.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let _ = tx.send(DeleteMsg::Done(report));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DupeGroup, FileEntry, KeepStrategy};

    fn entry(path: &std::path::Path, size: u64) -> FileEntry {
        FileEntry::new(path.to_path_buf(), size, None)
    }

    /// A real on-disk group: three identical files, the first kept.
    fn on_disk_group(dir: &std::path::Path) -> DupeGroup {
        let content = b"duplicate payload";
        let paths: Vec<_> = ["keep.bin", "drop1.bin", "drop2.bin"]
            .iter()
            .map(|name| {
                let p = dir.join(name);
                std::fs::write(&p, content).unwrap();
                p
            })
            .collect();

        DupeGroup::new(
            [9u8; 32],
            content.len() as u64,
            paths
                .iter()
                .map(|p| entry(p, content.len() as u64))
                .collect(),
        )
    }

    fn run_sync(targets: Vec<(PathBuf, u64)>, mode: DeleteMode) -> DeleteReport {
        let (tx, rx) = crossbeam_channel::unbounded();
        run(targets, mode, Arc::new(DeleteState::default()), tx);
        match rx.try_recv() {
            Ok(DeleteMsg::Done(report)) => report,
            _ => panic!("the deleter should always report a result"),
        }
    }

    #[test]
    fn pending_lists_only_the_unkept_copies() {
        let dir = tempfile::tempdir().unwrap();
        let group = on_disk_group(dir.path());
        let targets = pending_in([&group]);

        assert_eq!(targets.len(), 2);
        assert!(
            !targets.iter().any(|(p, _)| p.ends_with("keep.bin")),
            "the keeper must never be a deletion target"
        );
    }

    #[test]
    fn a_protected_copy_is_never_a_pending_target() {
        let dir = tempfile::tempdir().unwrap();
        let mut group = on_disk_group(dir.path());
        group.files[1].protected = true;
        group.files[1].keep = false;

        let targets = pending_in([&group]);
        assert!(!targets.iter().any(|(path, _)| path.ends_with("drop1.bin")));
    }

    #[test]
    fn a_skipped_group_contributes_no_targets() {
        let dir = tempfile::tempdir().unwrap();
        let mut group = on_disk_group(dir.path());
        group.skipped = true;
        assert!(pending_in([&group]).is_empty());
    }

    #[test]
    fn pending_totals_agree_with_the_group_arithmetic() {
        let dir = tempfile::tempdir().unwrap();
        let group = on_disk_group(dir.path());
        let targets = pending_in([&group]);

        assert_eq!(targets.len(), group.marked());
        assert_eq!(
            targets.iter().map(|(_, s)| s).sum::<u64>(),
            group.reclaimable()
        );
    }

    #[test]
    fn pending_in_narrows_to_the_groups_it_is_given() {
        let dir = tempfile::tempdir().unwrap();
        let a = on_disk_group(&{
            let p = dir.path().join("a");
            std::fs::create_dir(&p).unwrap();
            p
        });
        let b = on_disk_group(&{
            let p = dir.path().join("b");
            std::fs::create_dir(&p).unwrap();
            p
        });

        let both = pending_in([&a, &b]);
        let just_a = pending_in([&a]);
        assert_eq!(both.len(), just_a.len() * 2);
        assert!(
            just_a
                .iter()
                .all(|(p, _)| p.starts_with(dir.path().join("a")))
        );
    }

    #[test]
    fn permanent_deletion_removes_the_marked_copies_and_keeps_the_keeper() {
        let dir = tempfile::tempdir().unwrap();
        let group = on_disk_group(dir.path());
        let report = run_sync(pending_in([&group]), DeleteMode::Permanent);

        assert_eq!(report.deleted, 2);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(
            dir.path().join("keep.bin").exists(),
            "the kept copy must survive"
        );
        assert!(!dir.path().join("drop1.bin").exists());
        assert!(!dir.path().join("drop2.bin").exists());
    }

    #[test]
    fn changing_the_keeper_changes_what_gets_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let mut group = on_disk_group(dir.path());
        // Keep the shortest path instead: all three names are the same length,
        // so pick explicitly rather than relying on a tie-break.
        let keep_idx = group
            .files
            .iter()
            .position(|f| f.file_name() == "drop2.bin")
            .unwrap();
        group.keep_only(keep_idx);

        let report = run_sync(pending_in([&group]), DeleteMode::Permanent);

        assert_eq!(report.deleted, 2);
        assert!(
            dir.path().join("drop2.bin").exists(),
            "the newly chosen keeper must survive"
        );
        assert!(!dir.path().join("keep.bin").exists());
    }

    #[test]
    fn a_missing_file_is_reported_as_a_failure_not_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let ghost = dir.path().join("never-existed.bin");
        let report = run_sync(vec![(ghost.clone(), 10)], DeleteMode::Permanent);

        assert_eq!(report.deleted, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, ghost);
    }

    #[test]
    fn one_failure_does_not_stop_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("real.bin");
        std::fs::write(&good, b"payload").unwrap();
        let ghost = dir.path().join("missing.bin");

        let report = run_sync(vec![(ghost, 7), (good.clone(), 7)], DeleteMode::Permanent);

        assert_eq!(report.deleted, 1, "the deletable file should still go");
        assert_eq!(report.failures.len(), 1);
        assert!(!good.exists());
    }

    #[test]
    fn bytes_freed_only_counts_successful_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("real.bin");
        std::fs::write(&good, b"12345").unwrap();

        let report = run_sync(
            vec![(good, 5), (dir.path().join("ghost.bin"), 999)],
            DeleteMode::Permanent,
        );
        assert_eq!(report.bytes_freed, 5, "the failed file freed nothing");
    }

    #[test]
    fn cancelling_stops_before_the_next_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        let b = dir.path().join("b.bin");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();

        let state = Arc::new(DeleteState::default());
        state.request_cancel();
        let (tx, rx) = crossbeam_channel::unbounded();
        run(
            vec![(a.clone(), 1), (b.clone(), 1)],
            DeleteMode::Permanent,
            state,
            tx,
        );

        let Ok(DeleteMsg::Done(report)) = rx.try_recv() else {
            panic!("expected a report");
        };
        assert_eq!(report.deleted, 0);
        assert!(a.exists() && b.exists(), "a cancelled run deletes nothing");
    }

    #[test]
    fn an_empty_target_list_is_a_no_op() {
        let report = run_sync(Vec::new(), DeleteMode::Permanent);
        assert_eq!(report.deleted, 0);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn the_report_records_which_mode_ran() {
        let report = run_sync(Vec::new(), DeleteMode::Permanent);
        assert_eq!(report.mode_label, "Permanent");
        let report = run_sync(Vec::new(), DeleteMode::Trash);
        assert_eq!(report.mode_label, "Trash");
    }

    #[test]
    fn delete_mode_toggles_between_exactly_two_states() {
        assert_eq!(DeleteMode::Trash.toggled(), DeleteMode::Permanent);
        assert_eq!(DeleteMode::Permanent.toggled(), DeleteMode::Trash);
    }

    #[test]
    fn deleting_across_several_groups_respects_every_keeper() {
        let dir = tempfile::tempdir().unwrap();
        let mut groups = Vec::new();
        for g in 0..3 {
            let sub = dir.path().join(format!("g{g}"));
            std::fs::create_dir(&sub).unwrap();
            let mut group = on_disk_group(&sub);
            group.apply_strategy(KeepStrategy::First);
            groups.push(group);
        }

        let report = run_sync(pending_in(groups.iter()), DeleteMode::Permanent);
        assert_eq!(
            report.deleted, 6,
            "two of three copies in each of three groups"
        );

        for g in 0..3 {
            let sub = dir.path().join(format!("g{g}"));
            assert!(sub.join("keep.bin").exists(), "group {g} lost its keeper");
        }
    }
}
