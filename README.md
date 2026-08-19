# dupefind

A terminal UI for finding duplicate files by content and reclaiming the space
they waste. Runs on Linux and Windows 11.

Duplicates are matched by **content**, never by name: files are grouped by size,
then by the hash of their first 16 KiB, then by a full [BLAKE3](https://github.com/BLAKE3-team/BLAKE3)
hash of their contents. Two files are only ever reported as duplicates if every
byte matches.

```
┌ dupefind — /home/me/pictures ───────────────────────────────────────────────┐
│scanned 13,219 · size 465.24 MiB · groups 330 · duplicates 742 · took 0.2s    │
│marked 742 · reclaims 118.4 MiB · mode Trash · sort wasted                    │
├ groups (330) ───────────────────┬ group 1 of 330 — 3 copies ────────────────┤
│  2.86 MiB ×3   holiday.mp4      │● KEEP   backup/old/holiday-copy.mp4       │
│781.25 KiB ×2   IMG_0421.JPG     │✗ DELETE downloads/holiday.mp4             │
│      27 B ×4   config.toml      │✗ DELETE photos/2019/holiday.mp4           │
│                                 ├───────────────────────────────────────────┤
│                                 │each 2.86 MiB · wasted 5.72 MiB            │
│                                 │modified 2026-08-19 12:44 · blake3 af94aebc │
├─────────────────────────────────┴───────────────────────────────────────────┤
│↹ pane · ↑↓ move · ␣ keep this · d toggle · x skip group · 1 first · 2 newest │
│3 oldest · 4 shortest · s sort · t mode:trash · D delete marked · q quit      │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Building

Requires Rust 1.86 or newer.

```sh
cargo build --release          # ./target/release/dupefind
```

Cross-compiling for Windows from Linux, using [cargo-xwin](https://github.com/rust-cross/cargo-xwin):

```sh
cargo xwin build --release --target x86_64-pc-windows-msvc
```

## Usage

```sh
dupefind                  # open the directory browser
dupefind ~/Pictures       # scan straight away
```

| Flag | Effect |
|---|---|
| `--permanent` | Delete outright instead of moving to the Recycle Bin / Trash |
| `--hidden` | Include hidden files and directories |
| `--no-gitignore` | Ignore `.gitignore` / `.ignore` files |
| `--include-empty` | Include 0-byte files |
| `--no-collapse-hardlinks` | Treat hardlinks to one file as duplicates of each other |
| `--one-file-system` | Do not cross filesystem boundaries |
| `--follow-links` | Follow symbolic links |
| `--min-size BYTES` | Skip files below this size |

Every filter is also toggleable in the browser with keys `1`–`4`; the header
shows their current state.

## Keys

**Browser** — `↑↓` navigate · `⏎` open · `⌫` up · `~` home · `.` show hidden
directories · `1`–`4` filters · `s` scan the highlighted directory · `q` quit

`s` scans whatever directory is highlighted, so you do not have to navigate into
it first. The footer always names the target — `s SCAN photos` — so it is never
ambiguous.

Directories are listed first in bold with a `▸`, then files, dimmed and with
their size. Files are shown so you can see what is in a directory before
scanning it, but they are context only: dupefind scans trees, so `Enter` does
nothing on a file and `s` falls back to scanning the containing directory. The
`..` row behaves the same way — `s` there scans the directory you are in.

**Scanning** — `Esc` cancel

**Dashboard** — `↹` switch pane · `↑↓` move · `␣` keep the highlighted copy ·
`d` toggle one mark · `x` skip the whole group · `s` cycle sort ·
`t` Trash / permanent · `D` delete the marked copies · `r` rescan · `q` quit

Bulk keeper choices, applied to **every** group at once:

| Key | Keeps |
|---|---|
| `1` | the first copy listed (paths sort alphabetically) |
| `2` | the most recently modified copy |
| `3` | the oldest copy |
| `4` | the copy with the shortest path |

`1` is the quick "keep the first one everywhere" option.

## Safety

- **Every group always keeps at least one copy.** This is enforced in the data
  model, not just in the UI: `toggle_mark` refuses to clear a group's last
  keeper, so marking every copy of a file for deletion cannot be expressed.
- **Deletion goes to the Recycle Bin / Trash by default** and is recoverable.
  Permanent deletion requires switching mode with `t`, and the confirmation
  dialog says plainly which one is about to happen.
- **Nothing is deleted without confirmation**, showing the file count, the bytes
  to be freed, and the mode.
- **Hardlinks are collapsed by default.** Two names for one file free no space
  when one is deleted, so they are reported as a single entry.
- **A file that fails to delete stays listed** and is reported individually; one
  failure never aborts the rest of the run.

## How the scan works

| Phase | Work |
|---|---|
| 1. Walk | Collect paths, bucketed by file size |
| 2. Prune | Drop every size bucket holding one file — **no file is opened** |
| 3. Head hash | BLAKE3 the first 16 KiB of larger candidates, re-bucket, prune again |
| 4. Full hash | BLAKE3 whole files in parallel, re-bucket, prune again |
| 5. Finalize | Collapse hardlinks, sort by wasted space |

Phase 2 does the heavy lifting: on a typical tree it eliminates the large
majority of files without any I/O, because a file whose size is unique cannot
have a duplicate. Only what survives gets read. Measured on this machine, a tree
of 14,811 files totalling 500 MiB scans in well under a second.

Files are hashed in parallel across files rather than within a single file, so
the thread pool is not oversubscribed.

## Platform notes

- **Windows 11**: junctions and other reparse points are skipped so the walk
  cannot loop through them. Paths longer than 260 characters may fail to open;
  such files are reported as errors and the scan continues. Key events are
  filtered to `Press` only — Windows also reports `Release`, which would
  otherwise make every keystroke fire twice.
- **Linux**: the Trash is the freedesktop one. Files that live on a filesystem
  other than your home directory cannot be moved to `~/.local/share/Trash`; the
  `trash` crate uses a `.Trash-$UID` directory on that filesystem instead, or
  reports a per-file failure. Switch to permanent deletion with `t` if that is
  not what you want.
- Paths are never assumed to be UTF-8; display is lossy rather than fatal.

## Tests

```sh
cargo test
```

103 tests covering the scanner against fixture trees with known answers
(including files that share a 16 KiB head but differ in their last byte, which
must *not* be grouped), the keep/delete invariants, real on-disk deletion, and
rendering of every screen at terminal sizes from 1×1 to 300×8.

## What is not included

Group search/filter, report export, replacing duplicates with hardlinks,
similar-image matching, and persisted configuration.
