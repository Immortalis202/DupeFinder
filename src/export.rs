//! On-demand JSON and human-readable exports of the reviewed result state.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use base64::Engine as _;
use serde::Serialize;

use crate::model::{DupeGroup, ScanOptions};

const SCHEMA_VERSION: u32 = 1;

pub struct ExportOutcome {
    pub json: Result<PathBuf, String>,
    pub text: Result<PathBuf, String>,
}

#[derive(Serialize)]
struct ExportDocument {
    schema_version: u32,
    generated_at_unix_ms: u64,
    scan_root: ExportPath,
    reference_roots: Vec<ExportPath>,
    elapsed_ms: u64,
    filters: ExportFilters,
    totals: ExportTotals,
    groups: Vec<ExportGroup>,
}

#[derive(Serialize)]
struct ExportFilters {
    include_hidden: bool,
    respect_gitignore: bool,
    include_empty: bool,
    collapse_hardlinks: bool,
    one_file_system: bool,
    follow_links: bool,
    min_size: u64,
    head_hash_min: u64,
    cache_enabled: bool,
    cache_min_size: u64,
    excluded_extensions: Vec<String>,
}

#[derive(Serialize)]
struct ExportTotals {
    groups: usize,
    files: usize,
    marked: usize,
    reclaimable_bytes: u64,
}

#[derive(Serialize)]
struct ExportGroup {
    hash: String,
    size: u64,
    wasted_bytes: u64,
    reclaimable_bytes: u64,
    selected: bool,
    skipped: bool,
    files: Vec<ExportFile>,
}

#[derive(Serialize)]
struct ExportFile {
    path: ExportPath,
    size: u64,
    modified_unix_ms: Option<u64>,
    protected: bool,
    disposition: &'static str,
}

#[derive(Serialize)]
struct ExportPath {
    display: String,
    raw: RawPath,
}

#[derive(Serialize)]
#[serde(tag = "encoding", content = "value")]
enum RawPath {
    #[cfg(windows)]
    WindowsUtf16(Vec<u16>),
    #[cfg(unix)]
    UnixBytesBase64(String),
    #[cfg(not(any(windows, unix)))]
    Display(String),
}

pub fn write_results(
    directory: &Path,
    scan_root: &Path,
    options: &ScanOptions,
    elapsed: Duration,
    groups: &[DupeGroup],
    selected: &HashSet<[u8; 32]>,
) -> ExportOutcome {
    let timestamp = now_ms() / 1000;
    let (json_path, text_path) = available_paths(directory, timestamp);
    let document = build_document(scan_root, options, elapsed, groups, selected);

    let json = serde_json::to_vec_pretty(&document)
        .map_err(|err| err.to_string())
        .and_then(|bytes| atomic_write(&json_path, &bytes).map_err(|err| err.to_string()))
        .map(|()| json_path);
    let text_body = build_text(&document);
    let text = atomic_write(&text_path, text_body.as_bytes())
        .map_err(|err| err.to_string())
        .map(|()| text_path);
    ExportOutcome { json, text }
}

fn build_document(
    scan_root: &Path,
    options: &ScanOptions,
    elapsed: Duration,
    groups: &[DupeGroup],
    selected: &HashSet<[u8; 32]>,
) -> ExportDocument {
    let export_groups: Vec<_> = groups
        .iter()
        .map(|group| ExportGroup {
            hash: hex(&group.hash),
            size: group.size,
            wasted_bytes: group.wasted(),
            reclaimable_bytes: group.reclaimable(),
            selected: selected.contains(&group.hash),
            skipped: group.skipped,
            files: group
                .files
                .iter()
                .map(|file| ExportFile {
                    path: export_path(&file.path),
                    size: file.size,
                    modified_unix_ms: file.modified.and_then(system_time_ms),
                    protected: file.protected,
                    disposition: if file.protected {
                        "protected"
                    } else if file.keep {
                        "keep"
                    } else {
                        "delete"
                    },
                })
                .collect(),
        })
        .collect();
    ExportDocument {
        schema_version: SCHEMA_VERSION,
        generated_at_unix_ms: now_ms(),
        scan_root: export_path(scan_root),
        reference_roots: options
            .reference_roots
            .iter()
            .map(|path| export_path(path))
            .collect(),
        elapsed_ms: elapsed.as_millis().min(u64::MAX as u128) as u64,
        filters: ExportFilters {
            include_hidden: !options.skip_hidden,
            respect_gitignore: options.respect_gitignore,
            include_empty: !options.skip_empty,
            collapse_hardlinks: options.collapse_hardlinks,
            one_file_system: options.same_file_system,
            follow_links: options.follow_links,
            min_size: options.min_size,
            head_hash_min: options.head_hash_min,
            cache_enabled: options.use_cache,
            cache_min_size: options.cache_min_size,
            excluded_extensions: options.exclude_exts.clone(),
        },
        totals: ExportTotals {
            groups: groups.len(),
            files: groups.iter().map(|group| group.files.len()).sum(),
            marked: groups.iter().map(DupeGroup::marked).sum(),
            reclaimable_bytes: groups.iter().map(DupeGroup::reclaimable).sum(),
        },
        groups: export_groups,
    }
}

fn build_text(document: &ExportDocument) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "dupefind results");
    let _ = writeln!(out, "root: {}", document.scan_root.display);
    let _ = writeln!(out, "groups: {}", document.totals.groups);
    let _ = writeln!(out, "files: {}", document.totals.files);
    let _ = writeln!(out, "marked: {}", document.totals.marked);
    let _ = writeln!(
        out,
        "reclaimable bytes: {}\n",
        document.totals.reclaimable_bytes
    );
    for (index, group) in document.groups.iter().enumerate() {
        let _ = writeln!(
            out,
            "group {}: size={} hash={} wasted={} reclaimable={}{}{}",
            index + 1,
            group.size,
            group.hash,
            group.wasted_bytes,
            group.reclaimable_bytes,
            if group.selected { " selected" } else { "" },
            if group.skipped { " skipped" } else { "" }
        );
        for file in &group.files {
            let _ = writeln!(
                out,
                "  [{:9}] {}",
                file.disposition.to_uppercase(),
                file.path.display
            );
        }
        out.push('\n');
    }
    out
}

fn available_paths(directory: &Path, timestamp: u64) -> (PathBuf, PathBuf) {
    for suffix in 0u32.. {
        let stem = if suffix == 0 {
            format!("dupefind-results-{timestamp}")
        } else {
            format!("dupefind-results-{timestamp}-{suffix}")
        };
        let json = directory.join(format!("{stem}.json"));
        let text = directory.join(format!("{stem}.txt"));
        if !json.exists() && !text.exists() {
            return (json, text);
        }
    }
    unreachable!()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("export path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path).map_err(|err| err.error)?;
    Ok(())
}

fn export_path(path: &Path) -> ExportPath {
    #[cfg(windows)]
    let raw = {
        use std::os::windows::ffi::OsStrExt;
        RawPath::WindowsUtf16(path.as_os_str().encode_wide().collect())
    };
    #[cfg(unix)]
    let raw = {
        use std::os::unix::ffi::OsStrExt;
        RawPath::UnixBytesBase64(
            base64::engine::general_purpose::STANDARD.encode(path.as_os_str().as_bytes()),
        )
    };
    #[cfg(not(any(windows, unix)))]
    let raw = RawPath::Display(path.to_string_lossy().into_owned());
    ExportPath {
        display: path.to_string_lossy().into_owned(),
        raw,
    }
}

fn hex(hash: &[u8; 32]) -> String {
    hash.iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn system_time_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
}

fn now_ms() -> u64 {
    system_time_ms(SystemTime::now()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileEntry;

    #[test]
    fn writes_both_formats_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let group = DupeGroup::new(
            [7; 32],
            3,
            vec![
                FileEntry::new(PathBuf::from("a"), 3, None),
                FileEntry::new(PathBuf::from("b"), 3, None),
            ],
        );
        let first = write_results(
            dir.path(),
            dir.path(),
            &ScanOptions::default(),
            Duration::from_secs(1),
            std::slice::from_ref(&group),
            &HashSet::new(),
        );
        let first_json = first.json.unwrap();
        let first_text = first.text.unwrap();
        assert!(first_json.exists() && first_text.exists());
        let second = write_results(
            dir.path(),
            dir.path(),
            &ScanOptions::default(),
            Duration::from_secs(1),
            &[group],
            &HashSet::new(),
        );
        assert_ne!(first_json, second.json.unwrap());
    }

    #[test]
    fn json_contains_review_dispositions() {
        let mut protected = FileEntry::new_protected(PathBuf::from("reference"), 3, None, true);
        protected.keep = true;
        let group = DupeGroup::new(
            [1; 32],
            3,
            vec![protected, FileEntry::new(PathBuf::from("copy"), 3, None)],
        );
        let document = build_document(
            Path::new("root"),
            &ScanOptions::default(),
            Duration::ZERO,
            &[group],
            &HashSet::new(),
        );
        assert_eq!(document.groups[0].files[0].disposition, "protected");
        assert_eq!(document.groups[0].files[1].disposition, "delete");
    }
}
