# Agent Instructions — adele-tui

Shared standards live in [AGENTS.base.md](AGENTS.base.md), which is generated. This file holds the rules specific to this repo.

Repo-specific conventions for the ratatui terminal client. The overrides and additions to the base are listed at the end of this file.

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

Base rule 6.1 and the 6.1 override at the end of this file cover the posture. Repo-specific note: TUI dependencies are pure-Rust and the CVE blast radius is smaller than the GTK/KDE clients, but ratatui's render pipeline still parses untrusted markdown (assistant output), so input-handling crates (markdown parsers, color escape handlers) deserve specific attention on upgrades.

## Overrides and additions to the shared base

Everything in [AGENTS.base.md](AGENTS.base.md) applies to this repo. This section
records only the points where this repo deliberately differs from the base, or adds a
rule the base does not have.

### 3.1 The gate for this repo (addition)

The `adelie-ai` repos have no CI. The gate is local and the author runs it: `just check`.
Run `just install-hooks` once per clone to put the same gate on pre-push. Warnings are
denied mechanically by the `[lints]` table in `Cargo.toml`, so `cargo build`, `cargo test`,
and `cargo clippy` each hard-fail on a warning.

### 4.3 Branch and pull request - merge when green (override, weaker than the base)

The base opens a pull request and waits for the user. In these repos the merge is delegated:
merge your own pull request as soon as it is green and independently shippable. Green here
means more than a clean build. The gate above passed, the tests cover the new behavior and
not only the absence of a panic, the security pass is done, and the change stands on its own.
Assign `dspadea` with `gh pr edit --add-assignee` and verify it; a review request from the
same account no-ops without an error, so never report a pull request as review-requested.
When in doubt, hold.

### 4.4 Worktrees - the group convention (addition)

Put the worktree at `.worktrees/<repo>/issue-N-slug/` under the group directory, on a branch
that mirrors the slug. Before you run tasks in parallel worktrees, look for shared files,
shared `Cargo.toml` dependency edits, and shared migration ordinals. Serialize the work where
they overlap, and tell each parallel agent the scope it owns.

### 6.1 Dependencies - a high or critical advisory is a hard blocker (override, stricter than the base)

Scan after you add a dependency and before the first build:

1. Add the dependency (`cargo add <crate>`). This writes the lockfile but does not build.
2. Scan the updated lockfile with the `cve-mcp` server's `scan_packages` tool, or with
   `cargo audit`. Pass every (name, version, ecosystem) tuple.
3. A high or critical finding blocks the change. Patch it in the same change, or prove the
   path unreachable and write down why, or file an issue and reference it from the change.
4. Build only after the scan is clean, or after you have accepted the findings in writing.

Never pin around an advisory without a comment or a tracked issue.

### 9.1 Tracker for this project

GitHub Issues on `github.com/adelie-ai/adele-tui`, together with the shared `adelie-ai` project
board. Manage entries with the `gh` CLI (`gh issue create`, `gh issue list`, `gh issue edit`,
`gh pr create`). The board states in use are In Progress, In Review, and Done.
