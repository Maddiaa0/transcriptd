use anyhow::{bail, Context, Result};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::config::{CliConfig, Config};
use crate::openrouter::{
    strip_fences, truncate, Payload, TranscribeInput, TranscribeOutput, Transcriber,
};

/// Transcribes by shelling out to a local agent CLI (OpenAI Codex, hermes,
/// ...) instead of the OpenRouter API, so an existing subscription can pay
/// for the work. The argv templates come from the [cli] config table;
/// {file}, {prompt} and {output} are substituted inside each argument.
#[derive(Debug)]
pub struct CliTranscriber {
    cli: CliConfig,
    prompt: String,
    timeout: Duration,
}

impl CliTranscriber {
    pub fn new(cfg: &Config) -> Result<Self> {
        let cli = cfg.cli.clone();
        let overrides = [
            ("image_command", cli.image_command.as_ref()),
            ("pdf_command", cli.pdf_command.as_ref()),
            ("text_command", cli.text_command.as_ref()),
        ];
        if cli.command.is_empty() && overrides.iter().all(|(_, t)| t.is_none()) {
            bail!(
                "backend = \"cli\" but no [cli] command is configured; \
                 see the template from `transcriptd init --global`"
            );
        }
        for (name, template) in overrides
            .into_iter()
            .chain([("command", Some(&cli.command))])
        {
            let Some(template) = template else { continue };
            if template.is_empty() {
                continue; // an absent kind falls back to `command`
            }
            if !template.iter().any(|arg| arg.contains("{file}")) {
                bail!("[cli] {name} has no {{file}} placeholder - the command would never see the document");
            }
        }
        Ok(CliTranscriber {
            cli,
            prompt: cfg.prompt().to_string(),
            timeout: Duration::from_secs(cfg.request_timeout_seconds),
        })
    }
}

impl Transcriber for CliTranscriber {
    fn transcribe(&self, input: &TranscribeInput) -> Result<TranscribeOutput> {
        let (kind, template, doc) = match &input.payload {
            Payload::Image { mime, data } => (
                "image",
                self.cli.image_command.as_ref().unwrap_or(&self.cli.command),
                TempFile::create(image_ext(mime), data)?,
            ),
            Payload::Pdf { data } => (
                "pdf",
                self.cli.pdf_command.as_ref().unwrap_or(&self.cli.command),
                TempFile::create("pdf", data)?,
            ),
            Payload::Text { body } => (
                "text",
                self.cli.text_command.as_ref().unwrap_or(&self.cli.command),
                TempFile::create("txt", body.as_bytes())?,
            ),
        };
        if template.is_empty() {
            bail!("no [cli] command configured for {kind} files");
        }

        let wants_output = template.iter().any(|arg| arg.contains("{output}"));
        let output = TempFile::reserve("out.md");
        let argv: Vec<String> = template
            .iter()
            .map(|arg| {
                arg.replace("{file}", &doc.0.to_string_lossy())
                    .replace("{output}", &output.0.to_string_lossy())
                    .replace("{prompt}", &self.prompt)
            })
            .collect();

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        let (status, stdout, stderr) = run_with_timeout(cmd, self.timeout)
            .with_context(|| format!("running {} for {}", argv[0], input.filename))?;
        if !status.success() {
            bail!(
                "{} exited with {status} for {}: {}",
                argv[0],
                input.filename,
                truncate(stderr.trim(), 500)
            );
        }

        let text = if wants_output {
            fs::read_to_string(&output.0).with_context(|| {
                format!("{} succeeded but wrote nothing to the {{output}} file", argv[0])
            })?
        } else {
            stdout
        };
        let markdown = strip_fences(&text);
        if markdown.is_empty() {
            // Never record an empty transcript as done (R10).
            bail!("empty transcription from {}", argv[0]);
        }
        // Shaped like a chat-completions response so cache recovery from the
        // retained raw (openrouter::extract_markdown) works for this backend.
        let raw = json!({
            "choices": [{ "message": { "content": markdown } }],
            "transcriptd_cli": {
                "command": argv[0],
                "stderr": truncate(stderr.trim(), 2000),
            }
        });
        Ok(TranscribeOutput { markdown, raw })
    }
}

fn image_ext(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "bin",
    }
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Temp file removed on drop, so failed runs don't accumulate litter.
struct TempFile(PathBuf);

impl TempFile {
    fn path(ext: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "transcriptd-{}-{}.{ext}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn create(ext: &str, contents: &[u8]) -> Result<TempFile> {
        let path = Self::path(ext);
        fs::write(&path, contents)
            .with_context(|| format!("writing temp file {}", path.display()))?;
        Ok(TempFile(path))
    }

    /// A unique path the child process may write to; nothing is created.
    fn reserve(ext: &str) -> TempFile {
        TempFile(Self::path(ext))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Run to completion with a deadline. Stdout/stderr are drained on threads so
/// a chatty CLI can't fill the pipe buffer and deadlock against try_wait.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> Result<(ExitStatus, String, String)> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .context("spawning command (is it installed and on PATH?)")?;

    fn drain(mut pipe: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    }
    let out_thread = drain(child.stdout.take().expect("stdout piped"));
    let err_thread = drain(child.stderr.take().expect("stderr piped"));

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for command")? {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok((status, stdout, stderr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn cfg_with(cli: CliConfig) -> Config {
        Config {
            backend: "cli".to_string(),
            cli,
            ..Config::default()
        }
    }

    fn text_input(body: &str) -> TranscribeInput {
        TranscribeInput {
            filename: "note.txt".to_string(),
            payload: Payload::Text {
                body: body.to_string(),
            },
        }
    }

    #[test]
    fn missing_command_is_a_config_error() {
        let err = CliTranscriber::new(&cfg_with(CliConfig::default())).unwrap_err();
        assert!(format!("{err:#}").contains("no [cli] command"));
    }

    #[test]
    fn template_without_file_placeholder_is_rejected() {
        let cli = CliConfig {
            command: vec!["echo".into(), "{prompt}".into()],
            ..CliConfig::default()
        };
        let err = CliTranscriber::new(&cfg_with(cli)).unwrap_err();
        assert!(format!("{err:#}").contains("{file}"));
    }

    #[cfg(unix)]
    #[test]
    fn stdout_becomes_the_transcript() {
        let cli = CliConfig {
            command: vec!["cat".into(), "{file}".into()],
            ..CliConfig::default()
        };
        let t = CliTranscriber::new(&cfg_with(cli)).unwrap();
        let out = t.transcribe(&text_input("# Hello\n")).unwrap();
        assert_eq!(out.markdown, "# Hello");
        // Raw is chat-completions shaped so cache recovery keeps working.
        assert_eq!(
            crate::openrouter::extract_markdown(&out.raw).unwrap(),
            "# Hello"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_placeholder_reads_the_written_file() {
        let cli = CliConfig {
            command: vec![
                "sh".into(),
                "-c".into(),
                "cat {file} > /dev/null && printf '# From file' > {output}".into(),
            ],
            ..CliConfig::default()
        };
        let t = CliTranscriber::new(&cfg_with(cli)).unwrap();
        let out = t.transcribe(&text_input("ignored")).unwrap();
        assert_eq!(out.markdown, "# From file");
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_is_an_error_with_stderr() {
        let cli = CliConfig {
            command: vec![
                "sh".into(),
                "-c".into(),
                "cat {file} > /dev/null; echo boom >&2; exit 3".into(),
            ],
            ..CliConfig::default()
        };
        let t = CliTranscriber::new(&cfg_with(cli)).unwrap();
        let err = t.transcribe(&text_input("x")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("boom"), "missing stderr in: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn empty_output_is_an_error() {
        let cli = CliConfig {
            command: vec!["sh".into(), "-c".into(), "cat {file} > /dev/null".into()],
            ..CliConfig::default()
        };
        let t = CliTranscriber::new(&cfg_with(cli)).unwrap();
        assert!(t.transcribe(&text_input("x")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn image_override_wins_over_fallback_command() {
        let cli = CliConfig {
            command: vec!["false".into(), "{file}".into()],
            image_command: Some(vec![
                "sh".into(),
                "-c".into(),
                "printf 'image via override # %s' {file}".into(),
            ]),
            ..CliConfig::default()
        };
        let t = CliTranscriber::new(&cfg_with(cli)).unwrap();
        let out = t
            .transcribe(&TranscribeInput {
                filename: "page.png".to_string(),
                payload: Payload::Image {
                    mime: "image/png",
                    data: vec![1, 2, 3],
                },
            })
            .unwrap();
        assert!(out.markdown.starts_with("image via override"));
    }

    #[cfg(unix)]
    #[test]
    fn hung_command_times_out() {
        let cli = CliConfig {
            command: vec!["sh".into(), "-c".into(), "cat {file}; sleep 30".into()],
            ..CliConfig::default()
        };
        let cfg = Config {
            request_timeout_seconds: 1,
            ..cfg_with(cli)
        };
        let t = CliTranscriber::new(&cfg).unwrap();
        let start = Instant::now();
        let err = t.transcribe(&text_input("x")).unwrap_err();
        assert!(format!("{err:#}").contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}
