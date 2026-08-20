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

Requires Rust 1.88 or newer.

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
| `--head-hash-min BYTES` | Only head hash files above this size (default 1 MiB) |
| `--cache` | Reuse hashes when path, size, and modification time still match |
| `--cache-min-size BYTES` | Only cache files at least this large (default 256 KiB) |
| `--clear-cache` | Remove the persistent hash cache and exit |
| `--reference DIRECTORY` | Match against a protected reference tree; repeatable |
| `--export-dir DIRECTORY` | Write on-demand exports here instead of the current directory |
| `--exclude-ext EXT` | Leave out files with this extension |

Every boolean filter is also toggleable in the browser with keys `1`–`5`; the
header shows their current state.

Reference directories may be nested inside the scan root or live elsewhere.
Their files participate in duplicate matching but are permanently protected:
they cannot be marked, directly deleted, or included in a deletion plan. A
reference that contains (or equals) the scan root is rejected because it would
make the whole scan read-only. In the browser, highlight a directory and press
`R` to add or remove it as a reference.

`--exclude-ext` repeats and accepts commas, so `--exclude-ext dll --exclude-ext exe`
and `--exclude-ext dll,exe` are the same. Matching is case-insensitive and a
leading dot is accepted, so `.DLL`, `DLL` and `dll` all work. Active exclusions
are shown in the browser header, since they silently shrink the results:

```sh
dupefind --exclude-ext dll C:\Program Files
```

Shared libraries are the reason this exists. Two applications shipping the same
DLL is deliberate, so those groups are not reclaimable space — excluding them
outright beats skipping the same groups by hand on every scan. Use group
selection instead if you would rather see them and decide case by case.

## Keys

**Browser** — `↑↓` navigate · `Enter` open · `Bksp` up · `~` home · `d` drives
(Windows) / filesystem root · `.` show hidden directories · `1`–`5` filters ·
`R` toggle the highlighted reference directory · `s` scan the highlighted
directory · `q` quit

The first row of every listing is `[ scan all of <path> ]`, and it starts
highlighted — so pressing `s` on entry scans the directory you are in. It is
always there, which matters at a drive root: nothing sits above `C:\`, so there is
no `..` row to fall back on, and without it a drive holding no loose files could
not be scanned at all.

Every other row scans what it denotes: `..` scans the parent, a drive row scans
that drive, a directory row scans that directory. A file is not scannable, so `s`
there scans the directory holding it. The footer always names the target.

The other drives are listed in place of the `..` row once you reach the top of a
drive, so switching from `C:` to `D:` is `d` then `Enter` on the drive you want.

Only directories and files you would care about are listed: on Windows the
browser honours the hidden attribute as well as the dotfile convention, so a
drive root does not bury its contents under `$Recycle.Bin`,
`System Volume Information`, `pagefile.sys` and `hiberfil.sys`. `.` reveals them.

`s` scans whatever directory is highlighted, so you do not have to navigate into
it first. The footer always names the target — `s SCAN photos` — so it is never
ambiguous.

Directories are listed first in bold with a `▸`, then files, dimmed and with
their size. Files are shown so you can see what is in a directory before
scanning it, but they are context only: dupefind scans trees, so `Enter` does
nothing on a file and `s` falls back to scanning the containing directory. The
`..` row behaves the same way — `s` there scans the directory you are in.

**Scanning** — `Esc` cancel

**Dashboard** — `Tab` switch pane · `↑↓` move · `Space` keep the highlighted copy ·
`d` toggle one mark · `x` skip the whole group · `s` cycle sort ·
`t` Trash / permanent · `Del` delete just the highlighted file ·
`D` delete the marked copies · `e` export JSON and text reports · `r` rescan ·
`q` quit

Exports are created only when `e` is pressed. Each press writes a timestamped
`.json` file for machine processing and a `.txt` file for review, without
overwriting an earlier export. They include scan options, exact content hashes,
timestamps, reference status, current keep/delete marks, selection state, and
lossless raw path data in addition to display paths.

`Del` is the direct route when one copy is obviously the junk one: it deletes
exactly the highlighted file, whatever its mark, without needing you to unmark
everything else first. It confirms, names the file, and says how many copies
would remain. It refuses on the last remaining copy.

**Selecting groups** — `m` marks the highlighted group, `Shift+↑`/`Shift+↓` (or
`K`/`J`) extend a contiguous block, `a` selects all or clears, `Esc` clears the
selection.

Once any group is selected, **every bulk key acts only on the selection**: `D`
deletes just within it, `1`–`4` set the keeper only there, and `x` skips or
un-skips the whole block at once. This is how you cherry-pick a batch delete
around groups you must not touch — a DLL that two applications legitimately
share, for example. With nothing selected the bulk keys act on every group, as
before. The header always states which: `scope 12 selected` or
`scope all 318 groups`, and the confirmation repeats it.

Bulk keeper choices, applied to every group **in scope**:

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
  A single-file delete may remove the keeper, and the group then picks a new one
  rather than being left with none.
- **Deletion goes to the Recycle Bin / Trash by default** and is recoverable.
  Permanent deletion requires switching mode with `t`, and the confirmation
  dialog says plainly which one is about to happen.
- **Nothing is deleted without confirmation**, showing the file count, the bytes
  to be freed, the mode, and whether the delete is narrowed to a selection. The
  `marked` and `reclaims` figures in the header are scoped the same way, so the
  number you read is the number that will be acted on.
- **Hardlinks are collapsed by default.** Two names for one file free no space
  when one is deleted, so they are reported as a single entry.
- **Reference files are protected in every layer.** The data model, dashboard,
  deletion planner, and direct-delete path all refuse to target them.
- **A file that fails to delete stays listed** and is reported individually; one
  failure never aborts the rest of the run.

## How the scan works

| Phase | Work |
|---|---|
| 1. Walk | Collect paths, bucketed by file size |
| 2. Prune | Drop every size bucket holding one file — **no file is opened** |
| 3. Head hash | BLAKE3 the first 16 KiB of candidates above `--head-hash-min`, re-bucket, prune again |
| 4. Full hash | BLAKE3 whole files in parallel, re-bucket, prune again |
| 5. Finalize | Identify every grouped file in parallel, collapse hardlinks, sort by wasted space |

Phase 2 does the heavy lifting: on a typical tree it eliminates the large
majority of files without any I/O, because a file whose size is unique cannot
have a duplicate. Only what survives gets read. Measured on this machine, a tree
of 14,811 files totalling 500 MiB scans in well under a second.

Files are hashed in parallel across files rather than within a single file, so
the thread pool is not oversubscribed. Reads are issued in path order, so each
worker walks a run of neighbouring files instead of jumping around the tree.
Each worker reuses one 128 KiB read buffer across files, avoiding an allocation
for every hash operation.

The optional persistent cache stores the 16 KiB prefix and full BLAKE3 hashes.
A record is reused only when its path, size, and modification timestamp match;
otherwise the file is read normally. Records older than 90 days are pruned and
the cache is capped at one million entries. Cache hits are shown during a scan.
The cache is off by default so a first scan and explicitly uncached scans retain
the same behavior as before.

### Tuning for a spinning disk

An HDD is limited by seeks, not bandwidth, and every file costs one whether it is
5 KiB or 5 MiB. Two flags matter there:

- `--head-hash-min` decides which files get a cheap 16 KiB probe before being
  read in full. That probe costs an extra open, and therefore an extra seek, for
  every candidate it fails to eliminate. On an SSD it is nearly free and almost
  always worth it; on an HDD it only pays when reading the whole file would cost
  far more than a seek. Measured on one real HDD scan, the old 64 KiB threshold
  head hashed 775,780 candidates to eliminate 19,077 of them — 2.5%, nowhere near
  enough to pay for the seeks. Hence the 1 MiB default; raise it further for a
  slow disk, lower it for a fast one.
- `--min-size` skips small files outright. They are where nearly all the seeks
  are and where nearly none of the reclaimable space is.

The read rate on the scanning screen is bytes read over time spent reading, so
during phase 3 it is capped by 16 KiB per file however fast the disk is. Phase 5
opens every file in a surviving group, which on a large scan over a slow disk is
minutes of work; it reports its own progress and can be cancelled.

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

## Download

Prebuilt binaries are on the [releases page](https://github.com/Immortalis202/DupeFinder/releases):

- **Windows** — `dupefind-<version>-x86_64-pc-windows-msvc.exe`, a single
  self-contained executable. Download and run it; there is nothing to unzip and
  no Visual C++ Redistributable to install, because the MSVC runtime is linked
  statically. Run it from Windows Terminal rather than double-clicking, since it
  is a terminal application.
- **Linux** — `dupefind-<version>-x86_64-unknown-linux-gnu.tar.gz`. A tarball
  rather than a bare file so the executable bit survives the download:
  `tar xzf dupefind-*.tar.gz && ./dupefind-*/dupefind`

## Releases

Releases are driven by the `version` in `Cargo.toml`. Bump it and push to `main`;
the release publishes itself with the Windows `.exe` and the Linux tarball
attached.

`Cargo.lock` records this package's own version and CI builds with `--locked`, so
the lockfile has to be refreshed in the same commit or the build fails with
*"cannot update the lock file ... because --locked was passed"*:

```sh
# 1. bump the version in Cargo.toml, e.g. 0.1.0 -> 0.1.1
# 2. refresh Cargo.lock
cargo check
# 3. commit both files together, then push
git commit -am "Release 0.1.1"
git push
```

With [cargo-edit](https://github.com/killercup/cargo-edit) installed,
`cargo set-version 0.1.1` does steps 1 and 2 in one go.

Pushing a commit that does not change the version builds nothing. A hand-pushed
tag also works and must agree with `Cargo.toml`, or the workflow fails rather
than publishing binaries whose `--version` disagrees with the release name:

```sh
git tag v0.1.1 && git push origin v0.1.1
```

`.github/workflows/ci.yml` runs formatting, clippy and the tests on both Linux
and Windows for every push and pull request, plus a job pinned to the minimum
supported Rust version so `rust-version` in `Cargo.toml` cannot drift.

## A tree to try it on

Testing a deletion tool means deleting things, so the sample tree is generated
rather than committed — re-run this whenever you want it back:

```sh
cargo run --example make-testdata          # creates ./testdata
cargo run --example make-testdata -- /tmp/x   # or somewhere else
cargo run --release -- testdata            # scan it
```

It prints the result the scan should produce (4 groups, 7 duplicates, 4.45 MiB)
and lists what it deliberately leaves un-duplicated and why — equal-size files
with different content, two 200 KB files identical except their final byte,
0-byte files, a hidden copy, a gitignored copy, and a hardlink pair. Those exist
to prove matching is by content, not by size, name or prefix: the head-hash phase
reads only the first 16 KiB, so the same-head pair reaches the full-hash phase and
must be rejected there.

`apps/*/shared.dll` and `system/shared.dll` are four copies of one library, the
case group selection is for: mark the group with `m` and `x` to protect it.

The generator refuses to touch a directory it did not create, so pointing it at
a real folder cannot wipe it.

## Tests

```sh
cargo test
```

187 tests covering the scanner against fixture trees with known answers
(including files that share a 16 KiB head but differ in their last byte, which
must *not* be grouped), the keep/delete invariants, real on-disk deletion, and
rendering of every screen at terminal sizes from 1×1 to 300×8.

## What is not included

Group search/filter, replacing duplicates with hardlinks, similar-image
matching, and persisted configuration.
