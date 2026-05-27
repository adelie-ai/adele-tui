# Agent Instructions — adele-tui

Repo-specific conventions for the ratatui terminal client. Cross-project workflow rules (issue/PR/board sync, parallel worktrees, warnings-are-failures, security review posture, TDD posture) live in the user's memory and are not duplicated here.

## What this repo is

`ratatui`-based TUI that talks to `desktop-assistant-daemon` over WebSocket or D-Bus. Shared protocol types come from `adelie-ai/desktop-assistant`'s `api-model` and `client-common` crates pulled in as git dependencies. `Cargo.lock` pins the exact revision.

## Where things live

- `src/main.rs` — entry, CLI parsing, transport selection.
- `src/app.rs` — top-level event loop and state machine. New screens hook into this.
- `src/ui.rs` — top-level layout / draw dispatch.
- `src/widgets`-style modules at `src/` root — `connections.rs`, `kb.rs`, `model_selector.rs`, `picker.rs`, `profile.rs`, `purposes.rs`, `settings.rs`, `toolbar.rs`. One file per screen / panel.
- `src/markdown.rs` — terminal-flavored rendering of assistant messages.
- `src/credentials.rs`, `src/keys.rs`, `src/oauth.rs` — auth handling. Same posture as the rest of the platform: secrets never logged, `Display` is fingerprint-only.

## TUI conventions

- **Event-loop separation.** Input events, transport events, and tick events arrive on separate channels and are merged in `app.rs`. New asynchronous sources get their own channel and merge in — don't poll inside the draw loop.
- **Stateless draw, stateful update.** The `draw` path should be a function of current state; mutation happens in the update path. If a widget needs to mutate during draw to "remember" something, factor that into state.
- **Don't fight ratatui's layout.** Use `Layout::default()` / `Constraint::*` rather than hand-computing rectangles. Hand-computed geometry breaks on resize.
- **Pickers / modals are full widgets.** When a piece of UI grows beyond ~50 lines, give it its own module under `src/` and route into it from `app.rs`. The existing screen modules are the shape to match.

## ratatui version drift

ratatui's API has historically broken between minor versions (most recently 0.30 + `ratatui-textarea`). When the upstream version bumps, the entire draw path may need migration in lockstep. Treat the upgrade as its own PR with no other behavior changes, so the migration is reviewable independently.

## Shared types & version pinning

`api-model` and `client-common` come from the desktop-assistant repo via git dep. When the daemon's protocol changes, the version bump here is a deliberate update — coordinate the bump across TUI / GTK / KDE so the three clients track the protocol together. Mention the corresponding daemon PR in the commit message when bumping.

## Rust conventions

The desktop-assistant `AGENTS.md` is the canonical Rust style reference for the platform — error handling, async/locking, generics, unsafe, doc comments. This crate follows it.

## Build & install

- `cargo build`, `cargo test`.
- `cargo install --path .` installs `adele` to `~/.cargo/bin/`.

CLI flags and env vars are documented in `README.md`.

## Dependency safety

The user-memory security-review rule covers the posture. Repo-specific note: TUI dependencies are pure-Rust and the CVE blast radius is smaller than the GTK/KDE clients, but ratatui's render pipeline still parses untrusted markdown (assistant output), so input-handling crates (markdown parsers, color escape handlers) deserve specific attention on upgrades.
