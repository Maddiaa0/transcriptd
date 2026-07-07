use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

use transcriptd::cli::CliTranscriber;
use transcriptd::config::{self, Config, EXAMPLE_CONFIG, EXAMPLE_GLOBAL_CONFIG};
use transcriptd::openrouter::{OpenRouterClient, Transcriber};
use transcriptd::state::{write_atomic, Failures, Ledger, StateDir};
use transcriptd::{scan, watch};

/// Folder transcription daemon: monitors a folder and transcribes new or
/// changed files into markdown via OpenRouter or a local agent CLI.
#[derive(Parser)]
#[command(name = "transcriptd", version, about)]
struct Cli {
    /// The watched folder (state lives in <folder>/.transcriptd)
    #[arg(long, default_value = ".", global = true)]
    folder: PathBuf,

    /// Config file path (default: <folder>/.transcriptd/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run one idempotent sweep over the folder (default)
    Scan {
        /// Keep running: rescan on filesystem changes and on a timer
        #[arg(long)]
        watch: bool,
    },
    /// Write a commented config template into <folder>/.transcriptd/
    Init {
        /// Write the global template (~/.config/transcriptd/config.toml)
        /// instead — the place for the API key and default model
        #[arg(long)]
        global: bool,
    },
    /// Summarize transcription state and visible failures
    Status,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("transcriptd: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let folder = cli
        .folder
        .canonicalize()
        .with_context(|| format!("folder not found: {}", cli.folder.display()))?;
    if !folder.is_dir() {
        bail!("{} is not a directory", folder.display());
    }
    let state = StateDir::new(&folder)?;

    // Layered config: global XDG file, then the folder's own config, then an
    // explicit --config path. Later layers override earlier ones key by key.
    let mut layers: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = config::xdg_config_path() {
        layers.push(xdg);
    }
    layers.push(state.config_path());
    if let Some(custom) = &cli.config {
        if !custom.exists() {
            bail!("config file not found: {}", custom.display());
        }
        layers.push(custom.clone());
    }
    let cfg = Config::load_layered(&layers)?;

    match cli.command.unwrap_or(Command::Scan { watch: false }) {
        Command::Init { global } => {
            let (path, template) = if global {
                let path = config::xdg_config_path().context(
                    "cannot determine config dir: neither XDG_CONFIG_HOME nor HOME is set",
                )?;
                (path, EXAMPLE_GLOBAL_CONFIG)
            } else {
                (state.config_path(), EXAMPLE_CONFIG)
            };
            if path.exists() {
                println!("config already exists: {}", path.display());
            } else {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                write_atomic(&path, template.as_bytes())?;
                if global {
                    // The global config may hold the API key.
                    restrict_permissions(&path);
                }
                println!("wrote {}", path.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Status => {
            let ledger = Ledger::load(&state)?;
            let failures = Failures::load(&state)?;
            println!("transcriptd status for {}", folder.display());
            println!(
                "  transcribed (unique content hashes): {}",
                ledger.entries.len()
            );
            println!("  skipped (unsupported/oversize): {}", ledger.skipped.len());
            println!("  failures pending retry: {}", failures.entries.len());
            for (path, f) in &failures.entries {
                println!(
                    "    {path} — {} attempt(s), last at {}: {}",
                    f.attempts,
                    f.last_attempt,
                    first_line(&f.last_error)
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Scan { watch: watch_mode } => {
            let client: Box<dyn Transcriber> = match cfg.backend.as_str() {
                "openrouter" => {
                    // The most common misconfiguration is expecting backend =
                    // "cli" but the file that sets it not being read (wrong
                    // HOME under systemd, stale path); name the active
                    // backend and every layer consulted so that's visible.
                    let api_key = cfg.resolve_api_key().with_context(|| {
                        let consulted: String = layers
                            .iter()
                            .map(|p| {
                                format!(
                                    "\n  - {} ({})",
                                    p.display(),
                                    if p.exists() { "loaded" } else { "not found" }
                                )
                            })
                            .collect();
                        format!(
                            "no API key: the active backend is \"openrouter\", which needs one. \
                             Set the {} environment variable or api_key in the global config; \
                             to transcribe via a local agent CLI without an API key, set \
                             backend = \"cli\". Config files consulted:{consulted}",
                            cfg.api_key_env
                        )
                    })?;
                    Box::new(OpenRouterClient::new(&cfg, api_key)?)
                }
                "cli" => Box::new(CliTranscriber::new(&cfg)?),
                other => bail!("unknown backend \"{other}\": expected \"openrouter\" or \"cli\""),
            };
            if watch_mode {
                watch::watch(&folder, &cfg, &state, client.as_ref())?;
                Ok(ExitCode::SUCCESS) // unreachable: watch loops forever
            } else {
                let outcome = scan::scan(&folder, &cfg, &state, client.as_ref())?;
                println!("scan complete: {}", outcome.summary());
                if outcome.failed > 0 {
                    Ok(ExitCode::FAILURE)
                } else {
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}
