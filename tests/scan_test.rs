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

fn frontmatter_json(path: &Path) -> serde_json::Value {
    let markdown = fs::read_to_string(path).unwrap();
    let rest = markdown.strip_prefix("---\n").unwrap();
    let (frontmatter, _) = rest.split_once("\n---\n").unwrap();
    serde_json::from_str(frontmatter).unwrap()
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
fn rollup_dir_rolls_up_every_folder_from_direct_files() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        rollup_dir: Some("rollups".to_string()),
        ..test_config()
    };
    let state = setup(tmp.path());
    // No index.md markers anywhere: every folder still rolls up.
    fs::create_dir_all(tmp.path().join("topic1")).unwrap();
    fs::create_dir_all(tmp.path().join("topic2/drafts")).unwrap();
    fs::write(tmp.path().join("topic1/a.png"), b"page a").unwrap();
    fs::write(tmp.path().join("topic1/b.png"), b"page b").unwrap();
    fs::write(tmp.path().join("topic2/drafts/c.png"), b"page c").unwrap();
    fs::write(tmp.path().join("loose.png"), b"loose page").unwrap();

    let mock = Mock::new();
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(outcome.transcribed, 4);
    // topic1, topic2/drafts, and the root each roll up; topic2 has no
    // direct files so it gets no rollup of its own.
    assert_eq!(outcome.rollups_written, 3);

    let out = tmp.path().join("rollups");
    let topic1 = fs::read_to_string(out.join("topic1").join(&cfg.rollup_name)).unwrap();
    assert!(topic1.contains("## a.png") && topic1.contains("## b.png"));
    let drafts = fs::read_to_string(out.join("topic2/drafts").join(&cfg.rollup_name)).unwrap();
    assert!(drafts.contains("## c.png"));
    assert!(!drafts.contains("## a.png"), "direct files only");
    let root = fs::read_to_string(out.join(&cfg.rollup_name)).unwrap();
    assert!(root.contains("## loose.png"));
    assert!(!out.join("topic2").join(&cfg.rollup_name).exists());

    // Rollups exist ONLY in the rollup tree, never at folder roots.
    assert!(!tmp.path().join("topic1").join(&cfg.rollup_name).exists());
    // Sidecars still land next to sources (rollup_dir doesn't move them).
    assert!(sidecar_path(&tmp.path().join("topic1/a.png")).exists());

    // Idempotent rescan: rollup tree not scanned, nothing rewritten.
    let second = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(second.api_calls, 0);
    assert_eq!(second.rollups_written, 0);
    assert_eq!(second.up_to_date, 4);
}

#[test]
fn rollup_dir_replaces_marker_rollups() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        rollup_dir: Some("rollups".to_string()),
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

    let mock = Mock::new();
    scan(tmp.path(), &cfg, &state, &mock).unwrap();
    // The marker is inert while rollup_dir is set: no in-place rollup.
    assert!(!doc.join(&cfg.rollup_name).exists());
    assert!(tmp
        .path()
        .join("rollups/notebook")
        .join(&cfg.rollup_name)
        .exists());
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
fn changed_source_failure_removes_the_old_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config();
    let state = setup(tmp.path());
    let source = tmp.path().join("page.png");
    fs::write(&source, b"original").unwrap();

    let mock = Mock::new();
    scan(tmp.path(), &cfg, &state, &mock).unwrap();
    let sidecar = sidecar_path(&source);
    assert!(sidecar.exists());

    fs::write(&source, b"changed").unwrap();
    mock.fail_on("page.png");
    let outcome = scan(tmp.path(), &cfg, &state, &mock).unwrap();

    assert_eq!(outcome.failed, 1);
    assert!(
        !sidecar.exists(),
        "a stale successful transcript must not survive"
    );
}

#[test]
fn deleting_a_source_removes_its_generated_outputs() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        output_dir: Some("generated".to_string()),
        rollup_dir: Some("generated".to_string()),
        ..test_config()
    };
    let state = setup(tmp.path());
    let source = tmp.path().join("topic/page.png");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, b"page").unwrap();

    let mock = Mock::new();
    scan(tmp.path(), &cfg, &state, &mock).unwrap();
    let sidecar = tmp.path().join("generated/topic/page.png.md");
    let rollup = tmp.path().join("generated/topic/transcript.md");
    assert!(sidecar.exists());
    assert!(rollup.exists());

    fs::remove_file(source).unwrap();
    scan(tmp.path(), &cfg, &state, &mock).unwrap();

    assert!(!sidecar.exists());
    assert!(!rollup.exists());
}

#[test]
fn removing_a_document_marker_removes_its_rollup() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config();
    let state = setup(tmp.path());
    let doc = tmp.path().join("notebook");
    fs::create_dir(&doc).unwrap();
    fs::write(doc.join("index.md"), &cfg.marker).unwrap();
    fs::write(doc.join("page.png"), b"page").unwrap();

    let mock = Mock::new();
    scan(tmp.path(), &cfg, &state, &mock).unwrap();
    let rollup = doc.join(&cfg.rollup_name);
    assert!(rollup.exists());

    fs::write(doc.join("index.md"), "# Ordinary folder").unwrap();
    scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert!(!rollup.exists());
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
    let properties = frontmatter_json(&sidecar_path(&tmp.path().join("a.png")));
    assert_eq!(properties["source"], "a.png");
    assert!(properties["sha256"].as_str().is_some_and(|v| !v.is_empty()));
    assert_eq!(properties["model"], cfg.model);
    assert_eq!(properties["backend"], cfg.backend);
    assert_eq!(properties["prompt_version"], cfg.prompt_version);
    assert!(properties["prompt_sha256"]
        .as_str()
        .is_some_and(|v| !v.is_empty()));
    assert_eq!(properties["kind"], "transcript");
    assert_eq!(properties["tags"], json!(["transcriptd"]));
}

#[test]
fn frontmatter_preserves_yaml_significant_names_and_values() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        model: "vendor/model: latest # production".to_string(),
        prompt_version: "v2: #review".to_string(),
        rollup_dir: Some("generated".to_string()),
        ..test_config()
    };
    let state = setup(tmp.path());
    let topic = tmp.path().join("topic: #1");
    fs::create_dir_all(&topic).unwrap();
    let source = topic.join("# draft: true.png");
    fs::write(&source, b"content").unwrap();

    let mock = Mock::new();
    scan(tmp.path(), &cfg, &state, &mock).unwrap();

    let sidecar = frontmatter_json(&sidecar_path(&source));
    assert_eq!(sidecar["source"], "# draft: true.png");
    assert_eq!(sidecar["model"], cfg.model);
    assert_eq!(sidecar["prompt_version"], cfg.prompt_version);

    let rollup = frontmatter_json(&tmp.path().join("generated/topic: #1/transcript.md"));
    assert_eq!(rollup["folder"], "topic: #1");
    assert_eq!(rollup["kind"], "rollup");
    assert_eq!(rollup["sections"], 1);

    let second = scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(second.api_calls, 0);
    assert_eq!(second.up_to_date, 1);
}

#[test]
fn generation_setting_changes_refresh_existing_content() {
    let tmp = tempfile::tempdir().unwrap();
    let state = setup(tmp.path());
    fs::write(tmp.path().join("page.png"), b"unchanged content").unwrap();
    let mock = Mock::new();

    let cfg = test_config();
    scan(tmp.path(), &cfg, &state, &mock).unwrap();
    assert_eq!(mock.calls.get(), 1);

    let changed_model = Config {
        model: "example/new-model".to_string(),
        ..cfg.clone()
    };
    let outcome = scan(tmp.path(), &changed_model, &state, &mock).unwrap();
    assert_eq!(outcome.transcribed, 1);
    assert_eq!(mock.calls.get(), 2);

    let changed_backend = Config {
        backend: "cli".to_string(),
        ..changed_model.clone()
    };
    scan(tmp.path(), &changed_backend, &state, &mock).unwrap();
    assert_eq!(mock.calls.get(), 3);

    let changed_prompt_version = Config {
        prompt_version: "2".to_string(),
        ..changed_backend.clone()
    };
    scan(tmp.path(), &changed_prompt_version, &state, &mock).unwrap();
    assert_eq!(mock.calls.get(), 4);

    // The actual prompt is fingerprinted too, so forgetting to bump the
    // human-readable version cannot silently retain old output.
    let changed_prompt = Config {
        prompt: Some("A materially different prompt".to_string()),
        ..changed_prompt_version
    };
    scan(tmp.path(), &changed_prompt, &state, &mock).unwrap();
    assert_eq!(mock.calls.get(), 5);
}

#[test]
fn failed_generation_refresh_does_not_reuse_old_output_in_rollups() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        output_dir: Some("generated".to_string()),
        rollup_dir: Some("generated".to_string()),
        ..test_config()
    };
    let state = setup(tmp.path());
    fs::write(tmp.path().join("page.png"), b"unchanged content").unwrap();
    let mock = Mock::new();

    scan(tmp.path(), &cfg, &state, &mock).unwrap();
    let sidecar = tmp.path().join("generated/page.png.md");
    let rollup = tmp.path().join("generated/transcript.md");
    assert!(sidecar.exists());
    assert!(rollup.exists());

    mock.fail_on("page.png");
    let changed = Config {
        model: "example/new-model".to_string(),
        ..cfg
    };
    let outcome = scan(tmp.path(), &changed, &state, &mock).unwrap();

    assert_eq!(outcome.failed, 1);
    assert!(!sidecar.exists());
    assert!(!rollup.exists());
}

#[test]
fn unreadable_marker_paths_are_reported_as_scan_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config();
    let state = setup(tmp.path());
    let notebook = tmp.path().join("notebook");
    fs::create_dir_all(notebook.join("index.md")).unwrap();
    fs::write(notebook.join("page.png"), b"content").unwrap();

    let error = scan(tmp.path(), &cfg, &state, &Mock::new()).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("reading marker file"), "{message}");
    assert!(message.contains("index.md"), "{message}");
}
