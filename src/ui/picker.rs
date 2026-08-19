//! Directory browser: choose the tree to scan.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use super::{ACCENT, DELETE, DIM, KEEP, header_block, stat, stat_line};
use crate::app::{App, DirEntryRow, RowKind};
use crate::format;

pub fn draw(frame: &mut Frame, app: &mut App, header: Rect, body: Rect) {
    // Header: where we are, and what the scan would apply.
    let filters = stat_line(vec![
        toggle("hidden", !app.options.skip_hidden),
        toggle("gitignore", app.options.respect_gitignore),
        toggle("empty", !app.options.skip_empty),
        toggle("hardlinks", app.options.collapse_hardlinks),
    ]);

    let path = format::truncate_path(
        &app.cwd.to_string_lossy(),
        header.width.saturating_sub(7) as usize,
    );

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("in ", Style::default().fg(DIM)),
                Span::styled(
                    path,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ]),
            filters,
        ])
        .block(header_block("choose a directory to scan")),
        header,
    );

    // Body: subdirectories of the current directory.
    let inner_width = body.width.saturating_sub(2) as usize;
    let items: Vec<ListItem> = if let Some(err) = &app.picker_error {
        vec![ListItem::new(Line::from(Span::styled(
            format!("cannot read this directory: {err}"),
            Style::default().fg(DELETE),
        )))]
    } else if app.entries.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "this directory is empty — press s to scan it",
            Style::default().fg(DIM),
        )))]
    } else {
        app.entries
            .iter()
            .map(|row| row_line(row, inner_width))
            .map(ListItem::new)
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .title(Line::from(Span::styled(
                    contents_title(app),
                    Style::default().fg(DIM),
                ))),
        )
        .highlight_style(
            Style::default()
                .bg(ACCENT)
                .fg(ratatui::style::Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("")
        .scroll_padding(2);

    let mut state = ListState::default().with_selected(Some(app.picker_selected));
    frame.render_stateful_widget(list, body, &mut state);
}

/// One browser row.
///
/// Directories are the actionable rows and keep full contrast; files are listed
/// for context only, so they are dimmed and carry their size on the right.
fn row_line(row: &DirEntryRow, width: usize) -> Line<'static> {
    match row.kind {
        // The primary action, so it is the brightest row and states the path in
        // full rather than relying on a convention like "." that means nothing
        // on Windows.
        RowKind::Current => {
            let open = "[ scan all of ";
            let close = " ]";
            let budget = width.saturating_sub(open.chars().count() + close.chars().count());
            Line::from(vec![
                Span::styled(open, Style::default().fg(KEEP)),
                Span::styled(
                    format::truncate_path(&row.name, budget),
                    Style::default().fg(KEEP).add_modifier(Modifier::BOLD),
                ),
                Span::styled(close, Style::default().fg(KEEP)),
            ])
        }

        RowKind::Parent => Line::from(vec![
            Span::styled("^ ", Style::default().fg(DIM)),
            Span::styled("..", Style::default().fg(DIM)),
        ]),

        // A jump to another disk rather than a step down the tree, so it is
        // labelled instead of looking like an ordinary subdirectory.
        RowKind::Drive => {
            let suffix = " (drive)";
            Line::from(vec![
                Span::styled("> ", Style::default().fg(ACCENT)),
                Span::styled(
                    format::truncate(&row.name, width.saturating_sub(2 + suffix.len())),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(suffix, Style::default().fg(DIM)),
            ])
        }

        RowKind::Directory => Line::from(vec![
            Span::styled("▸ ", Style::default().fg(ACCENT)),
            Span::styled(
                format::truncate(&row.name, width.saturating_sub(2)),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),

        // Context only: dimmed, with the size pushed to the right margin.
        RowKind::File => {
            let size = row
                .size
                .map(format::bytes)
                .unwrap_or_else(|| "—".to_string());
            let size_len = size.chars().count();
            // 2 for the indent, 1 for at least one space before the size.
            let name = format::truncate(&row.name, width.saturating_sub(size_len + 3));
            let pad = width
                .saturating_sub(2 + name.chars().count() + size_len)
                .max(1);
            Line::from(vec![
                Span::raw("  "),
                Span::styled(name, Style::default().fg(DIM)),
                Span::raw(" ".repeat(pad)),
                Span::styled(size, Style::default().fg(DIM)),
            ])
        }
    }
}

/// `4 directories · 11 files`, counting only the contents -- not the parent,
/// drive or current-directory rows, which are navigation rather than content.
fn contents_title(app: &App) -> String {
    let count = |kind: RowKind| app.entries.iter().filter(|r| r.kind == kind).count();
    let dirs = count(RowKind::Directory);
    let files = count(RowKind::File);
    let drives = count(RowKind::Drive);
    if drives > 0 {
        format!(" {drives} drives · {dirs} directories · {files} files ")
    } else {
        format!(" {dirs} directories · {files} files ")
    }
}

/// `name:on` / `name:off`, coloured so the active filter set is readable.
fn toggle<'a>(name: &'a str, on: bool) -> Vec<Span<'a>> {
    let mut spans = stat(name, if on { "on".into() } else { "off".into() });
    let colour = if on { KEEP } else { DIM };
    if let Some(last) = spans.last_mut() {
        *last = Span::styled(
            if on { "on" } else { "off" },
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        );
    }
    spans
}
