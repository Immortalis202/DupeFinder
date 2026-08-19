//! The duplicate scanner.
//!
//! Five phases, cheapest first, so that the expensive I/O only ever touches
//! files that could still be duplicates:
//!
//! 1. walk the tree and bucket paths by size
//! 2. drop every size bucket holding a single file (no I/O at all)
//! 3. hash the first 16 KiB of larger candidates and re-bucket
//! 4. hash full contents in parallel and re-bucket
//! 5. collapse hardlinks and sort
//!
//! Phase 2 is what makes this fast: on a typical tree it eliminates the large
//! majority of files without opening any of them.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crossbeam_channel::Sender;
use ignore::WalkBuilder;
use rayon::prelude::*;
use same_file::Handle;

use crate::model::{DupeGroup, FileEntry, Phase, ScanError, ScanMsg, ScanOptions, ScanState};

/// Size below which the head-hash phase is pointless: reading 16 KiB of a 20 KiB
/// file costs the same as reading all of it, so we go straight to the full hash.
const HEAD_HASH_MIN_SIZE: u64 = 64 * 1024;

/// How much of the front of a file the head-hash phase reads.
const HEAD_HASH_BYTES: u64 = 16 * 1024;

/// Read buffer for full-file hashing.
const READ_BUFFER: usize = 128 * 1024;

/// A candidate file carried between phases.
struct Candidate {
    path: PathBuf,
    size: u64,
}

/// Run a full scan. Intended to be called on a dedicated thread; progress is
/// published through `state` and stage transitions through `tx`.
pub fn run(root: PathBuf, options: ScanOptions, state: Arc<ScanState>, tx: Sender<ScanMsg>) {
    // The UI's throughput figure needs to know when reading began, separately
    // from when the scan began.
    let started = Instant::now();

    let by_size = match walk(&root, &options, &state, &tx) {
        Some(map) => map,
        None => {
            let _ = tx.send(ScanMsg::Cancelled);
            return;
        }
    };

    let _ = tx.send(ScanMsg::Phase(Phase::Pruning));
    let candidates = prune_by_size(by_size);
    state
        .candidates
        .store(candidates.len() as u64, Ordering::Relaxed);

    if state.is_cancelled() {
        let _ = tx.send(ScanMsg::Cancelled);
        return;
    }

    let _ = tx.send(ScanMsg::Phase(Phase::HeadHashing));
    let candidates = match head_hash_pass(candidates, &state, &tx, started) {
        Some(c) => c,
        None => {
            let _ = tx.send(ScanMsg::Cancelled);
            return;
        }
    };
    state
        .candidates
        .store(candidates.len() as u64, Ordering::Relaxed);
    state.files_hashed.store(0, Ordering::Relaxed);

    let _ = tx.send(ScanMsg::Phase(Phase::FullHashing));
    let groups = match full_hash_pass(candidates, &state, &tx, started) {
        Some(g) => g,
        None => {
            let _ = tx.send(ScanMsg::Cancelled);
            return;
        }
    };

    let _ = tx.send(ScanMsg::Phase(Phase::Finalizing));
    let mut groups = finalize(groups, &options);
    groups.sort_by(|a, b| {
        b.wasted()
            .cmp(&a.wasted())
            .then_with(|| a.size.cmp(&b.size))
    });

    let _ = tx.send(ScanMsg::Done(groups));
}

/// Phase 1: walk the tree, bucketing files by size. Returns `None` if cancelled.
fn walk(
    root: &Path,
    options: &ScanOptions,
    state: &Arc<ScanState>,
    tx: &Sender<ScanMsg>,
) -> Option<HashMap<u64, Vec<Candidate>>> {
    let _ = tx.send(ScanMsg::Phase(Phase::Walking));

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(options.skip_hidden)
        .git_ignore(options.respect_gitignore)
        .git_global(options.respect_gitignore)
        .git_exclude(options.respect_gitignore)
        .ignore(options.respect_gitignore)
        .parents(options.respect_gitignore)
        // Without this, a .gitignore outside a git repository is ignored, which
        // is surprising for a tool pointed at an arbitrary directory.
        .require_git(false)
        .follow_links(options.follow_links)
        .same_file_system(options.same_file_system);

    let mut by_size: HashMap<u64, Vec<Candidate>> = HashMap::new();

    for result in builder.build() {
        if state.is_cancelled() {
            return None;
        }

        let entry = match result {
            Ok(e) => e,
            Err(err) => {
                ScanState::bump(&state.errors, 1);
                let _ = tx.send(ScanMsg::Error(ScanError::new(None, err.to_string())));
                continue;
            }
        };

        let file_type = match entry.file_type() {
            Some(ft) => ft,
            // `None` only happens for stdin, which cannot appear here.
            None => continue,
        };

        if file_type.is_dir() {
            ScanState::bump(&state.dirs_seen, 1);
            state.set_current(&entry.path().to_string_lossy());
            continue;
        }

        if !file_type.is_file() {
            // Symlinks (when not followed), sockets, FIFOs, devices.
            ScanState::bump(&state.files_skipped, 1);
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                ScanState::bump(&state.errors, 1);
                let _ = tx.send(ScanMsg::Error(ScanError::new(
                    Some(entry.path().to_path_buf()),
                    err.to_string(),
                )));
                continue;
            }
        };

        if is_reparse_point(&metadata) {
            ScanState::bump(&state.files_skipped, 1);
            continue;
        }

        let size = metadata.len();
        ScanState::bump(&state.files_seen, 1);
        ScanState::bump(&state.bytes_seen, size);

        if (options.skip_empty && size == 0)
            || size < options.min_size
            || options.excluded_by_extension(entry.path())
        {
            ScanState::bump(&state.files_skipped, 1);
            continue;
        }

        by_size.entry(size).or_default().push(Candidate {
            path: entry.path().to_path_buf(),
            size,
        });
    }

    Some(by_size)
}

/// On Windows, skip junctions and other reparse points so the walk cannot loop
/// through them. Symlinks are already excluded by `follow_links(false)`.
#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

/// Phase 2: only sizes seen more than once can contain duplicates.
fn prune_by_size(by_size: HashMap<u64, Vec<Candidate>>) -> Vec<Candidate> {
    by_size
        .into_values()
        .filter(|bucket| bucket.len() > 1)
        .flatten()
        .collect()
}

/// Phase 3: split same-size buckets by the hash of their first 16 KiB.
fn head_hash_pass(
    candidates: Vec<Candidate>,
    state: &Arc<ScanState>,
    tx: &Sender<ScanMsg>,
    started: Instant,
) -> Option<Vec<Candidate>> {
    // Files too small to benefit skip straight to the full hash.
    let (large, small): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .partition(|c| c.size > HEAD_HASH_MIN_SIZE);

    if large.is_empty() {
        return Some(small);
    }

    let hashed = hash_in_parallel(large, state, tx, started, |path| {
        hash_prefix_of(path, HEAD_HASH_BYTES)
    })?;

    // Group by (size, head hash): a shared head is necessary but not sufficient.
    let mut buckets: HashMap<(u64, [u8; 32]), Vec<Candidate>> = HashMap::new();
    for (candidate, hash) in hashed {
        buckets
            .entry((candidate.size, hash))
            .or_default()
            .push(candidate);
    }

    let mut survivors: Vec<Candidate> = buckets
        .into_values()
        .filter(|bucket| bucket.len() > 1)
        .flatten()
        .collect();
    survivors.extend(small);
    Some(survivors)
}

/// Phase 4: full content hash, then group by digest.
fn full_hash_pass(
    candidates: Vec<Candidate>,
    state: &Arc<ScanState>,
    tx: &Sender<ScanMsg>,
    started: Instant,
) -> Option<HashMap<[u8; 32], Vec<Candidate>>> {
    let hashed = hash_in_parallel(candidates, state, tx, started, hash_whole_file)?;

    let mut buckets: HashMap<[u8; 32], Vec<Candidate>> = HashMap::new();
    for (candidate, hash) in hashed {
        buckets.entry(hash).or_default().push(candidate);
    }
    buckets.retain(|_, bucket| bucket.len() > 1);
    Some(buckets)
}

/// Put candidates in path order, so that reads walk the tree instead of
/// jumping around it.
///
/// Both hashing passes build their input by flattening a `HashMap`, whose
/// iteration order is arbitrary, so the reads were issued in effectively random
/// order across the whole tree. On a spinning disk that is the worst case: every
/// file costs a full seek, and small files spend all their time waiting for the
/// head rather than transferring. Path order is not physical order, but files in
/// one directory are usually allocated together, so it is a good proxy -- and
/// because rayon splits the slice into contiguous ranges, each worker then walks
/// its own run of neighbouring files instead of the whole tree.
///
/// Costs one sort of the candidate paths, which is nothing next to the I/O it
/// saves. On an SSD, where seeks are free, it changes little either way.
fn sort_for_sequential_reads(candidates: &mut [Candidate]) {
    candidates.sort_unstable_by(|a, b| a.path.cmp(&b.path));
}

/// Hash many files across the rayon pool, reporting progress and swallowing
/// per-file I/O errors. Returns `None` if the scan was cancelled.
///
/// Parallelism is across files rather than within one file (`update_rayon`) so
/// the thread pool is not oversubscribed.
fn hash_in_parallel<F>(
    mut candidates: Vec<Candidate>,
    state: &Arc<ScanState>,
    tx: &Sender<ScanMsg>,
    started: Instant,
    hasher: F,
) -> Option<Vec<(Candidate, [u8; 32])>>
where
    F: Fn(&Path) -> std::io::Result<([u8; 32], u64)> + Send + Sync,
{
    // Sorted before the mark, since ordering is not reading and must not count
    // against the reported rate.
    sort_for_sequential_reads(&mut candidates);

    if !candidates.is_empty() {
        state.mark_hashing_started(started.elapsed());
    }

    let out: Vec<_> = candidates
        .into_par_iter()
        .filter_map(|candidate| {
            if state.is_cancelled() {
                return None;
            }
            state.set_current(&candidate.path.to_string_lossy());
            match hasher(&candidate.path) {
                Ok((hash, read)) => {
                    ScanState::bump(&state.files_hashed, 1);
                    ScanState::bump(&state.bytes_hashed, read);
                    Some((candidate, hash))
                }
                Err(err) => {
                    ScanState::bump(&state.errors, 1);
                    ScanState::bump(&state.files_hashed, 1);
                    let _ = tx.send(ScanMsg::Error(ScanError::new(
                        Some(candidate.path.clone()),
                        err.to_string(),
                    )));
                    None
                }
            }
        })
        .collect();

    if state.is_cancelled() {
        return None;
    }
    Some(out)
}

/// BLAKE3 of the whole file. Returns the digest and the number of bytes read.
/// BLAKE3 of the whole file. Returns the digest and the number of bytes read.
fn hash_whole_file(path: &Path) -> std::io::Result<([u8; 32], u64)> {
    hash_upto(path, u64::MAX)
}

/// BLAKE3 of at most the first `limit` bytes.
fn hash_prefix_of(path: &Path, limit: u64) -> std::io::Result<([u8; 32], u64)> {
    hash_upto(path, limit)
}

/// Hash up to `limit` bytes of `path`, reading through a heap buffer.
///
/// Deliberately not `blake3::Hasher::update_reader`: that puts a 64 KiB array on
/// the stack, and because hashing runs inside a rayon parallel collect -- whose
/// split recursion deepens with the thread count -- that frame was enough to
/// overflow a worker's 2 MiB stack on a many-core machine scanning tens of
/// thousands of files. Reading into a heap buffer keeps the frame small however
/// deep the split tree gets, and also drops a redundant copy of every byte:
/// `update_reader` buffers internally, so wrapping the file in a `BufReader`
/// as well copied the data twice.
fn hash_upto(path: &Path, limit: u64) -> std::io::Result<([u8; 32], u64)> {
    let mut file = File::open(path)?.take(limit);
    let mut hasher = blake3::Hasher::new();
    // Never allocate more than the caller could possibly read.
    let mut buf = vec![0u8; READ_BUFFER.min(limit.max(1) as usize)];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((*hasher.finalize().as_bytes(), total))
}

/// Phase 5: turn surviving buckets into groups, collapsing hardlinks.
fn finalize(buckets: HashMap<[u8; 32], Vec<Candidate>>, options: &ScanOptions) -> Vec<DupeGroup> {
    let mut groups = Vec::new();

    for (hash, mut bucket) in buckets {
        if options.collapse_hardlinks {
            bucket = collapse_hardlinks(bucket);
            // A group of hardlinks to one inode is not a duplicate at all.
            if bucket.len() < 2 {
                continue;
            }
        }

        // Stable, predictable ordering so "keep the first" means something.
        bucket.sort_by(|a, b| a.path.cmp(&b.path));

        let size = bucket.first().map(|c| c.size).unwrap_or(0);
        let files = bucket
            .into_iter()
            .map(|c| {
                let modified = std::fs::metadata(&c.path)
                    .ok()
                    .and_then(|m| m.modified().ok());
                FileEntry::new(c.path, c.size, modified)
            })
            .collect();

        groups.push(DupeGroup::new(hash, size, files));
    }

    groups
}

/// Drop entries that are additional names for a file already in the bucket.
///
/// `same_file::Handle` holds the file open, so handles are built and dropped
/// inside this function only; buckets are small, keeping descriptor use bounded.
fn collapse_hardlinks(bucket: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen: Vec<Handle> = Vec::with_capacity(bucket.len());
    let mut out = Vec::with_capacity(bucket.len());

    for candidate in bucket {
        match Handle::from_path(&candidate.path) {
            Ok(handle) => {
                if seen.contains(&handle) {
                    continue;
                }
                seen.push(handle);
                out.push(candidate);
            }
            // If we cannot open it to check, keep it and let deletion report
            // the error rather than silently dropping a real duplicate.
            Err(_) => out.push(candidate),
        }
    }

    out
}

/// Number of files hashed so far, for the progress gauge.
pub fn hashed_of(state: &ScanState) -> (u64, u64) {
    (
        ScanState::get(&state.files_hashed),
        ScanState::get(&state.candidates),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ScanMsg;
    use std::collections::BTreeSet;
    use std::fs;

    /// Run a scan to completion on the calling thread and return the groups.
    fn scan_sync(root: &Path, options: ScanOptions) -> Vec<DupeGroup> {
        let state = Arc::new(ScanState::default());
        let (tx, rx) = crossbeam_channel::unbounded();
        run(root.to_path_buf(), options, state, tx);
        let mut groups = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let ScanMsg::Done(g) = msg {
                groups = g;
            }
        }
        groups
    }

    /// Reads are issued in candidate order, and both passes build their input
    /// from a `HashMap`, so without this the disk head jumps across the tree.
    #[test]
    fn candidates_are_read_in_path_order() {
        let mut candidates: Vec<Candidate> =
            ["z/last.bin", "a/second.bin", "a/first.bin", "m/middle.bin"]
                .iter()
                .map(|p| Candidate {
                    path: PathBuf::from(p),
                    size: 1,
                })
                .collect();

        sort_for_sequential_reads(&mut candidates);

        let order: Vec<String> = candidates
            .iter()
            .map(|c| c.path.to_string_lossy().replace('\\', "/"))
            .collect();
        assert_eq!(
            order,
            ["a/first.bin", "a/second.bin", "m/middle.bin", "z/last.bin"],
            "files in one directory must be read as a run, not scattered"
        );
    }

    /// Each group rendered as its sorted set of file names, for order-independent
    /// comparison.
    fn group_names(groups: &[DupeGroup]) -> BTreeSet<BTreeSet<String>> {
        groups
            .iter()
            .map(|g| g.files.iter().map(|f| f.file_name()).collect())
            .collect()
    }

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A tree with a deliberately known answer, covering every filter and both
    /// hashing phases.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Three copies of the same content in different directories.
        let dup = b"the quick brown fox jumps over the lazy dog, repeatedly";
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::create_dir_all(root.join("c/deep")).unwrap();
        fs::write(root.join("a/one.txt"), dup).unwrap();
        fs::write(root.join("b/one_copy.txt"), dup).unwrap();
        fs::write(root.join("c/deep/three.txt"), dup).unwrap();

        // Unique content: must never appear in a group.
        fs::write(root.join("a/unique.txt"), b"nothing else looks like this").unwrap();

        // Same size, different content: must not be grouped.
        fs::write(root.join("samesize1.bin"), vec![0x01u8; 32]).unwrap();
        fs::write(root.join("samesize2.bin"), vec![0x02u8; 32]).unwrap();

        // Empty files: identical to each other, excluded by default.
        fs::write(root.join("empty1.txt"), b"").unwrap();
        fs::write(root.join("empty2.txt"), b"").unwrap();

        // Hidden copy of the duplicate content, excluded by default.
        fs::write(root.join(".hidden_dup.txt"), dup).unwrap();

        // Gitignored copy of the duplicate content, excluded by default.
        fs::write(root.join(".gitignore"), b"ignored/\n").unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::write(root.join("ignored/dup.txt"), dup).unwrap();

        // Large identical pair: exercises the head-hash phase and survives it.
        let big = vec![0xABu8; 80 * 1024];
        fs::write(root.join("big1.bin"), &big).unwrap();
        fs::write(root.join("big2.bin"), &big).unwrap();

        // Same size and same first 16 KiB, differing only in the final byte.
        // The head-hash phase cannot separate these; the full hash must.
        let mut head_a = vec![0xCDu8; 80 * 1024];
        let mut head_b = head_a.clone();
        *head_a.last_mut().unwrap() = 0x01;
        *head_b.last_mut().unwrap() = 0x02;
        fs::write(root.join("bighead1.bin"), &head_a).unwrap();
        fs::write(root.join("bighead2.bin"), &head_b).unwrap();

        dir
    }

    #[test]
    fn finds_exactly_the_planted_duplicates() {
        let dir = fixture();
        let groups = scan_sync(dir.path(), ScanOptions::default());

        let expected: BTreeSet<BTreeSet<String>> = [
            names(&["one.txt", "one_copy.txt", "three.txt"]),
            names(&["big1.bin", "big2.bin"]),
        ]
        .into_iter()
        .collect();

        assert_eq!(group_names(&groups), expected);
    }

    #[test]
    fn files_sharing_a_head_but_not_a_tail_are_not_duplicates() {
        let dir = fixture();
        let groups = scan_sync(dir.path(), ScanOptions::default());
        for group in &groups {
            for file in &group.files {
                assert!(
                    !file.file_name().starts_with("bighead"),
                    "bighead files differ in their last byte and must not be grouped"
                );
            }
        }
    }

    #[test]
    fn same_size_different_content_is_not_grouped() {
        let dir = fixture();
        let groups = scan_sync(dir.path(), ScanOptions::default());
        for group in &groups {
            for file in &group.files {
                assert!(!file.file_name().starts_with("samesize"));
            }
        }
    }

    #[test]
    fn hidden_files_are_included_on_request() {
        let dir = fixture();
        let options = ScanOptions {
            skip_hidden: false,
            ..Default::default()
        };
        let groups = scan_sync(dir.path(), options);
        let dup_group = groups
            .iter()
            .find(|g| g.files.iter().any(|f| f.file_name() == "one.txt"))
            .expect("the duplicate group should still be found");
        assert!(
            dup_group
                .files
                .iter()
                .any(|f| f.file_name() == ".hidden_dup.txt"),
            "the hidden copy should join the group once hidden files are included"
        );
    }

    #[test]
    fn gitignored_files_are_included_on_request() {
        let dir = fixture();
        let options = ScanOptions {
            respect_gitignore: false,
            ..Default::default()
        };
        let groups = scan_sync(dir.path(), options);
        let dup_group = groups
            .iter()
            .find(|g| g.files.iter().any(|f| f.file_name() == "one.txt"))
            .expect("the duplicate group should still be found");
        assert!(
            dup_group.files.iter().any(|f| f.file_name() == "dup.txt"),
            "the gitignored copy should join the group once .gitignore is off"
        );
    }

    #[test]
    fn empty_files_group_only_when_included() {
        let dir = fixture();
        let options = ScanOptions {
            skip_empty: false,
            ..Default::default()
        };
        let groups = scan_sync(dir.path(), options);
        assert!(
            groups.iter().any(|g| g.size == 0 && g.files.len() == 2),
            "the two empty files should form a zero-byte group"
        );
    }

    /// Duplicate shared libraries are usually deliberate, so they are noise.
    #[test]
    fn excluded_extensions_are_left_out_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lib = vec![0x4Du8; 5000];
        fs::create_dir_all(root.join("appA")).unwrap();
        fs::create_dir_all(root.join("appB")).unwrap();
        fs::write(root.join("appA/shared.dll"), &lib).unwrap();
        fs::write(root.join("appB/shared.dll"), &lib).unwrap();
        // A real duplicate that must survive the filter.
        let doc = b"a document duplicated on purpose";
        fs::write(root.join("appA/notes.txt"), doc).unwrap();
        fs::write(root.join("appB/notes.txt"), doc).unwrap();

        // Without the filter, both pairs are reported.
        let groups = scan_sync(root, ScanOptions::default());
        assert_eq!(groups.len(), 2, "baseline: {:?}", group_names(&groups));

        let options = ScanOptions {
            exclude_exts: vec!["dll".to_string()],
            ..Default::default()
        };
        let groups = scan_sync(root, options);
        assert_eq!(
            group_names(&groups),
            [names(&["notes.txt", "notes.txt"])].into_iter().collect(),
            "only the .dll pair should be gone"
        );
    }

    #[test]
    fn extension_matching_ignores_case() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let lib = vec![0x4Du8; 5000];
        fs::write(root.join("one.DLL"), &lib).unwrap();
        fs::write(root.join("two.Dll"), &lib).unwrap();

        let options = ScanOptions {
            exclude_exts: vec!["dll".to_string()],
            ..Default::default()
        };
        assert!(
            scan_sync(root, options).is_empty(),
            "Windows treats FOO.DLL and foo.dll as the same file"
        );
    }

    #[test]
    fn an_excluded_extension_does_not_match_a_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let content = vec![0x7Fu8; 5000];
        // "dl" must not match ".dll", and a file merely containing "dll" in its
        // stem must not be excluded either.
        fs::write(root.join("a.dl"), &content).unwrap();
        fs::write(root.join("b.dl"), &content).unwrap();
        fs::write(root.join("dll-notes.txt"), &content).unwrap();
        fs::write(root.join("copy-dll-notes.txt"), &content).unwrap();

        let options = ScanOptions {
            exclude_exts: vec!["dll".to_string()],
            ..Default::default()
        };
        let groups = scan_sync(root, options);
        assert_eq!(
            groups.len(),
            1,
            "all four files share content, so one group survives: {:?}",
            group_names(&groups)
        );
        assert_eq!(groups[0].files.len(), 4);
    }

    #[test]
    fn files_without_an_extension_are_never_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let content = vec![0x11u8; 5000];
        fs::write(root.join("Makefile"), &content).unwrap();
        fs::write(root.join("Makefile.bak"), &content).unwrap();

        let options = ScanOptions {
            exclude_exts: vec!["dll".to_string()],
            ..Default::default()
        };
        assert_eq!(scan_sync(root, options).len(), 1);
    }

    #[test]
    fn min_size_excludes_small_duplicates() {
        let dir = fixture();
        let options = ScanOptions {
            min_size: 64 * 1024,
            ..Default::default()
        };
        let groups = scan_sync(dir.path(), options);
        assert_eq!(
            group_names(&groups),
            [names(&["big1.bin", "big2.bin"])].into_iter().collect(),
        );
    }

    #[test]
    fn groups_are_sorted_by_wasted_space() {
        let dir = fixture();
        let groups = scan_sync(dir.path(), ScanOptions::default());
        let wasted: Vec<u64> = groups.iter().map(DupeGroup::wasted).collect();
        let mut sorted = wasted.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(wasted, sorted, "most wasteful group should come first");
    }

    /// `hash_upto` replaced `blake3::Hasher::update_reader`, whose 64 KiB stack
    /// array overflowed rayon worker stacks. It must still hash identically.
    #[test]
    fn a_prefix_hash_matches_hashing_the_same_bytes_directly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &data).unwrap();

        // Whole file.
        let (digest, read) = hash_whole_file(&path).unwrap();
        assert_eq!(read, data.len() as u64);
        assert_eq!(digest, *blake3::hash(&data).as_bytes());

        // A prefix, including one that straddles the read buffer.
        for limit in [1u64, 16 * 1024, READ_BUFFER as u64 + 7, 299_999] {
            let (digest, read) = hash_prefix_of(&path, limit).unwrap();
            let want = &data[..limit as usize];
            assert_eq!(read, want.len() as u64, "bytes read for limit {limit}");
            assert_eq!(digest, *blake3::hash(want).as_bytes(), "digest for {limit}");
        }
    }

    #[test]
    fn hashing_an_empty_file_does_not_allocate_a_zero_length_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        fs::write(&path, b"").unwrap();
        // A zero limit must not panic on a zero-sized buffer.
        let (digest, read) = hash_prefix_of(&path, 0).unwrap();
        assert_eq!(read, 0);
        assert_eq!(digest, *blake3::hash(b"").as_bytes());
    }

    #[test]
    fn an_empty_tree_yields_no_groups() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scan_sync(dir.path(), ScanOptions::default()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn hardlinks_collapse_to_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let content = b"content reachable under two names";
        fs::write(root.join("original.bin"), content).unwrap();
        fs::hard_link(root.join("original.bin"), root.join("linked.bin")).unwrap();

        // With collapsing on, two names for one inode are not a duplicate pair.
        let groups = scan_sync(root, ScanOptions::default());
        assert!(
            groups.is_empty(),
            "a hardlink pair frees nothing and must not be reported: {:?}",
            group_names(&groups)
        );

        // With collapsing off, they are reported.
        let options = ScanOptions {
            collapse_hardlinks: false,
            ..Default::default()
        };
        let groups = scan_sync(root, options);
        assert_eq!(
            group_names(&groups),
            [names(&["linked.bin", "original.bin"])]
                .into_iter()
                .collect()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_real_duplicate_survives_alongside_a_hardlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let content = b"three names, two inodes";
        fs::write(root.join("a.bin"), content).unwrap();
        fs::hard_link(root.join("a.bin"), root.join("a_link.bin")).unwrap();
        // A separate inode with the same bytes: a genuine duplicate.
        fs::write(root.join("b.bin"), content).unwrap();

        let groups = scan_sync(root, ScanOptions::default());
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].files.len(),
            2,
            "the hardlink should collapse but the real copy must remain"
        );
    }

    #[test]
    fn cancellation_stops_the_scan() {
        let dir = fixture();
        let state = Arc::new(ScanState::default());
        state.request_cancel();
        let (tx, rx) = crossbeam_channel::unbounded();
        run(dir.path().to_path_buf(), ScanOptions::default(), state, tx);

        let cancelled = rx.try_iter().any(|m| matches!(m, ScanMsg::Cancelled));
        assert!(cancelled, "a pre-cancelled scan should report Cancelled");
    }
}
