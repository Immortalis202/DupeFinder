//! Rendering. Each screen lives in its own module; this one owns the palette,
//! the shared chrome and the dispatch.

mod modal;
mod picker;
mod results;
mod scanning;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, Screen};

// A small, deliberately restrained palette. Every colour is a named ANSI colour
// so the app inherits the user's terminal theme instead of fighting it.
pub const ACCENT: Color = Color::Cyan;
pub const KEEP: Color = Color::Green;
pub const DELETE: Color = Color::Red;
pub const DIM: Color = Color::DarkGray;
pub const WARN: Color = Color::Yellow;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // The footer is sized to the cheatsheet it must show, so no keybinding is
    // ever clipped, at any terminal width.
    let keys = footer_keys(app);
    let key_lines = pack_keys(&keys, area.width.saturating_sub(2) as usize);
    let footer_h = (key_lines.len() as u16 + 2).clamp(3, 6);
    // The picker and the dashboard both put two lines in the header.
    let header_h = if matches!(
        app.screen,
        Screen::Picker | Screen::Results | Screen::Confirm
    ) {
        4
    } else {
        3
    };

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(header_h),
        Constraint::Min(3),
        Constraint::Length(footer_h),
    ])
    .areas(area);

    match app.screen {
        Screen::Picker => picker::draw(frame, app, header, body),
        Screen::Scanning => scanning::draw(frame, app, header, body),
        Screen::Results | Screen::Confirm => {
            results::draw(frame, app, header, body);
            if app.screen == Screen::Confirm {
                modal::draw_confirm(frame, app);
            }
        }
        Screen::Deleting => scanning::draw_deleting(frame, app, header, body),
        Screen::Done => modal::draw_done(frame, app, header, body),
    }

    draw_footer(frame, footer, key_lines, app.status.as_deref());
}

/// The keybindings relevant to the current screen.
fn footer_keys(app: &App) -> Vec<(String, String)> {
    let owned = |pairs: Vec<(&str, &str)>| -> Vec<(String, String)> {
        pairs
            .into_iter()
            .map(|(k, d)| (k.to_string(), d.to_string()))
            .collect()
    };

    match app.screen {
        // The scan target follows the highlight, so the hint names it rather
        // than saying a bare "SCAN" the user has to interpret.
        Screen::Picker => {
            let mut keys = owned(vec![
                ("\u{2191}\u{2193}", "navigate"),
                ("Enter", "open"),
                ("Bksp", "up"),
                ("~", "home"),
                (".", "hidden dirs"),
                ("1-5", "filters"),
                ("R", "reference"),
                #[cfg(windows)]
                ("d", "drives"),
                #[cfg(not(windows))]
                ("d", "root"),
            ]);
            keys.push(("s".to_string(), format!("SCAN {}", app.scan_target_label())));
            keys.push(("q".to_string(), "quit".to_string()));
            keys
        }
        Screen::Scanning => owned(vec![("Esc", "cancel scan")]),
        Screen::Deleting => owned(vec![("Esc", "stop after current file")]),
        Screen::Results | Screen::Confirm => {
            let mut keys: Vec<(&'static str, &'static str)> = vec![
                ("Tab", "pane"),
                ("\u{2191}\u{2193}", "move"),
                ("Space", "keep this"),
                ("d", "toggle"),
                ("x", "skip group"),
                ("1", "first"),
                ("2", "newest"),
                ("3", "oldest"),
                ("4", "shortest"),
                ("s", "sort"),
                ("m", "mark group"),
                ("Shift+\u{2191}\u{2193}/JK", "block"),
                ("a", "all"),
            ];
            keys.push(match app.delete_mode {
                crate::delete::DeleteMode::Trash => ("t", "mode:trash"),
                crate::delete::DeleteMode::Permanent => ("t", "mode:PERM"),
            });
            keys.push(("Del", "delete this file"));
            keys.push(("D", "delete marked"));
            keys.push(("e", "export"));
            keys.push(("r", "rescan"));
            keys.push(("q", "quit"));
            owned(keys)
        }
        Screen::Done => owned(vec![
            ("Enter", "back to results"),
            ("e", "export"),
            ("r", "rescan"),
            ("\u{2191}\u{2193}", "failures"),
            ("q", "quit"),
        ]),
    }
}

/// Greedily pack `key · description` pairs into lines that fit `width`.
///
/// Done by hand rather than with `Wrap` so the footer height can be computed
/// before the layout is chosen, and so a pair is never split across lines.
fn pack_keys(keys: &[(String, String)], width: usize) -> Vec<Line<'static>> {
    if width == 0 || keys.is_empty() {
        return vec![Line::default()];
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    for (key, desc) in keys {
        let sep = if spans.is_empty() { 0 } else { 3 };
        let cost = sep + key.chars().count() + 1 + desc.chars().count();

        if !spans.is_empty() && used + cost > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            used = 0;
        }

        if !spans.is_empty() {
            spans.push(Span::styled(" \u{b7} ", Style::default().fg(DIM)));
            used += 3;
        }
        spans.push(Span::styled(
            key.clone(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::raw(desc.clone()));
        used += key.chars().count() + 1 + desc.chars().count();
    }

    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

/// The title bar shared by every screen.
pub fn header_block(title: &str) -> Block<'_> {
    Block::bordered()
        .border_style(Style::default().fg(ACCENT))
        .title(Line::from(vec![
            Span::styled(
                " dupefind ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("— {title} "), Style::default().fg(DIM)),
        ]))
}

/// Render the pre-packed keybinding cheatsheet into `area`.
pub fn draw_footer(frame: &mut Frame, area: Rect, key_lines: Vec<Line>, status: Option<&str>) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM));

    // A transient status message takes over the footer, since it is always more
    // urgent than the cheatsheet the user has already read.
    let content: Vec<Line> = match status {
        Some(msg) => vec![Line::from(Span::styled(
            msg.to_string(),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ))],
        None => key_lines,
    };

    frame.render_widget(Paragraph::new(content).block(block), area);
}

/// Greedily pack `label value` statistics into lines that fit `width`, so a
/// narrow terminal reflows them instead of clipping the last one.
pub fn pack_stats(pairs: Vec<(&str, String)>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || pairs.is_empty() {
        return vec![Line::default()];
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    for (label, value) in pairs {
        let sep = if spans.is_empty() { 0 } else { 3 };
        let cost = sep + label.chars().count() + 1 + value.chars().count();

        if !spans.is_empty() && used + cost > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            used = 0;
        }
        if !spans.is_empty() {
            spans.push(Span::styled(" \u{b7} ", Style::default().fg(DIM)));
            used += 3;
        }
        used += label.chars().count() + 1 + value.chars().count();
        spans.push(Span::styled(format!("{label} "), Style::default().fg(DIM)));
        spans.push(Span::styled(
            value,
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }

    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

/// A labelled statistic, e.g. `files 12,481`.
pub fn stat<'a>(label: &'a str, value: String) -> Vec<Span<'a>> {
    vec![
        Span::styled(format!("{label} "), Style::default().fg(DIM)),
        Span::styled(value, Style::default().add_modifier(Modifier::BOLD)),
    ]
}

/// Join stat groups with a separator.
pub fn stat_line<'a>(groups: Vec<Vec<Span<'a>>>) -> Line<'a> {
    let mut spans = Vec::new();
    for (i, mut group) in groups.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::default().fg(DIM)));
        }
        spans.append(&mut group);
    }
    Line::from(spans)
}

/// Centre a fixed-size box inside `area`, for modals.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    use crate::app::{App, Pane};
    use crate::delete::{DeleteFailure, DeleteMode, DeleteReport};
    use crate::model::{DupeGroup, FileEntry, Phase, ScanOptions};

    fn app_with_results() -> App {
        let mut app = App::new(
            std::env::temp_dir(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.scan_root = Some(PathBuf::from("/home/example/pictures"));
        app.groups = vec![
            DupeGroup::new(
                [1u8; 32],
                4 * 1024 * 1024,
                vec![
                    FileEntry::new(
                        PathBuf::from("/home/example/pictures/2019/IMG_0421.JPG"),
                        4 * 1024 * 1024,
                        None,
                    ),
                    FileEntry::new(
                        PathBuf::from("/home/example/downloads/IMG_0421.JPG"),
                        4 * 1024 * 1024,
                        None,
                    ),
                    FileEntry::new(
                        PathBuf::from("/home/example/backup/old/IMG_0421.JPG"),
                        4 * 1024 * 1024,
                        None,
                    ),
                ],
            ),
            DupeGroup::new(
                [2u8; 32],
                118 * 1024 * 1024,
                vec![
                    FileEntry::new(
                        PathBuf::from("/home/example/backup.tar.gz"),
                        118 * 1024 * 1024,
                        None,
                    ),
                    FileEntry::new(
                        PathBuf::from("/mnt/spare/backup.tar.gz"),
                        118 * 1024 * 1024,
                        None,
                    ),
                ],
            ),
        ];
        app.screen = Screen::Results;
        app
    }

    /// Draw at a given size and return the rendered text, one string per row.
    fn render(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    fn rendered_text(app: &mut App, width: u16, height: u16) -> String {
        render(app, width, height).join("\n")
    }

    #[test]
    fn every_screen_renders_without_panicking() {
        let screens = [
            Screen::Picker,
            Screen::Scanning,
            Screen::Results,
            Screen::Confirm,
            Screen::Deleting,
            Screen::Done,
        ];
        for screen in screens {
            let mut app = app_with_results();
            app.screen = screen;
            app.report = Some(DeleteReport {
                deleted: 3,
                bytes_freed: 1024,
                failures: vec![DeleteFailure {
                    path: PathBuf::from("/locked/file.bin"),
                    message: "permission denied".into(),
                }],
                mode_label: "Trash".into(),
                deleted_paths: vec![PathBuf::from("/gone/a.bin")],
            });
            let _ = rendered_text(&mut app, 100, 30);
        }
    }

    /// Terminals get resized to absurd shapes; the layout must not panic or
    /// overflow at any of them.
    #[test]
    fn extreme_terminal_sizes_do_not_panic() {
        let sizes = [(1u16, 1u16), (4, 3), (12, 6), (40, 10), (200, 60), (300, 8)];
        for screen in [
            Screen::Picker,
            Screen::Scanning,
            Screen::Results,
            Screen::Confirm,
            Screen::Deleting,
            Screen::Done,
        ] {
            for (w, h) in sizes {
                let mut app = app_with_results();
                app.screen = screen;
                app.report = Some(DeleteReport::default());
                let rows = render(&mut app, w, h);
                assert_eq!(rows.len(), h as usize, "{screen:?} at {w}x{h}");
                for row in rows {
                    assert_eq!(
                        row.chars().count(),
                        w as usize,
                        "{screen:?} at {w}x{h} produced a row of the wrong width"
                    );
                }
            }
        }
    }

    #[test]
    fn the_results_screen_shows_paths_sizes_and_marks() {
        let mut app = app_with_results();
        let text = rendered_text(&mut app, 120, 30);

        // The dashboard must show the group sizes and the reclaimable total.
        assert!(text.contains("118"), "group size missing:\n{text}");
        assert!(text.contains("groups"), "group count label missing");
        assert!(text.contains("reclaims"), "reclaimable total missing");
        // And the keep/delete decision for the selected group.
        assert!(text.contains("KEEP"), "keep marker missing:\n{text}");
        assert!(text.contains("DELETE"), "delete marker missing:\n{text}");
    }

    #[test]
    fn the_files_pane_shows_the_full_path_of_each_duplicate() {
        let mut app = app_with_results();
        app.group_selected = 1;
        let text = rendered_text(&mut app, 120, 30);
        // Both copies of the selected group must be visible by path.
        assert!(text.contains("backup.tar.gz"), "paths missing:\n{text}");
        assert!(text.contains("/mnt/spare"), "second path missing:\n{text}");
    }

    #[test]
    fn permanent_mode_is_called_out_in_the_confirmation() {
        let mut app = app_with_results();
        app.delete_mode = DeleteMode::Permanent;
        app.screen = Screen::Confirm;
        let text = rendered_text(&mut app, 100, 30);
        assert!(
            text.contains("PERMANENT") || text.contains("permanently"),
            "a permanent delete must be labelled as such:\n{text}"
        );
    }

    /// The single-file confirmation must name the file, since which file it is
    /// is the only thing the user needs to check.
    #[test]
    fn the_single_delete_confirmation_names_the_file() {
        let mut app = app_with_results();
        app.plan = crate::app::DeletePlan::Single { group: 0, file: 1 };
        app.screen = Screen::Confirm;
        let text = rendered_text(&mut app, 100, 30);

        assert!(
            text.contains("this one file"),
            "should be worded as a single delete:\n{text}"
        );
        assert!(
            text.contains("IMG_0421.JPG"),
            "the target file must be named:\n{text}"
        );
        assert!(
            text.contains("would remain"),
            "should say how many copies survive:\n{text}"
        );
    }

    #[test]
    fn a_permanent_single_delete_is_called_out() {
        let mut app = app_with_results();
        app.plan = crate::app::DeletePlan::Single { group: 0, file: 1 };
        app.delete_mode = DeleteMode::Permanent;
        app.screen = Screen::Confirm;
        let text = rendered_text(&mut app, 100, 30);
        assert!(
            text.contains("PERMANENTLY") || text.contains("permanently"),
            "permanent single deletes must be labelled:\n{text}"
        );
    }

    #[test]
    fn trash_mode_promises_recoverability() {
        let mut app = app_with_results();
        app.screen = Screen::Confirm;
        let text = rendered_text(&mut app, 100, 30);
        assert!(
            text.contains("restored") || text.contains("Recycle"),
            "trash mode should say the files can come back:\n{text}"
        );
    }

    /// The scope must be legible before D is pressed, and again in the
    /// confirmation, so a narrowed delete is never mistaken for a full one.
    #[test]
    fn the_header_states_the_selection_scope() {
        let mut app = app_with_results();
        let text = rendered_text(&mut app, 120, 30);
        assert!(
            text.contains("all 2 groups"),
            "with no selection the scope should say so:\n{text}"
        );

        app.selected.insert(app.groups[0].hash);
        let text = rendered_text(&mut app, 120, 30);
        assert!(
            text.contains("1 selected"),
            "a selection must be stated in the header:\n{text}"
        );
    }

    #[test]
    fn selected_groups_carry_a_marker() {
        let mut app = app_with_results();
        let before = rendered_text(&mut app, 120, 30);
        app.selected.insert(app.groups[0].hash);
        let after = rendered_text(&mut app, 120, 30);
        assert_ne!(before, after, "selecting a group must change the list");
        assert!(
            after.contains('\u{2713}'),
            "a selected row needs a visible tick:\n{after}"
        );
        assert!(
            !before.contains('\u{2713}'),
            "an unselected list must carry no tick:\n{before}"
        );
    }

    #[test]
    fn the_confirmation_says_the_delete_is_narrowed() {
        let mut app = app_with_results();
        app.selected.insert(app.groups[0].hash);
        app.screen = Screen::Confirm;
        let text = rendered_text(&mut app, 100, 30);
        assert!(
            text.contains("selected group"),
            "the confirmation must repeat the narrowed scope:\n{text}"
        );
    }

    #[test]
    fn selection_markers_do_not_shift_the_columns() {
        let mut app = app_with_results();
        let unselected = render(&mut app, 120, 30);
        app.selected.insert(app.groups[0].hash);
        let selected = render(&mut app, 120, 30);
        // Every row keeps the same width; the marker lives in its own gutter.
        for (a, b) in unselected.iter().zip(selected.iter()) {
            assert_eq!(a.chars().count(), b.chars().count());
        }
    }

    #[test]
    fn an_empty_result_set_renders_a_message_not_a_crash() {
        let mut app = app_with_results();
        app.groups.clear();
        let text = rendered_text(&mut app, 80, 20);
        assert!(
            text.contains("no duplicate"),
            "expected an empty state:\n{text}"
        );
    }

    #[test]
    fn the_scanning_screen_reports_live_counters() {
        let mut app = app_with_results();
        app.screen = Screen::Scanning;
        app.phase = Phase::FullHashing;
        crate::model::ScanState::bump(&app.scan_state.files_seen, 12_481);
        let text = rendered_text(&mut app, 100, 30);
        assert!(text.contains("12,481"), "file count missing:\n{text}");
        assert!(text.contains("Hashing"), "phase label missing:\n{text}");
    }

    /// Focus is conveyed by style (border colour, highlight), not by different
    /// characters, so this compares whole cells rather than just the symbols.
    fn render_buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// The results cheatsheet is longer than one line at any sane width, so the
    /// footer must wrap rather than clip the trailing keys.
    #[test]
    fn the_footer_shows_every_keybinding_including_the_last() {
        for width in [80u16, 100, 120, 160] {
            let mut app = app_with_results();
            let text = rendered_text(&mut app, width, 34);
            assert!(
                text.contains("quit"),
                "the last keybinding is clipped at width {width}:\n{text}"
            );
            assert!(
                text.contains("delete marked"),
                "the delete keybinding is clipped at width {width}:\n{text}"
            );
        }
    }

    /// Every panel must reflow rather than clip at the narrowest width people
    /// actually use.
    #[test]
    fn nothing_is_clipped_at_eighty_columns() {
        let mut app = app_with_results();
        let text = rendered_text(&mut app, 80, 32);
        for needle in ["blake3", "wasted", "modified", "quit", "delete marked"] {
            assert!(
                text.contains(needle),
                "{needle:?} was clipped at 80 columns:\n{text}"
            );
        }
    }

    /// The footer advertises keys 1-5 for the scan filters, so their current
    /// state has to be on screen.
    #[test]
    fn the_picker_shows_the_filter_states_it_lets_you_toggle() {
        let mut app = app_with_results();
        app.screen = Screen::Picker;
        let text = rendered_text(&mut app, 100, 20);
        for needle in ["hidden", "gitignore", "empty", "hardlinks", "cache"] {
            assert!(
                text.contains(needle),
                "the {needle:?} filter state is not visible:\n{text}"
            );
        }
    }

    /// The footer must name the directory `s` would scan, so the target is never
    /// ambiguous while navigating.
    /// Footer key labels must not use glyphs whose rendered width disagrees
    /// with unicode-width. U+232B and U+2B06 in particular measure as one cell
    /// but most fonts draw them two cells wide, which swallows the space after
    /// them and produces output like "Bkspup". Plain ASCII cannot misalign; the
    /// bare arrows are allowed because they render narrow in practice.
    #[test]
    fn footer_key_labels_avoid_ambiguous_width_glyphs() {
        const ALLOWED_NON_ASCII: &[char] = &['\u{2191}', '\u{2193}'];

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
            for (key, desc) in footer_keys(&app) {
                for c in key.chars().chain(desc.chars()) {
                    assert!(
                        c.is_ascii() || ALLOWED_NON_ASCII.contains(&c),
                        "{screen:?}: key label {key:?}/{desc:?} uses U+{:04X}, whose \
                         rendered width is unreliable",
                        c as u32
                    );
                }
            }
        }
    }

    #[test]
    fn the_picker_footer_names_the_scan_target() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("holidays")).unwrap();

        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.screen = Screen::Picker;
        app.picker_selected = app
            .entries
            .iter()
            .position(|r| r.name == "holidays")
            .expect("the directory should be listed");

        let text = rendered_text(&mut app, 100, 20);
        assert!(
            text.contains("SCAN holidays"),
            "the footer should name the highlighted directory:\n{text}"
        );
    }

    /// Files are listed for context, with their size, so you can see what you
    /// are about to point the scanner at.
    /// The counts describe the directory's contents, so the navigation rows the
    /// browser adds must not inflate them.
    #[test]
    fn the_title_counts_contents_not_navigation_rows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("one")).unwrap();
        std::fs::create_dir(dir.path().join("two")).unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.screen = Screen::Picker;
        let text = rendered_text(&mut app, 100, 20);

        // Four rows are listed -- action, .., two dirs, one file -- but only the
        // two directories and one file are contents.
        assert!(
            text.contains("2 directories") && text.contains("1 files"),
            "counts must exclude the action and parent rows:\n{text}"
        );
    }

    #[test]
    fn the_picker_lists_files_with_their_sizes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("photos")).unwrap();
        std::fs::write(dir.path().join("holiday.mp4"), vec![0u8; 3 * 1024 * 1024]).unwrap();

        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.screen = Screen::Picker;
        let text = rendered_text(&mut app, 100, 20);

        assert!(text.contains("photos"), "directory missing:\n{text}");
        assert!(text.contains("holiday.mp4"), "file missing:\n{text}");
        assert!(text.contains("3 MiB"), "file size missing:\n{text}");
        assert!(
            text.contains("1 directories") && text.contains("1 files"),
            "the title should count both:\n{text}"
        );
    }

    /// Drive rows must be visually distinct from subdirectories, since entering
    /// one moves to another disk rather than descending the current tree.
    #[test]
    fn drive_rows_render_as_drives() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.screen = Screen::Picker;
        app.entries = crate::app::App::drive_rows(
            &[
                std::path::PathBuf::from("C:\\"),
                std::path::PathBuf::from("D:\\"),
            ],
            std::path::Path::new("C:\\"),
        );
        let text = rendered_text(&mut app, 100, 20);
        assert!(
            text.contains("D:"),
            "the other drive should be listed:\n{text}"
        );
        assert!(
            text.contains("(drive)"),
            "a drive should be labelled as one:\n{text}"
        );
        assert!(
            text.contains("1 drives"),
            "the title should count drives:\n{text}"
        );
    }

    /// Scanning the directory you are in has to be visible in the list, not
    /// just reachable: at a drive root there is no `..` to fall back on.
    #[test]
    fn the_first_row_offers_to_scan_the_current_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let mut app = App::new(
            dir.path().to_path_buf(),
            ScanOptions::default(),
            DeleteMode::Trash,
        );
        app.screen = Screen::Picker;

        assert_eq!(app.picker_selected, 0, "it starts highlighted");
        let text = rendered_text(&mut app, 100, 20);
        assert!(
            text.contains("scan all of"),
            "the action row must say what it does:\n{text}"
        );
        // And it names the directory, not a bare "." that means nothing on
        // Windows.
        let leaf = dir
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .expect("tempdir has a name");
        assert!(
            text.contains(&leaf),
            "the action row should name the directory ({leaf}):\n{text}"
        );
    }

    /// The rate must stay blank until reading starts, then divide by reading
    /// time. Dividing by total scan time made it understate the disk badly.
    #[test]
    fn the_read_rate_waits_for_reading_to_begin() {
        let mut app = app_with_results();
        app.screen = Screen::Scanning;
        app.phase = Phase::Walking;
        // A scan that started 10s ago and has read 1 MiB.
        app.scan_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(10));
        crate::model::ScanState::bump(&app.scan_state.bytes_hashed, 1024 * 1024);

        // Nothing has been read yet, so there is no rate to report -- even
        // though a byte counter exists.
        let text = rendered_text(&mut app, 100, 30);
        assert!(
            text.contains("read rate"),
            "the label should be shown:\n{text}"
        );
        assert!(
            !text.contains("MiB/s"),
            "no rate before reading starts:\n{text}"
        );

        // Reading began 2s into the scan, so 8s of the 10s was reading.
        app.scan_state
            .mark_hashing_started(std::time::Duration::from_secs(2));
        let text = rendered_text(&mut app, 100, 30);
        assert!(
            text.contains("/s"),
            "a rate should appear once reading has begun:\n{text}"
        );
    }

    /// Finalizing opens every grouped file, so it needs its own progress. Left on
    /// the hashing numbers the gauge sits at 100% for minutes and reads as a hang.
    #[test]
    fn the_gauge_follows_the_finalize_phase() {
        let mut app = app_with_results();
        app.screen = Screen::Scanning;
        app.phase = Phase::FullHashing;
        crate::model::ScanState::bump(&app.scan_state.files_hashed, 900);
        crate::model::ScanState::bump(&app.scan_state.candidates, 900);
        crate::model::ScanState::bump(&app.scan_state.files_checked, 4);
        crate::model::ScanState::bump(&app.scan_state.files_to_check, 40);

        let text = rendered_text(&mut app, 100, 30);
        assert!(
            text.contains("900 / 900 candidates hashed"),
            "hashing reports candidates:\n{text}"
        );

        app.phase = Phase::Finalizing;
        let text = rendered_text(&mut app, 100, 30);
        assert!(
            text.contains("4 / 40 files identified"),
            "finalizing reports its own files, not a full hashing gauge:\n{text}"
        );
    }

    #[test]
    fn the_focused_pane_is_visually_distinct() {
        let mut app = app_with_results();
        app.pane = Pane::Groups;
        let groups_focused = render_buffer(&mut app, 100, 30);
        app.pane = Pane::Files;
        let files_focused = render_buffer(&mut app, 100, 30);
        assert_ne!(
            groups_focused, files_focused,
            "switching panes must change the rendered styles"
        );
    }

    #[test]
    fn the_focused_pane_border_uses_the_accent_colour() {
        let mut app = app_with_results();
        app.pane = Pane::Groups;
        let buffer = render_buffer(&mut app, 100, 30);

        // The groups pane starts at the left edge of the body band.
        let body_top = 4;
        let left_border = buffer[(0, body_top)].style();
        assert_eq!(
            left_border.fg,
            Some(ACCENT),
            "the focused pane border should be drawn in the accent colour"
        );
    }
}
