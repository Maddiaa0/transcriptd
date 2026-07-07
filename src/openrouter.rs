use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde_json::{json, Value};
use std::time::Duration;

use crate::config::Config;

/// Anything that can turn a file into markdown. The OpenRouter client is the
/// real implementation; tests substitute a mock so the scan logic is
/// verifiable without spending API credit.
pub trait Transcriber {
    fn transcribe(&self, input: &TranscribeInput) -> Result<TranscribeOutput>;
}

pub enum Payload {
    Image { mime: &'static str, data: Vec<u8> },
    Pdf { data: Vec<u8> },
    Text { body: String },
}

pub struct TranscribeInput {
    pub filename: String,
    pub payload: Payload,
}

#[derive(Debug)]
pub struct TranscribeOutput {
    pub markdown: String,
    /// Full API response, retained per R7 so richer schemas later need no
    /// re-transcription.
    pub raw: Value,
}

pub struct OpenRouterClient {
    http: reqwest::blocking::Client,
    api_key: String,
    model: String,
    prompt: String,
    pdf_engine: String,
    api_base: String,
}

impl OpenRouterClient {
    pub fn new(cfg: &Config, api_key: String) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(cfg.request_timeout_seconds))
            .build()
            .context("building HTTP client")?;
        Ok(OpenRouterClient {
            http,
            api_key,
            model: cfg.model.clone(),
            prompt: cfg.prompt().to_string(),
            pdf_engine: cfg.pdf_engine.clone(),
            api_base: cfg.api_base.trim_end_matches('/').to_string(),
        })
    }
}

impl Transcriber for OpenRouterClient {
    fn transcribe(&self, input: &TranscribeInput) -> Result<TranscribeOutput> {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut body = match &input.payload {
            Payload::Image { mime, data } => json!({
                "model": self.model,
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "text", "text": self.prompt },
                        { "type": "image_url", "image_url": {
                            "url": format!("data:{mime};base64,{}", b64.encode(data))
                        }}
                    ]
                }]
            }),
            Payload::Pdf { data } => json!({
                "model": self.model,
                "messages": [{
                    "role": "user",
                    "content": [
                        { "type": "text", "text": self.prompt },
                        { "type": "file", "file": {
                            "filename": input.filename,
                            "file_data": format!("data:application/pdf;base64,{}", b64.encode(data))
                        }}
                    ]
                }]
            }),
            Payload::Text { body } => json!({
                "model": self.model,
                "messages": [{
                    "role": "user",
                    "content": format!("{}\n\n---\n\n{}", self.prompt, body)
                }]
            }),
        };
        if matches!(input.payload, Payload::Pdf { .. }) {
            body["plugins"] =
                json!([{ "id": "file-parser", "pdf": { "engine": self.pdf_engine } }]);
        }

        if crate::state::log_level() >= crate::state::Level::Trace {
            crate::state::console_log(
                crate::state::Level::Trace,
                &format!(
                    "POST {}/chat/completions model={} ({} KB body)",
                    self.api_base,
                    self.model,
                    body.to_string().len() / 1024
                ),
            );
        }
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.api_base))
            .bearer_auth(&self.api_key)
            .header("X-Title", "transcriptd")
            .json(&body)
            .send()
            .with_context(|| format!("request failed for {}", input.filename))?;

        let status = resp.status();
        let text = resp.text().context("reading response body")?;
        if !status.is_success() {
            bail!("OpenRouter returned {status}: {}", truncate(&text, 500));
        }
        let raw: Value = serde_json::from_str(&text)
            .with_context(|| format!("non-JSON response: {}", truncate(&text, 200)))?;
        let markdown = extract_markdown(&raw)?;
        Ok(TranscribeOutput { markdown, raw })
    }
}

/// Pull the transcript out of a chat-completions response. Also used to
/// rebuild a lost cache entry from a retained raw response.
pub fn extract_markdown(raw: &Value) -> Result<String> {
    if let Some(err) = raw.get("error") {
        bail!("OpenRouter error: {}", truncate(&err.to_string(), 500));
    }
    let content = raw
        .pointer("/choices/0/message/content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let markdown = strip_fences(content);
    if markdown.is_empty() {
        // Never record an empty transcript as done (R10).
        bail!("empty transcription in response");
    }
    Ok(markdown)
}

/// Models sometimes wrap output in a code fence despite instructions.
pub(crate) fn strip_fences(s: &str) -> String {
    let trimmed = s.trim();
    if !trimmed.starts_with("```") {
        return trimmed.to_string();
    }
    let mut lines: Vec<&str> = trimmed.lines().collect();
    lines.remove(0);
    if lines.last().is_some_and(|l| l.trim() == "```") {
        lines.pop();
    }
    lines.join("\n").trim().to_string()
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_code_fences() {
        assert_eq!(strip_fences("```markdown\n# Hi\n```"), "# Hi");
        assert_eq!(strip_fences("# Hi"), "# Hi");
        assert_eq!(strip_fences("```\nbody\n```"), "body");
    }

    #[test]
    fn empty_content_is_an_error() {
        let raw = json!({ "choices": [{ "message": { "content": "   " } }] });
        assert!(extract_markdown(&raw).is_err());
    }

    #[test]
    fn api_error_field_is_an_error() {
        let raw = json!({ "error": { "message": "model offline" } });
        assert!(extract_markdown(&raw).is_err());
    }

    use serde_json::json;
}
