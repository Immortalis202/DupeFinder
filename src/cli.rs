//! Command line surface. Every filter here is also toggleable inside the TUI;
//! the flags exist so a repeat scan of a known tree can skip the picker.

use std::path::PathBuf;

use clap::Parser;

use crate::delete::DeleteMode;
use crate::model::ScanOptions;

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
