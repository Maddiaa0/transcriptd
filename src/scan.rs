use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
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
    let _scan_lock = state.acquire_scan_lock()?;
    let mut ledger = Ledger::load(state)?;
    let mut failures = Failures::load(state)?;
    let mut outcome = ScanOutcome::default();
    // abs path -> content hash, for rollup stitching after the file pass
    let mut hashes: BTreeMap<PathBuf, String> = BTreeMap::new();
    // Generated sidecar path -> current source hash. A None value means the
    // source still exists but could not be checked this sweep (for example it
    // is inside the stability window), so an existing sidecar is preserved.
    let mut expected_sidecars: BTreeMap<PathBuf, Option<String>> = BTreeMap::new();
    let now = SystemTime::now();
    let max_bytes = cfg.max_file_mb * 1024 * 1024;
    let out_root = cfg.output_root(folder);
    // Output/rollup trees inside the watched folder hold only generated
    // markdown; never treat anything dropped there as a source.
    let excluded_roots: Vec<PathBuf> = [out_root.clone(), cfg.rollup_root(folder)]
        .into_iter()
        .flatten()
        .filter(|r| r != folder && r.starts_with(folder))
        .collect();

    let files: Vec<PathBuf> = WalkDir::new(folder)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| !is_hidden(e))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| !excluded_roots.iter().any(|r| p.starts_with(r)))
        .collect();
    state.log(
        "DEBUG",
        &format!("sweep start: {} files to consider", files.len()),
    );
    // For per-file work logs: what actually does the transcription.
    let backend_desc = if cfg.backend == "cli" {
        cfg.cli
            .command
            .first()
            .or_else(|| cfg.cli.image_command.as_ref().and_then(|c| c.first()))
            .cloned()
            .unwrap_or_else(|| "cli".to_string())
    } else {
        cfg.model.clone()
    };

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

        let sidecar = resolve_sidecar(folder, out_root.as_deref(), &path);
        expected_sidecars.insert(sidecar.clone(), None);

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

        let source_bytes = match read_source_bytes(&path, max_bytes) {
            Ok(bytes) => bytes,
            Err(e) => {
                state.log("WARN", &format!("cannot snapshot {rel}: {e:#}"));
                continue;
            }
        };
        let sha = hash_bytes(&source_bytes);
        expected_sidecars.insert(sidecar.clone(), Some(sha.clone()));
        hashes.insert(path.clone(), sha.clone());

        let mut needs_api = true;
        if let Some(entry) = ledger.entries.get(&sha).cloned() {
            // Hash already recorded: never re-sent to the API (R2).
            if sidecar_matches(&sidecar, &sha) {
                state.log("DEBUG", &format!("up to date: {rel}"));
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
                write_output(&sidecar, content.as_bytes())?;
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

        let kind_name = match kind {
            FileKind::Image(_) => "image",
            FileKind::Pdf => "pdf",
            FileKind::Docx => "docx",
            FileKind::Text => "text",
            FileKind::Markdown | FileKind::Unsupported => unreachable!("filtered above"),
        };
        let input = match build_input(&path, kind, source_bytes) {
            Ok(input) => input,
            Err(e) => {
                state.log("ERROR", &format!("cannot read {rel}: {e:#}"));
                failures.record(&rel, &sha, &format!("{e:#}"));
                failures.save(state)?;
                outcome.failed += 1;
                continue;
            }
        };

        state.log(
            "DEBUG",
            &format!(
                "transcribing {rel} ({kind_name}, {} KB) via {backend_desc}",
                meta.len() / 1024
            ),
        );
        outcome.api_calls += 1;
        let started = std::time::Instant::now();
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
                write_output(&sidecar, content.as_bytes())?;
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
                state.log(
                    "INFO",
                    &format!(
                        "transcribed {rel} in {:.1}s ({} chars)",
                        started.elapsed().as_secs_f64(),
                        out.markdown.chars().count()
                    ),
                );
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

    reconcile_sidecars(
        folder,
        out_root.as_deref().unwrap_or(folder),
        state,
        &expected_sidecars,
    )?;

    let rollups = stitch_rollups(folder, cfg, state, &hashes)?;
    outcome.rollups_written = rollups.written;
    let rollup_root = cfg
        .rollup_root(folder)
        .or_else(|| cfg.output_root(folder))
        .unwrap_or_else(|| folder.to_path_buf());
    reconcile_rollups(folder, &rollup_root, state, &rollups.expected)?;

    ledger.save(state)?;
    failures.save(state)?;
    Ok(outcome)
}

/// Read one bounded, immutable snapshot so the caller can derive both the
/// cache key and model payload from those exact bytes. If the live file changes
/// afterwards, the next sweep sees a different hash and processes it normally.
fn read_source_bytes(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("file grew beyond the configured size limit while being read");
    }
    Ok(bytes)
}

fn build_input(path: &Path, kind: FileKind, bytes: Vec<u8>) -> Result<TranscribeInput> {
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let payload = match kind {
        FileKind::Image(mime) => Payload::Image { mime, data: bytes },
        FileKind::Pdf => Payload::Pdf { data: bytes },
        FileKind::Docx => Payload::Text {
            body: extract::docx_bytes_to_text(&bytes)?,
        },
        FileKind::Text => Payload::Text {
            body: String::from_utf8_lossy(&bytes).to_string(),
        },
        FileKind::Markdown | FileKind::Unsupported => {
            unreachable!("filtered before build_input")
        }
    };
    Ok(TranscribeInput { filename, payload })
}

fn hash_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
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

/// Remove generated sidecars whose source disappeared or whose recorded hash
/// no longer matches the current source. This prevents stale content from
/// remaining searchable after a deletion or failed re-transcription.
fn reconcile_sidecars(
    folder: &Path,
    output_root: &Path,
    state: &StateDir,
    expected: &BTreeMap<PathBuf, Option<String>>,
) -> Result<()> {
    for path in generated_markdown_files(output_root) {
        let Some(recorded_sha) = managed_sidecar_sha(&path) else {
            continue;
        };
        let keep = match expected.get(&path) {
            Some(Some(current_sha)) => current_sha == &recorded_sha,
            Some(None) => true,
            None => false,
        };
        if !keep {
            fs::remove_file(&path).with_context(|| format!("removing stale {}", path.display()))?;
            state.log(
                "INFO",
                &format!("removed stale sidecar: {}", display_output(folder, &path)),
            );
        }
    }
    Ok(())
}

fn reconcile_rollups(
    folder: &Path,
    rollup_root: &Path,
    state: &StateDir,
    expected: &BTreeSet<PathBuf>,
) -> Result<()> {
    for path in generated_markdown_files(rollup_root) {
        if !expected.contains(&path) && is_managed_rollup(&path) {
            fs::remove_file(&path).with_context(|| format!("removing stale {}", path.display()))?;
            state.log(
                "INFO",
                &format!("removed stale rollup: {}", display_output(folder, &path)),
            );
        }
    }
    Ok(())
}

fn generated_markdown_files(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return Vec::new();
    }
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_hidden(e))
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| matches!(classify(path), FileKind::Markdown))
        .collect()
}

fn managed_sidecar_sha(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let frontmatter = markdown_frontmatter(&text)?;
    if !frontmatter
        .lines()
        .any(|line| line.starts_with("generator: transcriptd "))
    {
        return None;
    }
    frontmatter
        .lines()
        .find_map(|line| line.strip_prefix("sha256: ").map(str::to_string))
}

fn is_managed_rollup(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| markdown_frontmatter(&text).map(str::to_string))
        .is_some_and(|frontmatter| {
            frontmatter
                .lines()
                .any(|line| line.starts_with("generator: transcriptd "))
                && frontmatter.lines().any(|line| line == "kind: rollup")
        })
}

fn markdown_frontmatter(markdown: &str) -> Option<&str> {
    let rest = markdown.strip_prefix("---\n")?;
    rest.split_once("\n---\n")
        .map(|(frontmatter, _)| frontmatter)
}

fn display_output(folder: &Path, path: &Path) -> String {
    path.strip_prefix(folder)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
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
/// With rollup_dir set, marker semantics are replaced: every folder rolls up
/// into the rollup tree instead.
struct RollupOutcome {
    written: usize,
    expected: BTreeSet<PathBuf>,
}

fn stitch_rollups(
    folder: &Path,
    cfg: &Config,
    state: &StateDir,
    hashes: &BTreeMap<PathBuf, String>,
) -> Result<RollupOutcome> {
    if let Some(rollup_root) = cfg.rollup_root(folder) {
        return stitch_every_folder(folder, cfg, state, hashes, &rollup_root);
    }
    let marked = find_marked_folders(folder, &cfg.marker);
    if marked.is_empty() {
        return Ok(RollupOutcome {
            written: 0,
            expected: BTreeSet::new(),
        });
    }

    // Each file belongs to its nearest marked ancestor only.
    let mut groups: BTreeMap<&PathBuf, Vec<(&PathBuf, &String)>> = BTreeMap::new();
    for (path, sha) in hashes {
        if let Some(owner) = nearest_marked_ancestor(path, &marked, folder) {
            groups.entry(owner).or_default().push((path, sha));
        }
    }

    let mut written = 0;
    let mut expected = BTreeSet::new();
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
        expected.insert(rollup_path.clone());
        if existing != content {
            write_output(&rollup_path, content.as_bytes())?;
            state.log(
                "INFO",
                &format!("restitched rollup: {folder_rel}/{}", cfg.rollup_name),
            );
            written += 1;
        }
    }
    Ok(RollupOutcome { written, expected })
}

/// rollup_dir mode: every folder containing transcribable files gets a
/// rollup of its DIRECT files (subfolders roll up separately), written at
/// the folder's mirrored path under the rollup tree — and only there. Like
/// marker rollups, this is pure reassembly from cache (R9).
fn stitch_every_folder(
    folder: &Path,
    cfg: &Config,
    state: &StateDir,
    hashes: &BTreeMap<PathBuf, String>,
    rollup_root: &Path,
) -> Result<RollupOutcome> {
    // Group by direct parent; BTreeMap iteration keeps filename order.
    let mut groups: BTreeMap<PathBuf, Vec<(&PathBuf, &String)>> = BTreeMap::new();
    for (path, sha) in hashes {
        if let Some(parent) = path.parent() {
            groups
                .entry(parent.to_path_buf())
                .or_default()
                .push((path, sha));
        }
    }

    let mut written = 0;
    let mut expected = BTreeSet::new();
    for (dir, members) in &groups {
        let mut sections = Vec::new();
        for (path, sha) in members {
            let Some(md) = cached_markdown(state, sha) else {
                continue; // not yet transcribed (failed or deferred)
            };
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            sections.push(format!("## {name}\n\n{}", md.trim()));
        }
        if sections.is_empty() {
            continue;
        }
        let rel = dir.strip_prefix(folder).unwrap_or(dir);
        let folder_rel = if rel.as_os_str().is_empty() {
            ".".to_string()
        } else {
            rel.to_string_lossy().to_string()
        };
        let content = format!(
            "---\ngenerator: transcriptd {}\nkind: rollup\nfolder: {folder_rel}\nsections: {}\n---\n\n{}\n",
            crate::VERSION,
            sections.len(),
            sections.join("\n\n")
        );
        let rollup_path = rollup_root.join(rel).join(&cfg.rollup_name);
        let existing = fs::read_to_string(&rollup_path).unwrap_or_default();
        expected.insert(rollup_path.clone());
        if existing != content {
            write_output(&rollup_path, content.as_bytes())?;
            state.log(
                "INFO",
                &format!("restitched rollup: {folder_rel}/{}", cfg.rollup_name),
            );
            written += 1;
        }
    }
    Ok(RollupOutcome { written, expected })
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
