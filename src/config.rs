use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

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
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn prompt(&self) -> &str {
        self.prompt.as_deref().unwrap_or(DEFAULT_PROMPT)
    }
}

pub const EXAMPLE_CONFIG: &str = r#"# transcriptd configuration
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
