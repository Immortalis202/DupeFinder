//! Overlays: the deletion confirmation and the final report.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use super::{ACCENT, DELETE, DIM, KEEP, WARN, centered, header_block, stat, stat_line};
use crate::app::{App, DeletePlan};
use crate::delete::DeleteMode;
use crate::format;

/// The point of no return. States exactly what will happen, in the terms the
/// user cares about: how many files, how much space, and whether it is undoable.
pub fn draw_confirm(frame: &mut Frame, app: &App) {
    let permanent = app.delete_mode == DeleteMode::Permanent;

    // A single-file delete gets its own wording: the one thing that matters is
    // which file, so it is named in full rather than counted.
    if let DeletePlan::Single { group, file } = app.plan {
        let target = app
            .groups
            .get(group)
            .and_then(|g| g.files.get(file))
            .map(|f| (f.path.to_string_lossy().into_owned(), f.size));
        draw_single_confirm(frame, app, target, permanent);
        return;
    }

    let area = centered(frame.area(), 68, 13);
    // Clear first: the dashboard underneath must not bleed through.
    frame.render_widget(Clear, area);

    let count = app.total_marked();
    let bytes = app.total_reclaimable();

    let (verdict, verdict_style) = if permanent {
        (
            "These files will be destroyed permanently and cannot be recovered.",
            Style::default().fg(DELETE).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "These files move to the Recycle Bin / Trash and can be restored.",
            Style::default().fg(KEEP),
        )
    };

    let group_count = app
        .groups
        .iter()
        .enumerate()
        .filter(|(idx, g)| !g.skipped && g.marked() > 0 && app.in_scope(*idx))
        .count();

    let body = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Delete ", Style::default()),
            Span::styled(
                format::count(count as u64),
                Style::default().fg(DELETE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" file{} from ", if count == 1 { "" } else { "s" }),
                Style::default(),
            ),
            Span::styled(
                format::count(group_count as u64),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " {}group{}?",
                    if app.selected_count() > 0 {
                        "selected "
                    } else {
                        ""
                    },
                    if group_count == 1 { "" } else { "s" }
                ),
                Style::default(),
            ),
        ]),
        Line::from(vec![
            Span::styled("Reclaims ", Style::default().fg(DIM)),
            Span::styled(
                format::bytes(bytes),
                Style::default().fg(KEEP).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  ·  one copy is kept in every group",
                Style::default().fg(DIM),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(verdict, verdict_style)),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "y / Enter",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  confirm     ", Style::default().fg(DIM)),
            Span::styled(
                "t",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  switch to ", Style::default().fg(DIM)),
            Span::styled(app.delete_mode.toggled().label(), Style::default().fg(WARN)),
            Span::styled("     ", Style::default()),
            Span::styled(
                "Esc",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  cancel", Style::default().fg(DIM)),
        ]),
    ];

    let border = if permanent { DELETE } else { WARN };
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: true }).block(
            Block::bordered()
                .border_style(Style::default().fg(border).add_modifier(Modifier::BOLD))
                .title(Line::from(Span::styled(
                    if permanent {
                        " confirm PERMANENT deletion "
                    } else {
                        " confirm deletion "
                    },
                    Style::default().fg(border).add_modifier(Modifier::BOLD),
                ))),
        ),
        area,
    );
}

/// Confirmation for deleting one specific file.
fn draw_single_confirm(
    frame: &mut Frame,
    app: &App,
    target: Option<(String, u64)>,
    permanent: bool,
) {
    let area = centered(frame.area(), 72, 12);
    frame.render_widget(Clear, area);

    let Some((path, size)) = target else {
        return;
    };
    let remaining = app
        .groups
        .get(match app.plan {
            DeletePlan::Single { group, .. } => group,
            DeletePlan::Marked => usize::MAX,
        })
        .map(|g| g.files.len().saturating_sub(1))
        .unwrap_or(0);

    let (verdict, verdict_style) = if permanent {
        (
            "This file will be destroyed permanently and cannot be recovered.",
            Style::default().fg(DELETE).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            "It moves to the Recycle Bin / Trash and can be restored.",
            Style::default().fg(KEEP),
        )
    };

    let body = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Delete this one file?",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format::truncate_path(&path, 68),
            Style::default().fg(DELETE),
        )),
        Line::from(vec![
            Span::styled("Frees ", Style::default().fg(DIM)),
            Span::styled(
                format::bytes(size),
                Style::default().fg(KEEP).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  ·  {} cop{} of this content would remain",
                    remaining,
                    if remaining == 1 { "y" } else { "ies" }
                ),
                Style::default().fg(DIM),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(verdict, verdict_style)),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "y / Enter",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  delete it     ", Style::default().fg(DIM)),
            Span::styled(
                "t",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  switch to ", Style::default().fg(DIM)),
            Span::styled(app.delete_mode.toggled().label(), Style::default().fg(WARN)),
            Span::styled("     ", Style::default()),
            Span::styled(
                "Esc",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  cancel", Style::default().fg(DIM)),
        ]),
    ];

    let border = if permanent { DELETE } else { WARN };
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: true }).block(
            Block::bordered()
                .border_style(Style::default().fg(border).add_modifier(Modifier::BOLD))
                .title(Line::from(Span::styled(
                    if permanent {
                        " delete file PERMANENTLY "
                    } else {
                        " delete file "
                    },
                    Style::default().fg(border).add_modifier(Modifier::BOLD),
                ))),
        ),
        area,
    );
}

/// The post-deletion report, including every failure.
pub fn draw_done(frame: &mut Frame, app: &mut App, header: Rect, body: Rect) {
    let report = app.report.clone().unwrap_or_default();

    frame.render_widget(
        Paragraph::new(stat_line(vec![
            stat("deleted", format::count(report.deleted)),
            stat("freed", format::bytes(report.bytes_freed)),
            stat("failed", format::count(report.failures.len() as u64)),
            stat("mode", report.mode_label.clone()),
        ]))
        .block(header_block("deletion complete")),
        header,
    );

    let [summary_area, failures_area] =
        Layout::vertical([Constraint::Length(5), Constraint::Min(3)]).areas(body);

    let headline = if report.failures.is_empty() {
        Line::from(Span::styled(
            format!(
                "Removed {} duplicate file{}, reclaiming {}.",
                format::count(report.deleted),
                if report.deleted == 1 { "" } else { "s" },
                format::bytes(report.bytes_freed)
            ),
            Style::default().fg(KEEP).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            format!(
                "Removed {}, but {} file{} could not be deleted.",
                format::count(report.deleted),
                format::count(report.failures.len() as u64),
                if report.failures.len() == 1 { "" } else { "s" }
            ),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ))
    };

    let mut summary = vec![headline, Line::from("")];
    if !report.failures.is_empty() && report.mode_label == DeleteMode::Trash.label() {
        summary.push(Line::from(Span::styled(
            "Trashing fails for files outside the home filesystem — press t on the \
             dashboard to switch to permanent deletion and retry.",
            Style::default().fg(DIM),
        )));
    }
    summary.push(Line::from(Span::styled(
        format!("{} duplicate group(s) still listed.", app.groups.len()),
        Style::default().fg(DIM),
    )));

    frame.render_widget(
        Paragraph::new(summary).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM)),
        ),
        summary_area,
    );

    let items: Vec<ListItem> = report
        .failures
        .iter()
        .map(|f| {
            ListItem::new(vec![
                Line::from(Span::styled(
                    format::truncate_path(
                        &f.path.to_string_lossy(),
                        failures_area.width.saturating_sub(4) as usize,
                    ),
                    Style::default().fg(DELETE),
                )),
                Line::from(Span::styled(
                    format!("  {}", f.message),
                    Style::default().fg(DIM),
                )),
            ])
        })
        .collect();

    let title = if report.failures.is_empty() {
        " no failures ".to_string()
    } else {
        format!(" failures ({}) ", report.failures.len())
    };

    let mut state = ListState::default().with_selected(if report.failures.is_empty() {
        None
    } else {
        Some(app.failure_scroll)
    });

    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(DIM))
                    .title(Line::from(Span::styled(title, Style::default().fg(DIM)))),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        failures_area,
        &mut state,
    );
}
