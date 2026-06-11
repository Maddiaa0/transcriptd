# transcriptd

A folder transcription daemon. It monitors one folder for new and changed
files and transcribes them into markdown via [OpenRouter](https://openrouter.ai),
so anything dropped into the folder — handwritten note pages, PDFs, office
documents — becomes plain text that `rg`, agents, and knowledge-base tools can
consume.

Requirements doc: `docs/brainstorms/2026-06-11-folder-transcription-daemon-requirements.md`.

## How it works

The core is one idempotent command:

```sh
export OPENROUTER_API_KEY=sk-or-...
transcriptd --folder /srv/notes scan
```

Each sweep walks the folder, hashes every file (sha256), and diffs against a
ledger. Content already transcribed is never re-sent to the API — regardless
of mtime churn from sync tools or file renames. New or changed content is
transcribed once and gets a markdown **sidecar** next to it
(`page.png` → `page.png.md`) with provenance frontmatter:

```markdown
---
source: page.png
sha256: 4f2a…
model: google/gemini-2.5-flash
prompt_version: 1
transcribed_at: 2026-06-11T10:00:00Z
generator: transcriptd 0.1.0
---

# Meeting notes …
```

A folder whose `index.md` contains the configured marker
(`<!-- transcriptd:document -->` by default) is treated as a **document**:
transcriptd maintains a stitched `transcript.md` at its root, one section per
file in filename order. Rollups are reassembled from cache — restitching
never costs an API call.

All state lives in `.transcriptd/` **inside the watched folder**, so a
folder-scoped grant (sync share, agent workspace) covers the corpus and
everything derived from it:

```
.transcriptd/
  config.toml       # settings (see `transcriptd init`)
  ledger.json       # sha256 -> transcription record
  failures.json     # files awaiting retry, with attempt counts
  cache/<sha>.md    # cached transcripts (rollup source)
  raw/<sha>.json    # full API responses, retained for future re-processing
  transcriptd.log
```

Failure behavior is deliberately loud: a failed transcription never produces
a sidecar, the failure is recorded with its error, and every subsequent sweep
retries it. `transcriptd status` shows what's pending.

Files modified within the last `stability_seconds` (default 10) are assumed
mid-sync and deferred to the next sweep.

## Supported inputs

| Type | Extensions | Sent as |
|---|---|---|
| Images | png, jpg, jpeg, webp | base64 image to the vision model |
| PDF | pdf | native file upload (engine configurable) |
| Word | docx | extracted text, reformatted by the model |
| Text | txt, csv, tsv, rst, org, html | raw text, reformatted by the model |

Markdown files are never transcribed (they're already the output format).
Anything else is logged as skipped, once, and the sweep continues.

## Commands

```sh
transcriptd --folder /srv/notes init          # write a commented config template
transcriptd --folder /srv/notes scan          # one sweep, exits non-zero if any file failed
transcriptd --folder /srv/notes scan --watch  # keep running, rescan on fs events + timer
transcriptd --folder /srv/notes status        # ledger + pending-failure summary
```

## Configuration

`transcriptd init` writes `.transcriptd/config.toml`. Everything has a
default; the file is optional. Swapping the transcription model is a config
change only:

```toml
model = "google/gemini-2.5-flash"   # any vision-capable OpenRouter model
api_key_env = "OPENROUTER_API_KEY"
marker = "<!-- transcriptd:document -->"
rollup_name = "transcript.md"
stability_seconds = 10
pdf_engine = "native"               # native | pdf-text | mistral-ocr
prompt_version = "1"                # bump when overriding `prompt`
```

## Deployment

`dist/` has systemd units for the two run modes:

- **Timer (recommended):** `transcriptd.service` + `transcriptd.timer` run a
  sweep every 5 minutes. Crash-proof — a failed run just means the next one
  picks up the delta.
- **Watch:** `transcriptd-watch.service` runs `scan --watch` for near-instant
  pickup.

Build a release binary with `cargo build --release`
(`target/release/transcriptd`).

## Development

```sh
cargo test    # acceptance examples AE1-AE6 run against a mock transcriber
cargo clippy
```
