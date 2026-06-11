use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

pub const DEFAULT_MODEL: &str = "google/gemini-2.5-flash";
pub const DEFAULT_MARKER: &str = "<!-- transcriptd:document -->";
pub const DEFAULT_ROLLUP_NAME: &str = "transcript.md";
pub const DEFAULT_PROMPT: &str = "You are a transcription engine. Convert the supplied document into clean, faithful Markdown.\n\
- Transcribe ALL legible text, including handwriting.\n\
- Preserve the document's structure: headings, lists, tables, emphasis.\n\
- Use Markdown tables for tabular content; describe figures or diagrams briefly in italics.\n\
- Mark genuinely unreadable words as [illegible].\n\
- Output ONLY the Markdown transcript - no preamble, no commentary, no code fences.";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// OpenRouter model id used for transcription.
    pub model: String,
    /// OpenRouter API key. Belongs in the global config file
    /// (~/.config/transcriptd/config.toml), not the in-folder one, which
    /// syncs with the corpus. The environment variable wins when set.
    pub api_key: Option<String>,
    /// Environment variable holding the OpenRouter API key.
    pub api_key_env: String,
    /// A folder whose index.md contains this string is a document (gets a rollup).
    pub marker: String,
    /// Filename of the stitched rollup written at a marked folder's root.
    pub rollup_name: String,
    /// Files modified more recently than this are considered mid-sync and skipped.
    pub stability_seconds: u64,
    /// Watch mode: quiet period after a filesystem event before rescanning.
    pub watch_debounce_seconds: u64,
    /// Watch mode: rescan at least this often even with no events.
    pub watch_rescan_seconds: u64,
    /// OpenRouter PDF parser engine: native | pdf-text | mistral-ocr.
    pub pdf_engine: String,
    /// Custom transcription prompt; bump prompt_version when changing it.
    pub prompt: Option<String>,
    /// Recorded in sidecar frontmatter so transcripts are traceable to a prompt.
    pub prompt_version: String,
    pub request_timeout_seconds: u64,
    pub api_base: String,
    /// Files larger than this are skipped (logged once), not sent to the API.
    pub max_file_mb: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            model: DEFAULT_MODEL.to_string(),
            api_key: None,
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            marker: DEFAULT_MARKER.to_string(),
            rollup_name: DEFAULT_ROLLUP_NAME.to_string(),
            stability_seconds: 10,
            watch_debounce_seconds: 2,
            watch_rescan_seconds: 300,
            pdf_engine: "native".to_string(),
            prompt: None,
            prompt_version: "1".to_string(),
            request_timeout_seconds: 300,
            api_base: "https://openrouter.ai/api/v1".to_string(),
            max_file_mb: 32,
        }
    }
}

impl Config {
    /// Merge config layers, later layers overriding earlier ones key by key.
    /// Missing files are skipped; every present file is validated on its own
    /// so a typo error names the file it came from.
    pub fn load_layered(layers: &[PathBuf]) -> Result<Config> {
        let mut merged = toml::Table::new();
        for path in layers {
            if !path.exists() {
                continue;
            }
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading config {}", path.display()))?;
            let _: Config = toml::from_str(&text)
                .with_context(|| format!("parsing config {}", path.display()))?;
            let table: toml::Table = toml::from_str(&text)
                .with_context(|| format!("parsing config {}", path.display()))?;
            for (k, v) in table {
                merged.insert(k, v);
            }
        }
        toml::Value::Table(merged)
            .try_into()
            .context("merging config layers")
    }

    pub fn prompt(&self) -> &str {
        self.prompt.as_deref().unwrap_or(DEFAULT_PROMPT)
    }

    /// Env var first (systemd EnvironmentFile, shell exports), then the
    /// api_key from the merged config.
    pub fn resolve_api_key(&self) -> Option<String> {
        std::env::var(&self.api_key_env)
            .ok()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| self.api_key.clone())
    }
}

/// Per-user application directory under XDG_CONFIG_HOME
/// (~/.config/transcriptd). Holds the global config and is the home for any
/// future machine-local application data (e.g. an index database).
pub fn xdg_app_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("transcriptd"))
}

pub fn xdg_config_path() -> Option<PathBuf> {
    Some(xdg_app_dir()?.join("config.toml"))
}

pub const EXAMPLE_GLOBAL_CONFIG: &str = r#"# transcriptd global configuration (~/.config/transcriptd/config.toml)
# Settings here apply to every watched folder; a folder's own
# .transcriptd/config.toml overrides them key by key.

# Any vision-capable model on OpenRouter; swapping requires no code change.
model = "google/gemini-2.5-flash"

# OpenRouter API key. Keep it here (or in the environment) rather than in a
# watched folder's config, which syncs with the corpus. The environment
# variable below takes precedence when set.
# api_key = "sk-or-..."
api_key_env = "OPENROUTER_API_KEY"
"#;

pub const EXAMPLE_CONFIG: &str = r#"# transcriptd per-folder configuration
# Overrides the global config (~/.config/transcriptd/config.toml) key by key.
# The OpenRouter API key belongs in the global config or the environment,
# NOT here - this file lives inside the synced corpus.

# Any vision-capable model on OpenRouter; swapping requires no code change.
model = "google/gemini-2.5-flash"

# Environment variable holding the OpenRouter API key.
api_key_env = "OPENROUTER_API_KEY"

# A folder whose index.md contains this string is a document:
# transcriptd maintains a stitched rollup at its root.
marker = "<!-- transcriptd:document -->"
rollup_name = "transcript.md"

# Files modified more recently than this many seconds are treated as
# mid-sync and picked up on a later sweep.
stability_seconds = 10

# Watch mode tuning.
watch_debounce_seconds = 2
watch_rescan_seconds = 300

# How OpenRouter parses PDFs: "native" (model sees pages as images),
# "pdf-text" (free, needs a text layer), or "mistral-ocr" (paid OCR).
pdf_engine = "native"

# Bump this when you change the prompt so sidecars record which prompt
# produced them.
prompt_version = "1"
# prompt = "Custom transcription prompt..."

request_timeout_seconds = 300
api_base = "https://openrouter.ai/api/v1"
max_file_mb = 32
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_layers_override_earlier_ones_key_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let folder = dir.path().join("folder.toml");
        std::fs::write(&global, "model = \"global/model\"\napi_key = \"sk-or-global\"\n").unwrap();
        std::fs::write(&folder, "model = \"folder/model\"\nstability_seconds = 60\n").unwrap();

        let cfg = Config::load_layered(&[global, folder]).unwrap();
        assert_eq!(cfg.model, "folder/model");
        assert_eq!(cfg.api_key.as_deref(), Some("sk-or-global"));
        assert_eq!(cfg.stability_seconds, 60);
        assert_eq!(cfg.prompt_version, "1"); // untouched keys keep defaults
    }

    #[test]
    fn missing_layers_are_skipped_and_defaults_apply() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load_layered(&[dir.path().join("nope.toml")]).unwrap();
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert!(cfg.api_key.is_none());
    }

    #[test]
    fn typo_error_names_the_offending_file() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "modle = \"oops\"\n").unwrap();
        let err = Config::load_layered(std::slice::from_ref(&bad)).unwrap_err();
        assert!(format!("{err:#}").contains("bad.toml"));
    }
}
