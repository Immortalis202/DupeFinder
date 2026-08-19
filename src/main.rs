//! dupefind — a terminal UI for finding and removing duplicate files.

mod app;
mod cli;
mod delete;
mod format;
mod model;
mod scan;
mod ui;

use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use crate::app::App;
use crate::cli::Args;

/// Poll interval while a background worker is running: fast enough that the
/// live counters look continuous.
const POLL_BUSY: Duration = Duration::from_millis(50);

/// Poll interval when idle. Long enough that a parked TUI uses no measurable CPU.
const POLL_IDLE: Duration = Duration::from_millis(250);

fn main() -> Result<()> {
    let args = Args::parse();

    // Resolve the starting directory before touching the terminal, so a bad path
    // produces an ordinary error message rather than a corrupted screen.
    let start_dir = match &args.directory {
        Some(dir) => dir
            .canonicalize()
            .with_context(|| format!("cannot open {}", dir.display()))?,
        None => std::env::current_dir().context("cannot determine the current directory")?,
    };
    if !start_dir.is_dir() {
        anyhow::bail!("{} is not a directory", start_dir.display());
    }

    let mut app = App::new(start_dir.clone(), args.scan_options(), args.delete_mode());

    // An explicit path means the user already chose; skip the browser.
    if args.directory.is_some() {
        app.start_scan(start_dir);
    }

    // `run` restores the terminal even if the closure returns an error or panics.
    ratatui::run(|terminal| -> Result<()> {
        while !app.should_quit {
            terminal.draw(|frame| ui::draw(frame, &mut app))?;

            // Drain worker messages without blocking the draw loop.
            let scan_msgs: Vec<_> = app
                .scan_rx
                .as_ref()
                .map(|rx| rx.try_iter().collect())
                .unwrap_or_default();
            for msg in scan_msgs {
                app.handle_scan_msg(msg);
            }

            let delete_msgs: Vec<_> = app
                .delete_rx
                .as_ref()
                .map(|rx| rx.try_iter().collect())
                .unwrap_or_default();
            for msg in delete_msgs {
                app.handle_delete_msg(msg);
            }

            let timeout = if app.is_busy() { POLL_BUSY } else { POLL_IDLE };
            if event::poll(timeout)? {
                match event::read()? {
                    // Windows reports both Press and Release; without this filter
                    // every keystroke would be handled twice.
                    Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                    _ => {}
                }
            }
        }
        Ok(())
    })?;

    // Anything worth reading after the alternate screen is gone.
    if let Some(report) = &app.report {
        println!(
            "Deleted {} file(s), reclaimed {} ({}).",
            report.deleted,
            format::bytes(report.bytes_freed),
            report.mode_label.to_lowercase()
        );
        for failure in &report.failures {
            eprintln!("failed: {}: {}", failure.path.display(), failure.message);
        }
    }

    Ok(())
}
