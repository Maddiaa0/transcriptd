use anyhow::{bail, Result};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use transcriptd::config::Config;
use transcriptd::openrouter::{Payload, TranscribeInput, TranscribeOutput, Transcriber};
use transcriptd::scan::{scan, sidecar_path};
use transcriptd::state::{Failures, StateDir};

struct Mock {
    calls: Cell<usize>,
    fail_files: RefCell<HashSet<String>>,
}

impl Mock {
    fn new() -> Self {
        Mock {
            calls: Cell::new(0),
            fail_files: RefCell::new(HashSet::new()),
        }
    }
    fn fail_on(&self, name: &str) {
        self.fail_files.borrow_mut().insert(name.to_string());
    }
    fn heal(&self, name: &str) {
        self.fail_files.borrow_mut().remove(name);
    }
}

impl Transcriber for Mock {
    fn transcribe(&self, input: &TranscribeInput) -> Result<TranscribeOutput> {
        self.calls.set(self.calls.get() + 1);
        if self.fail_files.borrow().contains(&input.filename) {
            bail!("mock API error");
        }
        let detail = match &input.payload {
            Payload::Image { data, .. } => format!("image:{}", data.len()),
            Payload::Pdf { data } => format!("pdf:{}", data.len()),
            Payload::Text { body } => format!("text:{}", body.trim()),
        };
        Ok(TranscribeOutput {
            markdown: format!("transcript of {} ({detail})", input.filename),
            raw: json!({ "mock": true, "file": input.filename }),
        })
    }
}

fn test_config() -> Config {
    Config {
        stability_seconds: 0, // tests write files "just now"
        ..Config::default()
    }
}

fn setup(dir: &Path) -> StateDir {
    StateDir::new(dir).unwrap()
}

#[test]
fn ae1_unchanged_rescan_makes_zero_api_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config();
    let state = setup(tmp.path());
    fs::write(tmp.path().join("a.png"), b"fake png bytes").unwrap();

    let mock = Mock::new();
    let first = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(first.transcribed, 1);
    assert_eq!(first.api_calls, 1);
    assert!(sidecar_path(&tmp.path().join("a.png")).exists());

    let second = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(second.api_calls, 0);
    assert_eq!(second.up_to_date, 1);
    assert_eq!(mock.calls.get(), 1);
}

#[test]
fn ae2_changed_content_is_retranscribed_and_sidecar_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config();
    let state = setup(tmp.path());
    let page = tmp.path().join("page.png");
    fs::write(&page, b"version one").unwrap();

    let mock = Mock::new();
    scan(tmp.path(), &cfg, &state, &mock).unwrap();
    let before = fs::read_to_string(sidecar_path(&page)).unwrap();

    fs::write(&page, b"version two - edited in the app").unwrap();
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.api_calls, 1);
    assert_eq!(outcome.transcribed, 1);

    let after = fs::read_to_string(sidecar_path(&page)).unwrap();
    assert_ne!(before, after, "sidecar must be replaced on content change");
    assert_eq!(mock.calls.get(), 2);
}

#[test]
fn ae3_unstable_file_is_deferred_until_stable() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        stability_seconds: 3600, // everything written "just now" is unstable
        ..Config::default()
    };
    let state = setup(tmp.path());
    fs::write(tmp.path().join("syncing.png"), b"partial").unwrap();

    let mock = Mock::new();
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.unstable, 1);
    assert_eq!(outcome.api_calls, 0);
    assert!(!sidecar_path(&tmp.path().join("syncing.png")).exists());

    // Once stable (stability window of 0), the same scan path picks it up.
    let cfg_stable = test_config();
    let outcome = scan(tmp.path(), &cfg_stable, &state, &mock).unwrap();
    assert_eq!(outcome.transcribed, 1);
}

#[test]
fn ae4_failure_writes_no_sidecar_and_is_retried_next_sweep() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config();
    let state = setup(tmp.path());
    fs::write(tmp.path().join("bad.png"), b"will fail").unwrap();

    let mock = Mock::new();
    mock.fail_on("bad.png");
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.failed, 1);
    assert!(!sidecar_path(&tmp.path().join("bad.png")).exists());

    let failures = Failures::load(&state).unwrap();
    assert_eq!(failures.entries.len(), 1);
    assert!(failures.entries["bad.png"]
        .last_error
        .contains("mock API error"));

    // Next sweep retries; once the API recovers, the sidecar appears and the
    // failure record clears.
    mock.heal("bad.png");
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.transcribed, 1);
    assert!(sidecar_path(&tmp.path().join("bad.png")).exists());
    assert!(Failures::load(&state).unwrap().entries.is_empty());
}

#[test]
fn output_dir_redirects_sidecars_and_rollups_into_mirrored_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        output_dir: Some("transcripts".to_string()),
        ..test_config()
    };
    let state = setup(tmp.path());
    let doc = tmp.path().join("notebook");
    fs::create_dir(&doc).unwrap();
    fs::write(
        doc.join("index.md"),
        format!("# Notebook\n\n{}\n", cfg.marker),
    )
    .unwrap();
    fs::write(doc.join("01.png"), b"page one").unwrap();
    fs::write(tmp.path().join("loose.png"), b"loose page").unwrap();

    let mock = Mock::new();
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.transcribed, 2);
    assert_eq!(outcome.rollups_written, 1);

    // Generated markdown lands in the mirrored tree, not next to sources.
    let out = tmp.path().join("transcripts");
    assert!(out.join("loose.png.md").exists());
    assert!(out.join("notebook/01.png.md").exists());
    assert!(out.join("notebook").join(&cfg.rollup_name).exists());
    assert!(!sidecar_path(&tmp.path().join("loose.png")).exists());
    assert!(!doc.join(&cfg.rollup_name).exists());

    // The output tree is never treated as a source, and the rescan is
    // idempotent: zero API calls, nothing rewritten.
    let second = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(second.api_calls, 0);
    assert_eq!(second.up_to_date, 2);
    assert_eq!(second.rollups_written, 0);

    // A deleted output file is rebuilt from cache without an API call.
    fs::remove_file(out.join("loose.png.md")).unwrap();
    let third = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(third.api_calls, 0);
    assert_eq!(third.reused, 1);
    assert!(out.join("loose.png.md").exists());
}

#[test]
fn ae5_marked_folder_gets_rollup_and_incremental_page_restitches_from_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config();
    let state = setup(tmp.path());
    let doc = tmp.path().join("notebook");
    fs::create_dir(&doc).unwrap();
    fs::write(
        doc.join("index.md"),
        format!("# Notebook\n\n{}\n", cfg.marker),
    )
    .unwrap();
    fs::write(doc.join("01.png"), b"page one").unwrap();
    fs::write(doc.join("02.png"), b"page two").unwrap();

    let mock = Mock::new();
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.transcribed, 2);
    assert_eq!(outcome.rollups_written, 1);

    let rollup = fs::read_to_string(doc.join(&cfg.rollup_name)).unwrap();
    let pos1 = rollup.find("## 01.png").unwrap();
    let pos2 = rollup.find("## 02.png").unwrap();
    assert!(pos1 < pos2, "sections must be in filename order");

    // One new page: exactly one API call, rollup restitched from cache.
    fs::write(doc.join("03.png"), b"page three").unwrap();
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.api_calls, 1);
    assert_eq!(outcome.rollups_written, 1);
    let rollup = fs::read_to_string(doc.join(&cfg.rollup_name)).unwrap();
    assert!(rollup.contains("## 03.png"));

    // Unchanged corpus: rollup content identical, nothing rewritten (AE1).
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.api_calls, 0);
    assert_eq!(outcome.rollups_written, 0);
}

#[test]
fn ae6_docx_is_transcribed_and_unknown_extension_is_logged_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config();
    let state = setup(tmp.path());

    // Minimal docx: a zip with word/document.xml.
    let docx_path = tmp.path().join("memo.docx");
    let file = fs::File::create(&docx_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zip.start_file("word/document.xml", options).unwrap();
    zip.write_all(b"<w:document><w:p><w:r><w:t>Quarterly memo</w:t></w:r></w:p></w:document>")
        .unwrap();
    zip.finish().unwrap();

    fs::write(tmp.path().join("mystery.xyz"), b"???").unwrap();

    let mock = Mock::new();
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.transcribed, 1);
    assert_eq!(outcome.unsupported, 1);

    let sidecar = fs::read_to_string(sidecar_path(&docx_path)).unwrap();
    assert!(sidecar.contains("Quarterly memo"));
    assert!(!sidecar_path(&tmp.path().join("mystery.xyz")).exists());
}

#[test]
fn renamed_file_reuses_cache_without_api_call() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config();
    let state = setup(tmp.path());
    let old = tmp.path().join("old-name.png");
    fs::write(&old, b"same content").unwrap();

    let mock = Mock::new();
    scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(mock.calls.get(), 1);

    let new = tmp.path().join("new-name.png");
    fs::rename(&old, &new).unwrap();
    fs::remove_file(sidecar_path(&old)).unwrap();

    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.api_calls, 0, "recorded hash is never re-sent (R2)");
    assert_eq!(outcome.reused, 1);
    assert!(sidecar_path(&new).exists());
    assert_eq!(mock.calls.get(), 1);
}

#[test]
fn markdown_and_hidden_files_are_never_transcribed() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config();
    let state = setup(tmp.path());
    fs::write(tmp.path().join("notes.md"), "# existing markdown").unwrap();
    fs::write(tmp.path().join(".hidden.png"), b"hidden").unwrap();

    let mock = Mock::new();
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.api_calls, 0);
    assert_eq!(outcome.unsupported, 0);
}

#[test]
fn sidecar_frontmatter_records_provenance() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config();
    let state = setup(tmp.path());
    fs::write(tmp.path().join("a.png"), b"content").unwrap();

    let mock = Mock::new();
    scan(tmp.path(), &cfg, &state, &mock).unwrap();
    let sidecar = fs::read_to_string(sidecar_path(&tmp.path().join("a.png"))).unwrap();
    assert!(sidecar.starts_with("---\n"));
    assert!(sidecar.contains("source: a.png"));
    assert!(sidecar.contains("sha256: "));
    assert!(sidecar.contains(&format!("model: {}", cfg.model)));
    assert!(sidecar.contains(&format!("prompt_version: {}", cfg.prompt_version)));
}
