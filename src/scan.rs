use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use walkdir::{DirEntry, WalkDir};

use crate::config::Config;
use crate::extract;
use crate::openrouter::{self, Payload, TranscribeInput, Transcriber};
use crate::state::{now_rfc3339, write_atomic, Failures, Ledger, LedgerEntry, StateDir};

#[derive(Debug, Default)]
pub struct ScanOutcome {
    pub api_calls: usize,
    pub transcribed: usize,
    /// Sidecar rebuilt from cache with no API call (moved/renamed file, or a
    /// deleted sidecar).
    pub reused: usize,
    pub up_to_date: usize,
    /// Mid-sync files deferred to a later sweep (R4).
    pub unstable: usize,
    pub unsupported: usize,
    pub failed: usize,
    pub rollups_written: usize,
}

impl ScanOutcome {
    pub fn summary(&self) -> String {
        format!(
            "{} transcribed, {} reused from cache, {} up to date, {} deferred (unstable), {} unsupported, {} failed, {} rollups written, {} API calls",
            self.transcribed,
            self.reused,
            self.up_to_date,
            self.unstable,
            self.unsupported,
            self.failed,
            self.rollups_written,
            self.api_calls
        )
    }
}

enum FileKind {
    Image(&'static str),
    Pdf,
    Docx,
    Text,
    Markdown,
    Unsupported,
}

fn classify(path: &Path) -> FileKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => FileKind::Image("image/png"),
        "jpg" | "jpeg" => FileKind::Image("image/jpeg"),
        "webp" => FileKind::Image("image/webp"),
        "pdf" => FileKind::Pdf,
        "docx" => FileKind::Docx,
        "txt" | "text" | "csv" | "tsv" | "rst" | "org" | "html" | "htm" => FileKind::Text,
        "md" | "markdown" => FileKind::Markdown,
        _ => FileKind::Unsupported,
    }
}

fn is_hidden(entry: &DirEntry) -> bool {
    entry.depth() > 0
        && entry
            .file_name()
            .to_str()
            .is_some_and(|s| s.starts_with('.'))
}

/// One idempotent sweep: walk the folder, diff against the ledger,
/// transcribe the delta, restitch rollups, exit. Scheduled runs, manual runs,
/// cold-start backlog, and watch mode all share this path.
pub fn scan(
    folder: &Path,
    cfg: &Config,
    state: &StateDir,
    transcriber: &dyn Transcriber,
) -> Result<ScanOutcome> {
    let mut ledger = Ledger::load(state)?;
    let mut failures = Failures::load(state)?;
    let mut outcome = ScanOutcome::default();
    // abs path -> content hash, for rollup stitching after the file pass
    let mut hashes: BTreeMap<PathBuf, String> = BTreeMap::new();
    let now = SystemTime::now();
    let max_bytes = cfg.max_file_mb * 1024 * 1024;
    let out_root = cfg.output_root(folder);
    // An output tree inside the watched folder holds only generated markdown;
    // never treat anything dropped there as a source.
    let excluded_root = out_root
        .clone()
        .filter(|r| r != folder && r.starts_with(folder));

    let files: Vec<PathBuf> = WalkDir::new(folder)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !is_hidden(e))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| excluded_root.as_ref().is_none_or(|r| !p.starts_with(r)))
        .collect();

    for path in files {
        let rel = path
            .strip_prefix(folder)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        let kind = classify(&path);
        if matches!(kind, FileKind::Markdown) {
            continue; // already markdown: sidecars, rollups, index.md, user notes
        }
        if matches!(kind, FileKind::Unsupported) {
            outcome.unsupported += 1;
            if !ledger.skipped.contains_key(&rel) {
                state.log("INFO", &format!("skipping unsupported file: {rel}"));
                ledger
                    .skipped
                    .insert(rel.clone(), "unsupported type".to_string());
            }
            continue;
        }

        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                state.log("WARN", &format!("cannot stat {rel}: {e}"));
                continue;
            }
        };
        if meta.len() > max_bytes {
            outcome.unsupported += 1;
            if !ledger.skipped.contains_key(&rel) {
                state.log(
                    "INFO",
                    &format!("skipping {rel}: larger than {} MB", cfg.max_file_mb),
                );
                ledger.skipped.insert(rel.clone(), "too large".to_string());
            }
            continue;
        }
        // Stability gate (R4): a file modified within the window may still be
        // syncing. Defer it; the next sweep picks it up.
        let stable = meta
            .modified()
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age.as_secs() >= cfg.stability_seconds);
        if !stable && cfg.stability_seconds > 0 {
            outcome.unstable += 1;
            state.log("INFO", &format!("deferring {rel}: modified too recently"));
            continue;
        }

        let sha = match hash_file(&path) {
            Ok(s) => s,
            Err(e) => {
                state.log("WARN", &format!("cannot hash {rel}: {e}"));
                continue;
            }
        };
        hashes.insert(path.clone(), sha.clone());

        let mut needs_api = true;
        if let Some(entry) = ledger.entries.get(&sha).cloned() {
            // Hash already recorded: never re-sent to the API (R2).
            let sc = resolve_sidecar(folder, out_root.as_deref(), &path);
            if sidecar_matches(&sc, &sha) {
                outcome.up_to_date += 1;
                needs_api = false;
            } else if let Some(md) = cached_markdown(state, &sha) {
                let content = sidecar_content(
                    &path,
                    &sha,
                    &entry.model,
                    &entry.prompt_version,
                    &entry.transcribed_at,
                    &md,
                );
                write_output(&sc, content.as_bytes())?;
                state.log("INFO", &format!("rebuilt sidecar from cache: {rel}"));
                outcome.reused += 1;
                needs_api = false;
            } else {
                state.log(
                    "WARN",
                    &format!("cache missing for recorded hash of {rel}; re-transcribing"),
                );
            }
            if !needs_api && entry.path != rel {
                ledger.entries.get_mut(&sha).unwrap().path = rel.clone();
            }
        }
        if !needs_api {
            continue;
        }

        let input = match build_input(&path, kind) {
            Ok(i) => i,
            Err(e) => {
                state.log("ERROR", &format!("cannot read {rel}: {e:#}"));
                failures.record(&rel, &sha, &format!("{e:#}"));
                failures.save(state)?;
                outcome.failed += 1;
                continue;
            }
        };

        outcome.api_calls += 1;
        match transcriber.transcribe(&input) {
            Ok(out) => {
                write_atomic(
                    &state.raw_path(&sha),
                    serde_json::to_vec_pretty(&out.raw)?.as_slice(),
                )?;
                write_atomic(&state.cache_path(&sha), out.markdown.as_bytes())?;
                let transcribed_at = now_rfc3339();
                let content = sidecar_content(
                    &path,
                    &sha,
                    &cfg.model,
                    &cfg.prompt_version,
                    &transcribed_at,
                    &out.markdown,
                );
                write_output(
                    &resolve_sidecar(folder, out_root.as_deref(), &path),
                    content.as_bytes(),
                )?;
                ledger.entries.insert(
                    sha.clone(),
                    LedgerEntry {
                        path: rel.clone(),
                        model: cfg.model.clone(),
                        prompt_version: cfg.prompt_version.clone(),
                        transcribed_at,
                    },
                );
                failures.clear(&rel);
                // Save after every success so a crash never loses a paid call.
                ledger.save(state)?;
                failures.save(state)?;
                outcome.transcribed += 1;
                state.log("INFO", &format!("transcribed {rel}"));
            }
            Err(e) => {
                // No sidecar on failure (R10); retried next sweep (R12).
                failures.record(&rel, &sha, &format!("{e:#}"));
                failures.save(state)?;
                outcome.failed += 1;
                state.log("ERROR", &format!("transcription failed for {rel}: {e:#}"));
            }
        }
    }

    outcome.rollups_written = stitch_rollups(folder, cfg, state, &hashes)?;

    ledger.save(state)?;
    failures.save(state)?;
    Ok(outcome)
}

fn build_input(path: &Path, kind: FileKind) -> Result<TranscribeInput> {
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let payload = match kind {
        FileKind::Image(mime) => Payload::Image {
            mime,
            data: fs::read(path)?,
        },
        FileKind::Pdf => Payload::Pdf {
            data: fs::read(path)?,
        },
        FileKind::Docx => Payload::Text {
            body: extract::docx_to_text(path)?,
        },
        FileKind::Text => Payload::Text {
            body: String::from_utf8_lossy(&fs::read(path)?).to_string(),
        },
        FileKind::Markdown | FileKind::Unsupported => unreachable!("filtered before build_input"),
    };
    Ok(TranscribeInput { filename, payload })
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn sidecar_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.md"))
}

/// Where a source's sidecar lives: next to the source by default, or at the
/// mirrored relative path under `output_dir` when configured.
fn resolve_sidecar(folder: &Path, out_root: Option<&Path>, source: &Path) -> PathBuf {
    match out_root {
        None => sidecar_path(source),
        Some(root) => root.join(sidecar_path(source.strip_prefix(folder).unwrap_or(source))),
    }
}

/// write_atomic, creating parent directories first — a redirected output
/// tree is built lazily as sidecars land in it.
fn write_output(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    write_atomic(path, contents)
}

fn sidecar_matches(sidecar: &Path, sha: &str) -> bool {
    fs::read_to_string(sidecar)
        .map(|text| text.contains(&format!("sha256: {sha}")))
        .unwrap_or(false)
}

/// Recover a transcript without an API call: from the markdown cache, or by
/// re-extracting from the retained raw response (R7) if the cache was lost.
fn cached_markdown(state: &StateDir, sha: &str) -> Option<String> {
    if let Ok(md) = fs::read_to_string(state.cache_path(sha)) {
        return Some(md);
    }
    let raw_text = fs::read_to_string(state.raw_path(sha)).ok()?;
    let raw: serde_json::Value = serde_json::from_str(&raw_text).ok()?;
    let md = openrouter::extract_markdown(&raw).ok()?;
    let _ = write_atomic(&state.cache_path(sha), md.as_bytes());
    Some(md)
}

fn sidecar_content(
    source: &Path,
    sha: &str,
    model: &str,
    prompt_version: &str,
    transcribed_at: &str,
    markdown: &str,
) -> String {
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    format!(
        "---\nsource: {name}\nsha256: {sha}\nmodel: {model}\nprompt_version: {prompt_version}\ntranscribed_at: {transcribed_at}\ngenerator: transcriptd {}\n---\n\n{markdown}\n",
        crate::VERSION
    )
}

/// A folder whose index.md contains the configured marker is a document:
/// maintain a stitched rollup at its root, one section per transcribed file
/// in filename order. Pure reassembly from cache — zero API calls (R9).
fn stitch_rollups(
    folder: &Path,
    cfg: &Config,
    state: &StateDir,
    hashes: &BTreeMap<PathBuf, String>,
) -> Result<usize> {
    let marked = find_marked_folders(folder, &cfg.marker);
    if marked.is_empty() {
        return Ok(0);
    }

    // Each file belongs to its nearest marked ancestor only.
    let mut groups: BTreeMap<&PathBuf, Vec<(&PathBuf, &String)>> = BTreeMap::new();
    for (path, sha) in hashes {
        if let Some(owner) = nearest_marked_ancestor(path, &marked, folder) {
            groups.entry(owner).or_default().push((path, sha));
        }
    }

    let mut written = 0;
    for marked_folder in &marked {
        let members = groups.get(marked_folder).cloned().unwrap_or_default();
        let mut sections = Vec::new();
        for (path, sha) in &members {
            let Some(md) = cached_markdown(state, sha) else {
                continue; // not yet transcribed (failed or deferred)
            };
            let rel_name = path
                .strip_prefix(marked_folder)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            sections.push(format!("## {rel_name}\n\n{}", md.trim()));
        }
        if sections.is_empty() {
            continue;
        }
        let folder_rel = marked_folder
            .strip_prefix(folder)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let folder_rel = if folder_rel.is_empty() {
            ".".to_string()
        } else {
            folder_rel
        };
        let content = format!(
            "---\ngenerator: transcriptd {}\nkind: rollup\nfolder: {folder_rel}\nsections: {}\n---\n\n{}\n",
            crate::VERSION,
            sections.len(),
            sections.join("\n\n")
        );
        let rollup_path = match cfg.output_root(folder) {
            None => marked_folder.join(&cfg.rollup_name),
            Some(root) => root
                .join(marked_folder.strip_prefix(folder).unwrap_or(marked_folder))
                .join(&cfg.rollup_name),
        };
        let existing = fs::read_to_string(&rollup_path).unwrap_or_default();
        if existing != content {
            write_output(&rollup_path, content.as_bytes())?;
            state.log(
                "INFO",
                &format!("restitched rollup: {folder_rel}/{}", cfg.rollup_name),
            );
            written += 1;
        }
    }
    Ok(written)
}

fn find_marked_folders(folder: &Path, marker: &str) -> Vec<PathBuf> {
    WalkDir::new(folder)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !is_hidden(e))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .map(|e| e.into_path())
        .filter(|dir| {
            fs::read_to_string(dir.join("index.md"))
                .map(|text| text.contains(marker))
                .unwrap_or(false)
        })
        .collect()
}

fn nearest_marked_ancestor<'a>(
    path: &Path,
    marked: &'a [PathBuf],
    folder: &Path,
) -> Option<&'a PathBuf> {
    let mut current = path.parent();
    while let Some(dir) = current {
        if let Some(m) = marked.iter().find(|m| m.as_path() == dir) {
            return Some(m);
        }
        if dir == folder {
            break;
        }
        current = dir.parent();
    }
    None
}
