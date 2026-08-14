use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

pub const STATE_DIR_NAME: &str = ".transcriptd";

/// All daemon state lives in `<folder>/.transcriptd` so a folder-scoped
/// grant covers the corpus and everything derived from it (R3).
#[derive(Clone)]
pub struct StateDir {
    pub root: PathBuf,
}

impl StateDir {
    pub fn new(folder: &Path) -> Result<Self> {
        let root = folder.join(STATE_DIR_NAME);
        fs::create_dir_all(root.join("cache"))
            .with_context(|| format!("creating {}", root.display()))?;
        fs::create_dir_all(root.join("raw"))?;
        Ok(StateDir { root })
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }
    pub fn ledger_path(&self) -> PathBuf {
        self.root.join("ledger.json")
    }
    pub fn failures_path(&self) -> PathBuf {
        self.root.join("failures.json")
    }
    pub fn log_path(&self) -> PathBuf {
        self.root.join("transcriptd.log")
    }
    /// Cached markdown transcript, keyed by content hash.
    pub fn cache_path(&self, sha: &str) -> PathBuf {
        self.root.join("cache").join(format!("{sha}.md"))
    }
    /// Raw API response, keyed by content hash (R7).
    pub fn raw_path(&self, sha: &str) -> PathBuf {
        self.root.join("raw").join(format!("{sha}.json"))
    }

    /// Hold an exclusive advisory lock for one complete scan. This prevents a
    /// timer, watcher, and manual invocation from loading the same ledger
    /// snapshot and performing duplicate paid transcription calls.
    pub fn acquire_scan_lock(&self) -> Result<ScanLock> {
        use fs2::FileExt as _;

        let path = self.root.join("scan.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening scan lock {}", path.display()))?;
        file.try_lock_exclusive().with_context(|| {
            format!(
                "another transcriptd scan is already running for {}",
                self.root.display()
            )
        })?;
        Ok(ScanLock { _file: file })
    }

    /// Log to stderr and the in-folder log file, subject to the process log
    /// level (see set_log_level). Level is a string ("INFO") at call sites
    /// to keep them terse; unknown strings are treated as INFO.
    pub fn log(&self, level: &str, msg: &str) {
        if Level::parse(level).unwrap_or(Level::Info) > log_level() {
            return;
        }
        let line = format!("{} {:<5} {}", now_rfc3339(), level, msg);
        eprintln!("{line}");
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// The operating system releases the lock when this guard is dropped, even
/// when a scan exits early with an error or the process terminates.
pub struct ScanLock {
    _file: fs::File,
}

/// Verbosity threshold: messages above the configured level are dropped.
/// Error < Warn < Info (default) < Debug (-v) < Trace (-vv).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            "trace" => Some(Level::Trace),
            _ => None,
        }
    }
}

/// Process-wide so components without a StateDir (the transcribers) can
/// consult it too. Set once at startup.
static LOG_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

pub fn set_log_level(level: Level) {
    LOG_LEVEL.store(level as u8, Ordering::Relaxed);
}

pub fn log_level() -> Level {
    match LOG_LEVEL.load(Ordering::Relaxed) {
        0 => Level::Error,
        1 => Level::Warn,
        3 => Level::Debug,
        4 => Level::Trace,
        _ => Level::Info,
    }
}

/// Stderr-only logging for components without a StateDir (the transcribers
/// run below the layer that owns the log file).
pub fn console_log(level: Level, msg: &str) {
    if level > log_level() {
        return;
    }
    eprintln!(
        "{} {:<5} {}",
        now_rfc3339(),
        format!("{level:?}").to_uppercase(),
        msg
    );
}

pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Write via a hidden sibling temp file + rename, so consumers never see a
/// partial file and the temp never shows up in a scan.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .context("path has no file name")?;
    let tmp = dir.join(format!(".{name}.tmp"));
    fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Hash ledger: a content hash recorded here is never re-sent to the API,
/// regardless of mtime or path changes (R2).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    /// sha256 -> entry
    #[serde(default)]
    pub entries: BTreeMap<String, LedgerEntry>,
    /// relative path -> reason; used to log unsupported/oversize skips once
    #[serde(default)]
    pub skipped: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Relative path where this content was last seen.
    pub path: String,
    pub model: String,
    pub prompt_version: String,
    /// Generation settings are part of cache identity. Defaults preserve
    /// compatibility with older ledgers and deliberately cause one refresh.
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub prompt_sha256: String,
    pub transcribed_at: String,
}

impl Ledger {
    pub fn load(state: &StateDir) -> Result<Ledger> {
        let path = state.ledger_path();
        if !path.exists() {
            return Ok(Ledger::default());
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, state: &StateDir) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        write_atomic(&state.ledger_path(), &json)
    }
}

/// Failed files: retried every sweep, never silently dropped (R10, R12).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Failures {
    /// relative path -> failure record
    #[serde(default)]
    pub entries: BTreeMap<String, FailureEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEntry {
    pub sha256: String,
    pub attempts: u32,
    pub last_error: String,
    pub last_attempt: String,
}

impl Failures {
    pub fn load(state: &StateDir) -> Result<Failures> {
        let path = state.failures_path();
        if !path.exists() {
            return Ok(Failures::default());
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, state: &StateDir) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        write_atomic(&state.failures_path(), &json)
    }

    pub fn record(&mut self, rel_path: &str, sha256: &str, error: &str) {
        let entry = self
            .entries
            .entry(rel_path.to_string())
            .or_insert_with(|| FailureEntry {
                sha256: sha256.to_string(),
                attempts: 0,
                last_error: String::new(),
                last_attempt: String::new(),
            });
        entry.sha256 = sha256.to_string();
        entry.attempts += 1;
        entry.last_error = error.to_string();
        entry.last_attempt = now_rfc3339();
    }

    pub fn clear(&mut self, rel_path: &str) {
        self.entries.remove(rel_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_order() {
        assert_eq!(Level::parse("info"), Some(Level::Info));
        assert_eq!(Level::parse("DEBUG"), Some(Level::Debug));
        assert_eq!(Level::parse("Warning"), Some(Level::Warn));
        assert_eq!(Level::parse("nope"), None);
        assert!(Level::Error < Level::Warn);
        assert!(Level::Info < Level::Debug);
        assert!(Level::Debug < Level::Trace);
    }

    #[test]
    fn scan_lock_is_exclusive_and_released_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::new(tmp.path()).unwrap();

        let first = state.acquire_scan_lock().unwrap();
        let err = state
            .acquire_scan_lock()
            .err()
            .expect("second lock must fail");
        assert!(format!("{err:#}").contains("another transcriptd scan"));

        drop(first);
        state.acquire_scan_lock().unwrap();
    }
}
