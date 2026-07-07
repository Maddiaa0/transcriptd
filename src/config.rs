use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_MODEL: &str = "google/gemini-2.5-flash";
pub const DEFAULT_MARKER: &str = "<!-- transcriptd:document -->";
pub const DEFAULT_ROLLUP_NAME: &str = "transcript.md";
pub const DEFAULT_PROMPT: &str = "You are a transcription engine. Convert the supplied document into clean, faithful Markdown.\n\
- Transcribe ALL legible text, including handwriting.\n\
- Preserve the document's structure: headings, lists, tables, emphasis.\n\
- Use Markdown tables for tabular content; describe figures or diagrams briefly in italics.\n\
- Mark genuinely unreadable words as [illegible].\n\
- Output ONLY the Markdown transcript - no preamble, no commentary, no code fences.\n
- Once complete - add a summary of important information and any key insights or findings to the top of the file.";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Transcription backend: "openrouter" (HTTP API, the default) or "cli"
    /// (shell out to a local agent CLI such as OpenAI Codex — see [cli]).
    pub backend: String,
    /// OpenRouter model id used for transcription. With backend = "cli" this
    /// is only a provenance label recorded in sidecar frontmatter.
    pub model: String,
    /// OpenRouter API key. Belongs in the global config file
    /// (~/.config/transcriptd/config.toml), not the in-folder one, which
    /// syncs with the corpus. The environment variable wins when set.
    pub api_key: Option<String>,
    /// Environment variable holding the OpenRouter API key.
    pub api_key_env: String,
    /// Where generated markdown (sidecars and rollups) is written, mirroring
    /// the watched folder's structure. Unset: sidecars land next to their
    /// source file and rollups at each marked folder's root. Relative paths
    /// resolve against the watched folder.
    pub output_dir: Option<String>,
    /// When set, EVERY folder containing transcribable files gets a rollup —
    /// no index.md marker needed — stitched from its direct files and written
    /// at the folder's mirrored path under this directory (and only there;
    /// nothing is written at the folder's own root). Replaces the
    /// marker-based rollup mechanism. Relative paths resolve against the
    /// watched folder.
    pub rollup_dir: Option<String>,
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
    /// HTTP request timeout; with backend = "cli", how long a command may run.
    pub request_timeout_seconds: u64,
    pub api_base: String,
    /// Files larger than this are skipped (logged once), not sent to the API.
    pub max_file_mb: u64,
    /// Settings for backend = "cli". Note: layering replaces this table
    /// wholesale — a folder config with a [cli] section overrides the global
    /// one entirely, not key by key.
    pub cli: CliConfig,
}

/// How to invoke a local agent CLI (OpenAI Codex, hermes, ...) as the
/// transcription backend. Each command is an argv template; the placeholders
/// {file}, {prompt} and {output} are substituted inside each argument:
///   {file}   — path to a temp copy of the document (image/pdf bytes, or
///              extracted text as .txt for docx/text files)
///   {prompt} — the transcription prompt
///   {output} — temp path the command should write the transcript to; when a
///              template has no {output}, stdout is the transcript instead
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CliConfig {
    /// Fallback argv template used for any file kind without an override.
    pub command: Vec<String>,
    /// Override for images (png/jpg/webp), e.g. Codex's `-i` attachment flag.
    pub image_command: Option<Vec<String>>,
    /// Override for PDFs.
    pub pdf_command: Option<Vec<String>>,
    /// Override for text payloads (txt/csv/html and extracted docx).
    pub text_command: Option<Vec<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            backend: "openrouter".to_string(),
            model: DEFAULT_MODEL.to_string(),
            api_key: None,
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            output_dir: None,
            rollup_dir: None,
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
            cli: CliConfig::default(),
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

    /// Root for generated markdown when `output_dir` redirects it; None means
    /// the default in-place layout. Relative paths resolve against the
    /// watched folder so a synced per-folder config works on every machine.
    pub fn output_root(&self, folder: &Path) -> Option<PathBuf> {
        resolve_against(folder, self.output_dir.as_deref())
    }

    /// Root of the per-folder rollup tree when `rollup_dir` enables it; None
    /// means the marker-based mechanism is active instead.
    pub fn rollup_root(&self, folder: &Path) -> Option<PathBuf> {
        resolve_against(folder, self.rollup_dir.as_deref())
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

/// Resolve a configured directory: absolute paths as-is, relative ones
/// against the watched folder, blank/unset as None.
fn resolve_against(folder: &Path, dir: Option<&str>) -> Option<PathBuf> {
    let dir = dir?.trim();
    if dir.is_empty() {
        return None;
    }
    let path = Path::new(dir);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        folder.join(path)
    })
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

# Transcription backend: "openrouter" (HTTP API, default) or "cli" (shell out
# to a local agent CLI — use an existing subscription instead of API credit).
# backend = "openrouter"

# Any vision-capable model on OpenRouter; swapping requires no code change.
# With backend = "cli" this is only a provenance label recorded in sidecars
# (set it to e.g. "codex" or "codex/gpt-5.1").
model = "google/gemini-2.5-flash"

# OpenRouter API key (not needed with backend = "cli"). Keep it here (or in
# the environment) rather than in a watched folder's config, which syncs with
# the corpus. The environment variable below takes precedence when set.
# api_key = "sk-or-..."
api_key_env = "OPENROUTER_API_KEY"

# Command templates for backend = "cli". Placeholders substituted inside each
# argument:
#   {file}   — temp copy of the document (image/pdf; docx/text arrive as .txt)
#   {prompt} — the transcription prompt
#   {output} — temp file the command should write the transcript to; omit
#              {output} from the template to read the transcript from stdout
#
# Example: OpenAI Codex CLI (rides your ChatGPT subscription — run
# `codex login` once first). Codex attaches images with -i; other kinds get
# the file path appended to the prompt so the agent reads it itself.
# [cli]
# command = ["codex", "exec", "--skip-git-repo-check", "--output-last-message", "{output}", "{prompt}\n\nThe document to transcribe is the file at: {file}"]
# image_command = ["codex", "exec", "--skip-git-repo-check", "--output-last-message", "{output}", "-i", "{file}", "{prompt}"]
#
# Any other agent CLI (hermes, claude, ...) works the same way — one argv
# template that receives the file and prompt and emits markdown.
"#;

pub const EXAMPLE_CONFIG: &str = r#"# transcriptd per-folder configuration
# Overrides the global config (~/.config/transcriptd/config.toml) key by key.
# The OpenRouter API key belongs in the global config or the environment,
# NOT here - this file lives inside the synced corpus.

# Transcription backend: "openrouter" (default) or "cli". The [cli] command
# templates belong in the global config — they are machine-specific.
# backend = "openrouter"

# Any vision-capable model on OpenRouter; swapping requires no code change.
model = "google/gemini-2.5-flash"

# Environment variable holding the OpenRouter API key.
api_key_env = "OPENROUTER_API_KEY"

# Where generated markdown (sidecars and rollups) goes, mirroring the
# folder's structure. Default: next to each source file. Relative paths
# resolve against the watched folder.
# output_dir = "transcripts"

# When set, EVERY folder gets a rollup of its direct files (no index.md
# marker needed), written at the folder's mirrored path under this directory
# and nowhere else. Replaces the marker mechanism below. Relative paths
# resolve against the watched folder.
# rollup_dir = "rollups"

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

# HTTP request timeout; with backend = "cli", how long a command may run.
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
        std::fs::write(
            &global,
            "model = \"global/model\"\napi_key = \"sk-or-global\"\n",
        )
        .unwrap();
        std::fs::write(
            &folder,
            "model = \"folder/model\"\nstability_seconds = 60\n",
        )
        .unwrap();

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
    fn output_root_resolves_relative_absolute_and_empty() {
        let folder = Path::new("/srv/notes");
        let mut cfg = Config::default();
        assert!(cfg.output_root(folder).is_none());

        cfg.output_dir = Some("transcripts".to_string());
        assert_eq!(
            cfg.output_root(folder).unwrap(),
            PathBuf::from("/srv/notes/transcripts")
        );

        cfg.output_dir = Some("/var/transcripts".to_string());
        assert_eq!(
            cfg.output_root(folder).unwrap(),
            PathBuf::from("/var/transcripts")
        );

        cfg.output_dir = Some("  ".to_string());
        assert!(cfg.output_root(folder).is_none());

        // rollup_root shares the same resolution rules.
        assert!(cfg.rollup_root(folder).is_none());
        cfg.rollup_dir = Some("rollups".to_string());
        assert_eq!(
            cfg.rollup_root(folder).unwrap(),
            PathBuf::from("/srv/notes/rollups")
        );
    }

    #[test]
    fn cli_backend_settings_parse_and_layer() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let folder = dir.path().join("folder.toml");
        std::fs::write(
            &global,
            r#"
backend = "cli"
model = "codex"
[cli]
command = ["codex", "exec", "{prompt}", "{file}"]
image_command = ["codex", "exec", "-i", "{file}", "{prompt}"]
"#,
        )
        .unwrap();
        // A folder layer without a [cli] table keeps the global one.
        std::fs::write(&folder, "stability_seconds = 5\n").unwrap();

        let cfg = Config::load_layered(&[global, folder]).unwrap();
        assert_eq!(cfg.backend, "cli");
        assert_eq!(cfg.cli.command[0], "codex");
        assert_eq!(cfg.cli.image_command.as_ref().unwrap()[2], "-i".to_string());
        assert!(cfg.cli.pdf_command.is_none());
        assert_eq!(cfg.stability_seconds, 5);
    }

    #[test]
    fn typo_error_names_the_offending_file() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "modle = \"oops\"\n").unwrap();
        let err = Config::load_layered(std::slice::from_ref(&bad)).unwrap_err();
        assert!(format!("{err:#}").contains("bad.toml"));
    }

    #[test]
    fn shipped_init_templates_parse_and_match_defaults() {
        // `transcriptd init` writes these verbatim; a template that no longer
        // deserializes would brick every fresh install.
        let folder: Config = toml::from_str(EXAMPLE_CONFIG).unwrap();
        let global: Config = toml::from_str(EXAMPLE_GLOBAL_CONFIG).unwrap();

        // Their uncommented values are documentation of the defaults — keep
        // them in lockstep with Config::default().
        let defaults = Config::default();
        assert_eq!(folder.model, defaults.model);
        assert_eq!(folder.api_key_env, defaults.api_key_env);
        assert_eq!(folder.marker, defaults.marker);
        assert_eq!(folder.rollup_name, defaults.rollup_name);
        assert_eq!(folder.stability_seconds, defaults.stability_seconds);
        assert_eq!(
            folder.watch_debounce_seconds,
            defaults.watch_debounce_seconds
        );
        assert_eq!(folder.watch_rescan_seconds, defaults.watch_rescan_seconds);
        assert_eq!(folder.pdf_engine, defaults.pdf_engine);
        assert_eq!(folder.prompt_version, defaults.prompt_version);
        assert_eq!(
            folder.request_timeout_seconds,
            defaults.request_timeout_seconds
        );
        assert_eq!(folder.api_base, defaults.api_base);
        assert_eq!(folder.max_file_mb, defaults.max_file_mb);
        assert_eq!(global.model, defaults.model);
        assert_eq!(global.api_key_env, defaults.api_key_env);
    }

    #[test]
    fn defaults_are_stable() {
        // Every default is observable behavior (recorded in sidecars, used as
        // API parameters); changing one should be a deliberate test edit.
        let cfg = Config::default();
        assert_eq!(cfg.backend, "openrouter");
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert!(cfg.api_key.is_none());
        assert_eq!(cfg.api_key_env, "OPENROUTER_API_KEY");
        assert!(cfg.output_dir.is_none());
        assert!(cfg.rollup_dir.is_none());
        assert_eq!(cfg.marker, DEFAULT_MARKER);
        assert_eq!(cfg.rollup_name, DEFAULT_ROLLUP_NAME);
        assert_eq!(cfg.stability_seconds, 10);
        assert_eq!(cfg.watch_debounce_seconds, 2);
        assert_eq!(cfg.watch_rescan_seconds, 300);
        assert_eq!(cfg.pdf_engine, "native");
        assert!(cfg.prompt.is_none());
        assert_eq!(cfg.prompt(), DEFAULT_PROMPT);
        assert_eq!(cfg.prompt_version, "1");
        assert_eq!(cfg.request_timeout_seconds, 300);
        assert_eq!(cfg.api_base, "https://openrouter.ai/api/v1");
        assert_eq!(cfg.max_file_mb, 32);
        assert!(cfg.cli.command.is_empty());
        assert!(cfg.cli.image_command.is_none());
        assert!(cfg.cli.pdf_command.is_none());
        assert!(cfg.cli.text_command.is_none());
    }

    #[test]
    fn every_field_deserializes_from_a_fully_populated_config() {
        let cfg: Config = toml::from_str(
            r#"
backend = "cli"
model = "m"
api_key = "k"
api_key_env = "E"
output_dir = "out"
rollup_dir = "roll"
marker = "MARK"
rollup_name = "roll.md"
stability_seconds = 1
watch_debounce_seconds = 2
watch_rescan_seconds = 3
pdf_engine = "pdf-text"
prompt = "P"
prompt_version = "9"
request_timeout_seconds = 4
api_base = "https://example.test/v1"
max_file_mb = 5

[cli]
command = ["a", "{file}"]
image_command = ["b", "{file}"]
pdf_command = ["c", "{file}"]
text_command = ["d", "{file}"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.backend, "cli");
        assert_eq!(cfg.model, "m");
        assert_eq!(cfg.api_key.as_deref(), Some("k"));
        assert_eq!(cfg.api_key_env, "E");
        assert_eq!(cfg.output_dir.as_deref(), Some("out"));
        assert_eq!(cfg.rollup_dir.as_deref(), Some("roll"));
        assert_eq!(cfg.marker, "MARK");
        assert_eq!(cfg.rollup_name, "roll.md");
        assert_eq!(cfg.stability_seconds, 1);
        assert_eq!(cfg.watch_debounce_seconds, 2);
        assert_eq!(cfg.watch_rescan_seconds, 3);
        assert_eq!(cfg.pdf_engine, "pdf-text");
        assert_eq!(cfg.prompt(), "P");
        assert_eq!(cfg.prompt_version, "9");
        assert_eq!(cfg.request_timeout_seconds, 4);
        assert_eq!(cfg.api_base, "https://example.test/v1");
        assert_eq!(cfg.max_file_mb, 5);
        assert_eq!(cfg.cli.command, vec!["a", "{file}"]);
        assert_eq!(cfg.cli.image_command.unwrap(), vec!["b", "{file}"]);
        assert_eq!(cfg.cli.pdf_command.unwrap(), vec!["c", "{file}"]);
        assert_eq!(cfg.cli.text_command.unwrap(), vec!["d", "{file}"]);
    }

    #[test]
    fn unknown_keys_are_rejected_everywhere() {
        // deny_unknown_fields is the typo guard; it must hold in the nested
        // [cli] table too, or a misspelled override silently falls back.
        assert!(toml::from_str::<Config>("nope = 1\n").is_err());
        assert!(toml::from_str::<Config>("[cli]\nimage_cmd = [\"x\"]\n").is_err());
    }

    #[test]
    fn wrong_value_type_error_names_the_offending_file() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad-type.toml");
        std::fs::write(&bad, "stability_seconds = \"ten\"\n").unwrap();
        let err = Config::load_layered(std::slice::from_ref(&bad)).unwrap_err();
        assert!(format!("{err:#}").contains("bad-type.toml"));
    }

    #[test]
    fn explicit_layer_beats_folder_beats_global() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let folder = dir.path().join("folder.toml");
        let explicit = dir.path().join("explicit.toml");
        std::fs::write(&global, "model = \"g\"\nmarker = \"G\"\nmax_file_mb = 1\n").unwrap();
        std::fs::write(&folder, "model = \"f\"\nmarker = \"F\"\n").unwrap();
        std::fs::write(&explicit, "model = \"e\"\n").unwrap();

        let cfg = Config::load_layered(&[global, folder, explicit]).unwrap();
        assert_eq!(cfg.model, "e");
        assert_eq!(cfg.marker, "F");
        assert_eq!(cfg.max_file_mb, 1);
    }

    #[test]
    fn cli_table_is_replaced_wholesale_by_later_layers() {
        // Documented behavior: layering merges top-level keys, so a folder's
        // [cli] table overrides the global one entirely, not key by key.
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.toml");
        let folder = dir.path().join("folder.toml");
        std::fs::write(
            &global,
            "[cli]\ncommand = [\"g\", \"{file}\"]\nimage_command = [\"gi\", \"{file}\"]\n",
        )
        .unwrap();
        std::fs::write(&folder, "[cli]\ncommand = [\"f\", \"{file}\"]\n").unwrap();

        let cfg = Config::load_layered(&[global, folder]).unwrap();
        assert_eq!(cfg.cli.command[0], "f");
        assert!(
            cfg.cli.image_command.is_none(),
            "the folder [cli] table must replace the global one wholesale"
        );
    }

    #[test]
    fn api_key_resolution_prefers_nonempty_env_var() {
        // Unique per-test env var names: tests run in parallel threads and
        // the environment is process-global.
        let var = "TRANSCRIPTD_TEST_KEY_RESOLUTION";
        std::env::remove_var(var);
        let cfg = Config {
            api_key: Some("from-config".to_string()),
            api_key_env: var.to_string(),
            ..Config::default()
        };
        assert_eq!(cfg.resolve_api_key().as_deref(), Some("from-config"));

        std::env::set_var(var, "from-env");
        assert_eq!(cfg.resolve_api_key().as_deref(), Some("from-env"));

        // A blank env var (e.g. `EnvironmentFile` with an empty value) must
        // not shadow a configured key.
        std::env::set_var(var, "   ");
        assert_eq!(cfg.resolve_api_key().as_deref(), Some("from-config"));
        std::env::remove_var(var);
    }

    #[test]
    fn api_key_resolution_is_none_when_nothing_is_set() {
        let cfg = Config {
            api_key_env: "TRANSCRIPTD_TEST_KEY_UNSET".to_string(),
            ..Config::default()
        };
        std::env::remove_var(&cfg.api_key_env);
        assert!(cfg.resolve_api_key().is_none());
    }
}
