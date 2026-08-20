//! Live progress: the scan screen and the deletion screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph};

use super::{ACCENT, DELETE, DIM, KEEP, WARN, header_block, stat, stat_line};
use crate::app::App;
use crate::format;
use crate::model::{Phase, ScanState};

pub fn draw(frame: &mut Frame, app: &mut App, header: Rect, body: Rect) {
    let state = &app.scan_state;
    let elapsed = app.elapsed();

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                app.phase.label(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  {}  ",
                    app.scan_root
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ),
                Style::default().fg(DIM),
            ),
        ]))
        .block(header_block("scanning")),
        header,
    );

    let [gauge_area, stats_area, current_area, errors_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(6),
        Constraint::Length(3),
        Constraint::Min(3),
    ])
    .areas(body);

    // Progress is only meaningful once the candidate set is known; during the
    // walk we cannot know the denominator, so the gauge shows activity instead.
    //
    // Finalizing gets its own numbers: by then every candidate has been hashed,
    // so the hashing gauge would sit at 100% while that phase opens every file
    // in every group -- minutes of work on a slow disk, looking like a hang.
    let (done, total, noun) = match app.phase {
        Phase::Finalizing => {
            let (checked, to_check) = app.finalize_progress();
            (checked, to_check, "files identified")
        }
        _ => {
            let (hashed, candidates) = app.hash_progress();
            (hashed, candidates, "candidates hashed")
        }
    };
    let ratio = if total > 0 {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let label = if total > 0 {
        format!("{} / {} {noun}", format::count(done), format::count(total))
    } else {
        "discovering files…".to_string()
    };

    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(DIM)),
            )
            .gauge_style(Style::default().fg(ACCENT))
            .ratio(ratio)
            .label(label),
        gauge_area,
    );

    let errors = ScanState::get(&state.errors);
    let lines = vec![
        stat_line(vec![
            stat("files", format::count(ScanState::get(&state.files_seen))),
            stat("dirs", format::count(ScanState::get(&state.dirs_seen))),
            stat(
                "skipped",
                format::count(ScanState::get(&state.files_skipped)),
            ),
        ]),
        stat_line(vec![
            stat("size", format::bytes(ScanState::get(&state.bytes_seen))),
            stat(
                "candidates",
                format::count(ScanState::get(&state.candidates)),
            ),
        ]),
        stat_line(vec![
            stat("hashed", format::bytes(ScanState::get(&state.bytes_hashed))),
            // Divide by time spent reading, not by total scan time: the walk
            // stats every file in the tree and can easily outlast the reading,
            // which made this figure read far below what the disk delivers.
            stat(
                "read rate",
                match state.hashing_elapsed(elapsed) {
                    Some(reading) => {
                        format::throughput(ScanState::get(&state.bytes_hashed), reading)
                    }
                    None => "—".to_string(),
                },
            ),
        ]),
        stat_line(vec![
            stat("elapsed", format::duration(elapsed)),
            vec![
                Span::styled("errors ", Style::default().fg(DIM)),
                Span::styled(
                    format::count(errors),
                    Style::default()
                        .fg(if errors > 0 { WARN } else { KEEP })
                        .add_modifier(Modifier::BOLD),
                ),
            ],
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .title(Line::from(Span::styled(
                    " statistics ",
                    Style::default().fg(DIM),
                ))),
        ),
        stats_area,
    );

    let current = state.current_path();
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format::truncate_path(&current, current_area.width.saturating_sub(4) as usize),
            Style::default().fg(DIM),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .title(Line::from(Span::styled(
                    " current ",
                    Style::default().fg(DIM),
                ))),
        ),
        current_area,
    );

    draw_error_list(frame, app, errors_area);
}

/// The most recent unreadable paths. Bounded, and shown newest-last so the tail
/// is what stays visible.
fn draw_error_list(frame: &mut Frame, app: &App, area: Rect) {
    let capacity = area.height.saturating_sub(2) as usize;
    let start = app.errors.len().saturating_sub(capacity);
    let items: Vec<ListItem> = app.errors[start..]
        .iter()
        .map(|e| {
            ListItem::new(Line::from(Span::styled(
                format::truncate(&e.display(), area.width.saturating_sub(4) as usize),
                Style::default().fg(WARN),
            )))
        })
        .collect();

    let title = if app.errors.is_empty() {
        " skipped / unreadable ".to_string()
    } else {
        format!(" skipped / unreadable ({}) ", app.errors.len())
    };

    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .title(Line::from(Span::styled(title, Style::default().fg(DIM)))),
        ),
        area,
    );
}

/// The deletion progress screen.
pub fn draw_deleting(frame: &mut Frame, app: &mut App, header: Rect, body: Rect) {
    use std::sync::atomic::Ordering;

    let state = &app.delete_state;
    let done = state.done.load(Ordering::Relaxed);
    let total = state.total.load(Ordering::Relaxed);
    let freed = state.bytes_freed.load(Ordering::Relaxed);
    let failed = state.failed.load(Ordering::Relaxed);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("Deleting — {}", app.delete_mode.label()),
            Style::default().fg(DELETE).add_modifier(Modifier::BOLD),
        )))
        .block(header_block("removing duplicates")),
        header,
    );

    let [gauge_area, stats_area, current_area] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(4),
        Constraint::Min(3),
    ])
    .areas(body);

    let ratio = if total > 0 {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(DIM)),
            )
            .gauge_style(Style::default().fg(DELETE))
            .ratio(ratio)
            .label(format!(
                "{} / {}",
                format::count(done),
                format::count(total)
            )),
        gauge_area,
    );

    frame.render_widget(
        Paragraph::new(vec![
            stat_line(vec![
                stat("removed", format::count(done.saturating_sub(failed))),
                stat("freed", format::bytes(freed)),
                stat("failed", format::count(failed)),
            ]),
            Line::from(Span::styled(
                format!("mode: {}", app.delete_mode.label()),
                Style::default().fg(DIM),
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM)),
        ),
        stats_area,
    );

    let current = state.current_path();
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format::truncate_path(&current, current_area.width.saturating_sub(4) as usize),
            Style::default().fg(DIM),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(DIM))
                .title(Line::from(Span::styled(
                    " current ",
                    Style::default().fg(DIM),
                ))),
        ),
        current_area,
    );
}
