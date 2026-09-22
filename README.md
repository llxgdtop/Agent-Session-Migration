# Agent Session Hub

**Your AI coding sessions, freed from tool lock-in.**

[English](README.md) | [简体中文](README.zh-CN.md)

<!-- TODO: screenshot. Capture the main window and save as docs/screenshot.png, then uncomment:
![Agent Session Hub main window](docs/screenshot.png)
-->

Start a task in one AI coding tool, finish it in another. Agent Session Hub moves coding-agent sessions between Claude Code, Codex, and ZCode in any direction -- messages, reasoning, and tool-call records included -- so the next tool picks up exactly where the last one left off. No re-explaining the context from scratch.

## What it does

- **Session overview** -- Scans all Claude Code, Codex, and ZCode sessions on your machine into a single list, sorted by recent activity, with source badges plus per-source filters and search. Click any session to preview the full message stream.
- **One-click migration** -- Convert any session to any of the three tools (all six directions are supported). Migration only creates new files or new database rows; your existing sessions are never touched.
- **Seamless continuation** -- After migrating to Claude Code or Codex, copy the resume command the app hands you (or open it in Terminal with one click) and keep working with full context. Migrating to ZCode opens the ZCode desktop app, where the session appears in your task list.

## Quick start

Requirements: macOS 11 or later and a recent Rust toolchain. Windows and Linux builds are on the roadmap.

```sh
cargo build --release
./scripts/build-app.sh          # produces dist/AgentSessionHub.app
open dist/AgentSessionHub.app   # or double-click the app in Finder
```

To migrate a session:

1. Pick a session in the list and review the preview.
2. Click the migrate button for the target tool. If the target tool is currently running, the app warns you first and only writes after you confirm.
3. Copy the resume command into your terminal -- or let the app open it for you -- and continue the conversation.

## How it works

Each tool stores sessions in its own format: JSONL files under `~/.claude/projects` and `~/.codex/sessions`, and a pair of SQLite databases under `~/.zcode/`. Agent Session Hub normalizes them through a three-stage pipeline:

```
reader (parse source format) --> unified IR --> writer (emit target format)
```

- `crates/hub-core` -- the core library: per-tool readers, the unified intermediate representation, per-tool writers, plus the resume-command launcher and the pre-write safety checks.
- `crates/hub-app` -- the native desktop UI (built with egui/eframe).

Everything runs on your machine. The app makes no network requests.

## Privacy & safety

- **Local only.** No network access, no telemetry, nothing uploaded.
- **Additive only.** Migration creates new files or new rows. Existing sessions are never modified or deleted.
- **Atomic writes.** File targets are written to a temporary file and renamed into place; ZCode targets are written inside a single SQLite transaction per database. A failure never leaves a half-written session behind.
- **Guarded ZCode writes.** Before touching ZCode's databases, the app verifies the schema version (and refuses to write if it does not recognize it) and makes timestamped backups; if a backup fails, the migration aborts.
- **Running-tool detection.** Writing while the target tool is running risks conflicts or corruption, so the app checks for running processes and asks you to confirm before proceeding.

## Roadmap

- Dark mode
- Drag-and-drop migration
- Windows and Linux builds
- Redaction of sensitive data before migration
- Cross-machine session transfer

## Contributing

Issues and pull requests are welcome. To work on the app locally:

```sh
cargo test                  # run all tests
cargo run -p hub-app        # run the app in dev mode
cargo clippy --all-targets  # lint
```

Conversion logic lives in `crates/hub-core`; the UI lives in `crates/hub-app`.

## License

TBD. A license has not been chosen yet.
