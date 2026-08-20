//! The dashboard: duplicate groups on the left, every path in the selected
//! group on the right with its keep/delete mark.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

use super::{ACCENT, DELETE, DIM, KEEP, header_block, pack_stats, stat, stat_line};
use crate::app::{App, Pane};
use crate::format;
use crate::model::DupeGroup;

pub fn draw(frame: &mut Frame, app: &mut App, header: Rect, body: Rect) {
    draw_header(frame, app, header);

    let [groups_area, files_area] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(body);

    draw_groups(frame, app, groups_area);
    draw_files(frame, app, files_area);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let root = app
        .scan_root
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let summary = stat_line(vec![
        stat("scanned", format::count(app.scanned_files())),
        stat("size", format::bytes(app.scanned_bytes())),
        stat("groups", format::count(app.groups.len() as u64)),
        stat("duplicates", format::count(app.total_duplicates() as u64)),
        stat("took", format::duration(app.elapsed())),
    ]);

    // The number that matters: what pressing D would actually free.
    let (marked, reclaimable) = app.marked_summary();
    let plan = Line::from(vec![
        Span::styled("marked ", Style::default().fg(DIM)),
        Span::styled(
            format::count(marked as u64),
            Style::default()
                .fg(if marked > 0 { DELETE } else { DIM })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · reclaims ", Style::default().fg(DIM)),
        Span::styled(
            format::bytes(reclaimable),
            Style::default()
                .fg(if marked > 0 { KEEP } else { DIM })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · mode ", Style::default().fg(DIM)),
        Span::styled(
            app.delete_mode.label(),
            Style::default()
                .fg(match app.delete_mode {
                    crate::delete::DeleteMode::Trash => KEEP,
                    crate::delete::DeleteMode::Permanent => DELETE,
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · sort {}", app.sort.label()),
            Style::default().fg(DIM),
        ),
        Span::styled(" · scope ", Style::default().fg(DIM)),
        if app.selected_count() > 0 {
            Span::styled(
                format!("{} selected", app.selected_count()),
                Style::default().fg(KEEP).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                format!("all {} groups", app.groups.len()),
                Style::default().fg(DIM),
            )
        },
    ]);

    frame.render_widget(
        Paragraph::new(vec![summary, plan]).block(header_block(&format::truncate_path(
            &root,
            area.width.saturating_sub(24) as usize,
        ))),
        area,
    );
}

fn draw_groups(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.pane == Pane::Groups;
    let inner_width = area.width.saturating_sub(4) as usize;
    let viewport = area.height.saturating_sub(2) as usize;
    let range = visible_window(
        app.groups.len(),
        app.group_selected,
        app.group_scroll,
        viewport,
        2,
    );
    app.group_scroll = range.start;

    let items: Vec<ListItem> = app
        .groups
        .get(range.clone())
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(relative_idx, g)| {
            let idx = range.start + relative_idx;
            // "118.0 MiB x4  backup.tar.gz", behind a two-column marker gutter.
            let size = format!("{:>10}", format::bytes(g.size));
            let count = format!(" ×{:<3}", g.files.len());
            let selected = app.is_selected(idx);
            // U+2713, the same Dingbats block and width class as the U+2717 the
            // files pane already renders correctly. Deliberately not U+2714
            // HEAVY CHECK MARK, which carries emoji presentation and is drawn
            // two cells wide while unicode-width measures it as one -- the bug
            // that made "Bksp up" run together.
            let marker = if selected { "✓ " } else { "  " };
            let used = marker.chars().count() + size.chars().count() + count.chars().count() + 1;
            let name = format::truncate(&g.label(), inner_width.saturating_sub(used));

            let name_style = if g.skipped {
                Style::default().fg(DIM).add_modifier(Modifier::CROSSED_OUT)
            } else {
                Style::default()
            };

            ListItem::new(Line::from(vec![
                Span::styled(
                    marker,
                    Style::default().fg(KEEP).add_modifier(Modifier::BOLD),
                ),
                Span::styled(size, Style::default().fg(ACCENT)),
                Span::styled(count, Style::default().fg(DIM)),
                Span::raw(" "),
                Span::styled(name, name_style),
            ]))
        })
        .collect();

    let title = format!(" groups ({}) ", app.groups.len());
    let list = List::new(items)
        .block(pane_block(&title, focused))
        .highlight_style(selection_style(focused))
        .highlight_symbol("")
        .scroll_padding(0);

    let mut state = ListState::default().with_selected(if app.groups.is_empty() {
        None
    } else {
        Some(app.group_selected.saturating_sub(range.start))
    });
    frame.render_stateful_widget(list, area, &mut state);

    draw_scrollbar(frame, area, app.groups.len(), app.group_selected);
}

/// Return the small slice of a large list that can actually be rendered.
/// `offset` is retained across frames so ordinary arrow movement scrolls only
/// when the cursor reaches the viewport padding.
fn visible_window(
    len: usize,
    selected: usize,
    offset: usize,
    viewport: usize,
    requested_padding: usize,
) -> std::ops::Range<usize> {
    if len == 0 || viewport == 0 {
        return 0..0;
    }
    let selected = selected.min(len - 1);
    let max_start = len.saturating_sub(viewport);
    let mut start = offset.min(max_start);
    let padding = requested_padding.min(viewport.saturating_sub(1) / 2);

    if selected < start.saturating_add(padding) {
        start = selected.saturating_sub(padding);
    } else {
        let last_comfortable = start.saturating_add(viewport).saturating_sub(padding + 1);
        if selected > last_comfortable {
            start = selected
                .saturating_add(padding + 1)
                .saturating_sub(viewport);
        }
    }
    start = start.min(max_start);
    start..start.saturating_add(viewport).min(len)
}

fn draw_files(frame: &mut Frame, app: &App, area: Rect) {
    let focused = app.pane == Pane::Files;

    let Some(group) = app.selected_group() else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no duplicate files found",
                Style::default().fg(DIM),
            )))
            .block(pane_block(" files ", focused)),
            area,
        );
        return;
    };

    // Size the detail box to the statistics it must show, so nothing is clipped
    // on a narrow terminal.
    let detail = detail_lines(group, app, area.width.saturating_sub(2) as usize);
    let detail_h = (detail.len() as u16 + 2).clamp(3, 7);
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(detail_h)]).areas(area);

    let inner_width = list_area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = group
        .files
        .iter()
        .map(|f| {
            let (mark, colour) = if f.protected {
                ("REF     ", ACCENT)
            } else if group.skipped {
                ("— SKIP  ", DIM)
            } else if f.keep {
                ("● KEEP  ", KEEP)
            } else {
                ("✗ DELETE", DELETE)
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    mark,
                    Style::default().fg(colour).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    format::truncate_path(
                        &format::relative_to(&f.path, app.scan_root.as_deref()),
                        inner_width.saturating_sub(9),
                    ),
                    if f.keep || group.skipped {
                        Style::default()
                    } else {
                        Style::default().fg(DIM)
                    },
                ),
            ]))
        })
        .collect();

    let title = format!(
        " group {} of {} — {} copies ",
        app.group_selected + 1,
        app.groups.len(),
        group.files.len()
    );

    let list = List::new(items)
        .block(pane_block(&title, focused))
        .highlight_style(selection_style(focused))
        .highlight_symbol("")
        .scroll_padding(1);

    let mut state = ListState::default().with_selected(Some(app.file_selected));
    frame.render_stateful_widget(list, list_area, &mut state);
    draw_scrollbar(frame, list_area, group.files.len(), app.file_selected);

    frame.render_widget(
        Paragraph::new(detail).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM)),
        ),
        detail_area,
    );
}

/// Statistics for the selected group, packed to the available width.
fn detail_lines(group: &DupeGroup, app: &App, width: usize) -> Vec<Line<'static>> {
    let modified = group
        .files
        .get(app.file_selected)
        .map(|f| format::timestamp(f.modified))
        .unwrap_or_else(|| "\u{2014}".to_string());

    let mut pairs = vec![
        ("each", format::bytes(group.size)),
        ("wasted", format::bytes(group.wasted())),
    ];
    // `reclaims` only differs from `wasted` once the marks are customised, so
    // the narrow case does not spend columns repeating the same number.
    if group.reclaimable() != group.wasted() {
        pairs.push(("reclaims", format::bytes(group.reclaimable())));
    }
    if group
        .files
        .get(app.file_selected)
        .is_some_and(|file| file.protected)
    {
        pairs.push(("reference", "protected".to_string()));
    }
    pairs.push(("modified", modified));
    pairs.push(("blake3", group.hash_prefix()));

    pack_stats(pairs, width)
}

/// Only draw a scrollbar when the content actually overflows, so short lists
/// are not decorated with a full-height thumb that means nothing.
fn draw_scrollbar(frame: &mut Frame, area: Rect, len: usize, position: usize) {
    let viewport = area.height.saturating_sub(2) as usize;
    if len <= viewport {
        return;
    }
    let mut state = ScrollbarState::new(len).position(position);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .thumb_style(Style::default().fg(ACCENT))
            .track_style(Style::default().fg(DIM)),
        area,
        &mut state,
    );
}

fn pane_block<'a>(title: &'a str, focused: bool) -> Block<'a> {
    let colour = if focused { ACCENT } else { DIM };
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colour))
        .title(Line::from(Span::styled(
            title,
            if focused {
                Style::default().fg(colour).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(colour)
            },
        )))
}

/// The unfocused pane keeps a visible but muted cursor, so the user does not
/// lose their place when switching panes.
fn selection_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .bg(ACCENT)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::DIM)
    }
}

#[cfg(test)]
mod virtualization_tests {
    use super::visible_window;

    #[test]
    fn a_large_list_only_exposes_one_viewport() {
        let range = visible_window(106_235, 50_000, 0, 40, 2);
        assert_eq!(range.len(), 40);
        assert!(range.contains(&50_000));
    }

    #[test]
    fn retained_offset_moves_only_at_the_padding() {
        assert_eq!(visible_window(100, 11, 10, 10, 2), 9..19);
        assert_eq!(visible_window(100, 16, 10, 10, 2), 10..20);
        assert_eq!(visible_window(100, 18, 10, 10, 2), 11..21);
    }

    #[test]
    fn tiny_and_empty_viewports_are_safe() {
        assert_eq!(visible_window(100, 99, 0, 0, 2), 0..0);
        assert_eq!(visible_window(100, 99, 0, 1, 2), 99..100);
        assert_eq!(visible_window(0, 0, 0, 20, 2), 0..0);
    }
}
