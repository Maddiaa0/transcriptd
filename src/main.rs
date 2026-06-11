use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;

use transcriptd::config::{Config, EXAMPLE_CONFIG};
use transcriptd::openrouter::OpenRouterClient;
use transcriptd::state::{write_atomic, Failures, Ledger, StateDir};
use transcriptd::{scan, watch};

/// Folder transcription daemon: monitors a folder and transcribes new or
/// changed files into markdown via OpenRouter.
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
    Init,
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

    let config_path = cli.config.clone().unwrap_or_else(|| state.config_path());
    let cfg = if config_path.exists() {
        Config::load(&config_path)?
    } else if cli.config.is_some() {
        bail!("config file not found: {}", config_path.display());
    } else {
        Config::default()
    };

    match cli.command.unwrap_or(Command::Scan { watch: false }) {
        Command::Init => {
            let path = state.config_path();
            if path.exists() {
                println!("config already exists: {}", path.display());
            } else {
                write_atomic(&path, EXAMPLE_CONFIG.as_bytes())?;
                println!("wrote {}", path.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Status => {
            let ledger = Ledger::load(&state)?;
            let failures = Failures::load(&state)?;
            println!("transcriptd status for {}", folder.display());
            println!("  transcribed (unique content hashes): {}", ledger.entries.len());
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
            let api_key = std::env::var(&cfg.api_key_env).with_context(|| {
                format!(
                    "API key environment variable {} is not set",
                    cfg.api_key_env
                )
            })?;
            let client = OpenRouterClient::new(&cfg, api_key)?;
            if watch_mode {
                watch::watch(&folder, &cfg, &state, &client)?;
                Ok(ExitCode::SUCCESS) // unreachable: watch loops forever
            } else {
                let outcome = scan::scan(&folder, &cfg, &state, &client)?;
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
