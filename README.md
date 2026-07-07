# transcriptd

A folder transcription daemon. It monitors one folder for new and changed
files and transcribes them into markdown via [OpenRouter](https://openrouter.ai)
or a local agent CLI (OpenAI Codex, hermes, …), so anything dropped into the
folder — handwritten note pages, PDFs, office documents — becomes plain text
that `rg`, agents, and knowledge-base tools can consume.

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
transcriptd init --global                     # global config: API key + default model
transcriptd --folder /srv/notes init          # per-folder config template
transcriptd --folder /srv/notes scan          # one sweep, exits non-zero if any file failed
transcriptd --folder /srv/notes scan --watch  # keep running, rescan on fs events + timer
transcriptd --folder /srv/notes status        # ledger + pending-failure summary
```

## Configuration

Config is layered; later layers override earlier ones key by key, and every
file is optional (everything has a default):

1. **Global:** `$XDG_CONFIG_HOME/transcriptd/config.toml`
   (`~/.config/transcriptd/config.toml`) — `transcriptd init --global`.
   Machine-wide settings: default model and the OpenRouter API key. Written
   with mode 600. This directory is also where future machine-local
   application data (e.g. an index database) will live.
2. **Per-folder:** `<folder>/.transcriptd/config.toml` — `transcriptd init`.
   Folder-specific overrides (marker, stability window, a different model).
   Don't put the API key here: this file syncs with the corpus.
3. **Explicit:** `--config <path>` overrides both.

The API key itself resolves as: `$OPENROUTER_API_KEY` (or whatever
`api_key_env` names) if set, else `api_key` from the merged config.

```toml
backend = "openrouter"              # or "cli" — see below
model = "google/gemini-2.5-flash"   # any vision-capable OpenRouter model
# api_key = "sk-or-..."             # global config only
api_key_env = "OPENROUTER_API_KEY"
# output_dir = "transcripts"        # default: sidecars land next to sources
marker = "<!-- transcriptd:document -->"
rollup_name = "transcript.md"
stability_seconds = 10
pdf_engine = "native"               # native | pdf-text | mistral-ocr
prompt_version = "1"                # bump when overriding `prompt`
```

### Choosing the output folder

By default every transcript lands next to its source file
(`page.png` → `page.png.md`) and rollups at each marked folder's root.
Setting `output_dir` redirects all generated markdown — sidecars and
rollups — into a separate tree that mirrors the folder's structure, keeping
the source folder clean:

```toml
# in .transcriptd/config.toml (or the global config)
output_dir = "transcripts"        # <folder>/transcripts/...
# output_dir = "/srv/transcripts" # absolute paths work too
```

```
notes/                            notes/transcripts/
  notebook/01.png          →        notebook/01.png.md
  notebook/index.md                 notebook/transcript.md   (rollup)
  loose.jpg                         loose.jpg.md
```

Relative paths resolve against the watched folder, so a per-folder config
that syncs with the corpus behaves the same on every machine. An output tree
inside the watched folder is never scanned as a source. Changing
`output_dir` later is cheap: the next scan rebuilds every sidecar at the new
location from cache, with no API calls (old files are not deleted).

## Backends

### OpenRouter (default)

`backend = "openrouter"` sends files to the OpenRouter chat-completions API
and needs an API key (see above).

### Local agent CLI (`backend = "cli"`)

`backend = "cli"` shells out to any agent CLI instead, so transcription can
ride a subscription you already pay for (e.g. ChatGPT via the Codex CLI)
rather than metered API credit. No API key is needed; auth is whatever the
CLI itself uses.

Commands are argv templates. Three placeholders are substituted inside each
argument:

- `{file}` — path to a temp copy of the document (image/PDF bytes; docx and
  text files arrive as extracted plain text with a `.txt` extension)
- `{prompt}` — the transcription prompt
- `{output}` — a temp path the command should write the transcript to. If the
  template has no `{output}`, stdout is the transcript instead (only suitable
  for CLIs with clean stdout).

`command` is the fallback for every file kind; `image_command`,
`pdf_command`, and `text_command` override it per kind. Example for the
OpenAI Codex CLI (run `codex login` once first) in
`~/.config/transcriptd/config.toml`:

```toml
backend = "cli"
model = "codex"   # provenance label recorded in sidecar frontmatter

[cli]
# Codex attaches images natively with -i; for PDFs/text the file path is
# appended to the prompt and the agent reads it itself.
command = ["codex", "exec", "--skip-git-repo-check", "--output-last-message", "{output}", "{prompt}\n\nThe document to transcribe is the file at: {file}"]
image_command = ["codex", "exec", "--skip-git-repo-check", "--output-last-message", "{output}", "-i", "{file}", "{prompt}"]
```

Any other CLI (hermes, claude, …) plugs in the same way — one argv template
that receives the file and prompt and emits markdown. A non-zero exit,
empty output, or a run longer than `request_timeout_seconds` is recorded as
a failure and retried next sweep, same as an API error. Note that layering
replaces the `[cli]` table wholesale rather than key by key, and `pdf_engine`
has no effect with this backend.

#### Headless / remote machines

transcriptd does no auth of its own for this backend — the CLI's normal
login must exist **on the machine that runs the scans**, in the home of the
**user the service runs as**. For Codex on a box with no browser, either:

- **Log in through an SSH tunnel:** `ssh -L 1455:localhost:1455 user@remote`,
  run `codex login` on the remote, and open the printed URL in your local
  browser. Each machine then refreshes its own tokens independently
  (recommended).
- **Copy the credentials:** `scp ~/.codex/auth.json remote:~/.codex/` from a
  logged-in machine (mode 600, owned by the service user). Codex rotates
  tokens on refresh, so a copy shared across machines can eventually
  invalidate itself; if scans start failing with auth errors in
  `transcriptd status`, re-copy or switch to a real login.

The CLI must be able to *write* its state dir (e.g. `~/.codex`) at runtime
for token refresh — under the hardened systemd units in `dist/` that means
adding it to `ReadWritePaths` (see the comments in the unit files). Systemd
also runs with a minimal `PATH`, so use an absolute path to the CLI in the
`[cli]` command templates (npm- or nvm-installed binaries won't be found
otherwise).

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
