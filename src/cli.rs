//! Command line surface. Every filter here is also toggleable inside the TUI;
//! the flags exist so a repeat scan of a known tree can skip the picker.

use std::path::PathBuf;

use clap::Parser;

use crate::delete::DeleteMode;
use crate::model::{DEFAULT_CACHE_MIN_SIZE, DEFAULT_HEAD_HASH_MIN, ScanOptions};

#[derive(Parser, Debug)]
#[command(
    name = "dupefind",
    version,
    about = "Find and remove duplicate files by content, in your terminal",
    long_about = "Scans a directory tree for files with byte-identical content \
(matched by BLAKE3 hash), shows what they waste, and deletes only the copies you \
did not choose to keep.\n\nWith no DIRECTORY argument, dupefind opens a directory \
browser so you can pick one."
)]
pub struct Args {
    /// Directory to scan. Omit to open the directory browser instead.
    pub directory: Option<PathBuf>,

    /// Delete permanently instead of moving to the Recycle Bin / Trash.
    #[arg(long)]
    pub permanent: bool,

    /// Include hidden files and directories.
    #[arg(long)]
    pub hidden: bool,

    /// Do not honour .gitignore / .ignore files.
    #[arg(long)]
    pub no_gitignore: bool,

    /// Include empty (0-byte) files, which are all identical to each other.
    #[arg(long)]
    pub include_empty: bool,

    /// Report hardlinks to the same file as duplicates of each other.
    #[arg(long)]
    pub no_collapse_hardlinks: bool,

    /// Do not descend into other mounted filesystems.
    #[arg(long)]
    pub one_file_system: bool,

    /// Follow symbolic links while walking.
    #[arg(long)]
    pub follow_links: bool,

    /// Ignore files smaller than this many bytes.
    #[arg(long, value_name = "BYTES", default_value_t = 0)]
    pub min_size: u64,

    /// Only head hash files larger than this many bytes.
    ///
    /// Before hashing a file in full, the scanner hashes its first 16 KiB to
    /// split same-size candidates cheaply. That extra open costs a seek, which
    /// on a spinning disk is about what reading a whole megabyte costs, so for
    /// smaller files the pass is a net loss. Raise it for an HDD, lower it for
    /// an SSD, or set it above your largest file to skip the pass entirely.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_HEAD_HASH_MIN)]
    pub head_hash_min: u64,

    /// Reuse cached hashes when path, size and modification time match.
    #[arg(long)]
    pub cache: bool,

    /// Only cache files at least this large.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_CACHE_MIN_SIZE)]
    pub cache_min_size: u64,

    /// Remove the persistent hash cache and exit.
    #[arg(long)]
    pub clear_cache: bool,

    /// Protected directory that participates in matching but never deletion.
    #[arg(long, value_name = "DIRECTORY")]
    pub reference: Vec<PathBuf>,

    /// Existing directory that receives on-demand exports.
    #[arg(long, value_name = "DIRECTORY")]
    pub export_dir: Option<PathBuf>,

    /// Leave out files with this extension. Repeatable, or comma-separated:
    /// `--exclude-ext dll --exclude-ext exe` and `--exclude-ext dll,exe` are the
    /// same. Case-insensitive, and a leading dot is accepted.
    ///
    /// Useful for shared libraries: two applications shipping the same DLL is
    /// deliberate, so those groups are noise you would otherwise skip by hand on
    /// every scan.
    #[arg(long, value_name = "EXT", value_delimiter = ',')]
    pub exclude_ext: Vec<String>,
}

/// Lower-case, strip any leading dot, and drop blanks, so `--exclude-ext .DLL`
/// and `--exclude-ext dll` behave the same.
fn normalise_exts(raw: &[String]) -> Vec<String> {
    raw.iter()
        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty())
        .collect()
}

impl Args {
    pub fn scan_options(&self) -> ScanOptions {
        ScanOptions {
            skip_hidden: !self.hidden,
            respect_gitignore: !self.no_gitignore,
            skip_empty: !self.include_empty,
            collapse_hardlinks: !self.no_collapse_hardlinks,
            same_file_system: self.one_file_system,
            follow_links: self.follow_links,
            min_size: self.min_size,
            head_hash_min: self.head_hash_min,
            use_cache: self.cache,
            cache_min_size: self.cache_min_size,
            reference_roots: self.reference.clone(),
            exclude_exts: normalise_exts(&self.exclude_ext),
        }
    }

    pub fn delete_mode(&self) -> DeleteMode {
        if self.permanent {
            DeleteMode::Permanent
        } else {
            DeleteMode::Trash
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> ScanOptions {
        let mut argv = vec!["dupefind"];
        argv.extend_from_slice(args);
        Args::parse_from(argv).scan_options()
    }

    #[test]
    fn no_exclusions_by_default() {
        assert!(opts(&[]).exclude_exts.is_empty());
    }

    #[test]
    fn a_single_extension_is_accepted() {
        assert_eq!(opts(&["--exclude-ext", "dll"]).exclude_exts, vec!["dll"]);
    }

    #[test]
    fn the_flag_repeats_and_also_splits_on_commas() {
        let repeated = opts(&["--exclude-ext", "dll", "--exclude-ext", "exe"]);
        let joined = opts(&["--exclude-ext", "dll,exe"]);
        assert_eq!(repeated.exclude_exts, vec!["dll", "exe"]);
        assert_eq!(repeated.exclude_exts, joined.exclude_exts);
    }

    #[test]
    fn a_leading_dot_and_case_are_forgiven() {
        // ".DLL", "DLL" and "dll" all mean the same thing to a user.
        for spelling in [".DLL", "DLL", "dll", " .dll "] {
            assert_eq!(
                opts(&["--exclude-ext", spelling]).exclude_exts,
                vec!["dll"],
                "for {spelling:?}"
            );
        }
    }

    #[test]
    fn empty_entries_are_dropped() {
        // A trailing comma should not produce an extension that matches nothing.
        assert_eq!(opts(&["--exclude-ext", "dll,"]).exclude_exts, vec!["dll"]);
    }

    #[test]
    fn min_size_and_exclusions_are_independent() {
        let o = opts(&["--min-size", "1024", "--exclude-ext", "dll"]);
        assert_eq!(o.min_size, 1024);
        assert_eq!(o.exclude_exts, vec!["dll"]);
    }

    #[test]
    fn cache_and_reference_flags_reach_scan_options() {
        let o = opts(&[
            "--cache",
            "--cache-min-size",
            "4096",
            "--reference",
            "archive",
            "--reference",
            "backup",
        ]);
        assert!(o.use_cache);
        assert_eq!(o.cache_min_size, 4096);
        assert_eq!(
            o.reference_roots,
            vec![PathBuf::from("archive"), PathBuf::from("backup")]
        );
    }
}
