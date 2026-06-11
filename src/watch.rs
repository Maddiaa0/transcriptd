use anyhow::{Context, Result};
use notify::{Event, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::openrouter::Transcriber;
use crate::scan;
use crate::state::StateDir;

/// Watch mode is a thin wrapper around the same idempotent scan: rescan when
/// the filesystem changes (debounced) and at least every rescan interval.
pub fn watch(
    folder: &Path,
    cfg: &Config,
    state: &StateDir,
    transcriber: &dyn Transcriber,
) -> Result<()> {
    run_scan(folder, cfg, state, transcriber);

    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("creating filesystem watcher")?;
    watcher
        .watch(folder, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", folder.display()))?;
    state.log("INFO", &format!("watching {}", folder.display()));

    let rescan_interval = Duration::from_secs(cfg.watch_rescan_seconds.max(1));
    let debounce = Duration::from_secs(cfg.watch_debounce_seconds);
    loop {
        if wait_for_relevant_event(&rx, rescan_interval, &state.root) {
            // Let a burst of sync writes settle before scanning.
            loop {
                match rx.recv_timeout(debounce) {
                    Ok(_) => continue,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        }
        run_scan(folder, cfg, state, transcriber);
    }
}

fn run_scan(folder: &Path, cfg: &Config, state: &StateDir, transcriber: &dyn Transcriber) {
    match scan::scan(folder, cfg, state, transcriber) {
        Ok(outcome) => state.log("INFO", &format!("scan complete: {}", outcome.summary())),
        Err(e) => state.log("ERROR", &format!("scan failed: {e:#}")),
    }
}

/// Returns true if a relevant event arrived, false on timeout (periodic
/// rescan) or watcher loss.
fn wait_for_relevant_event(
    rx: &Receiver<notify::Result<Event>>,
    timeout: Duration,
    state_root: &Path,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(Ok(event)) if event_is_relevant(&event, state_root) => return true,
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => return false,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Ignore our own writes (state dir, sidecars/rollups) so a scan's output
/// doesn't immediately trigger another scan. index.md is allowed through
/// because adding a marker should create a rollup promptly.
fn event_is_relevant(event: &Event, state_root: &Path) -> bool {
    event.paths.iter().any(|p| {
        if p.starts_with(state_root) {
            return false;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') {
            return false;
        }
        if name == "index.md" {
            return true;
        }
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default();
        ext != "md" && ext != "markdown"
    })
}
