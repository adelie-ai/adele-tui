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

Base rule 6.1 covers the posture. Repo-specific note: TUI dependencies are pure-Rust and the CVE blast radius is smaller than the GTK/KDE clients, but ratatui's render pipeline still parses untrusted markdown (assistant output), so input-handling crates (markdown parsers, color escape handlers) deserve specific attention on upgrades.

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

### 6.1 Dependencies - the group's scan workflow (addition)

Base rule 6.1 sets the policy, including that a high or critical advisory blocks the change.
This group runs it with its own tooling:

1. Add the dependency (`cargo add <crate>`). This writes the lockfile but does not build.
2. Scan the updated lockfile with the `cve-mcp` server's `scan_packages` tool, or with
   `cargo audit`. Pass every (name, version, ecosystem) tuple.
3. Build only after the scan is clean, or after you have accepted the findings in writing.

### 9.1 Tracker for this project

GitHub Issues on `github.com/adelie-ai/adele-tui`, together with the shared `adelie-ai` project
board `Adelie AI Roadmap` (project number 1). Manage entries with the `gh` CLI
(`gh issue create`, `gh issue list`, `gh issue edit`, `gh pr create`). Put a new issue on the
board with `gh project item-add 1 --owner adelie-ai --url <issue-url>`, which lands it in
Todo. The board states are Todo, In Progress, and Done.

### Platform, not a single product (addition)

Adele is a platform, not one product. Solve for the general case at every seam that is
plural by domain: storage backends, LLM providers, transports, clients, MCP servers, speech
engines. When a requirement names two of something, ask whether the real requirement is N
of them, and build that one instead.

Put the abstraction at the port. Keep the conditional compilation and the selection in one
factory, so a new implementation costs a crate, a feature, and one arm - not an edit to
every implementation that already exists. A hand-rolled `AnyX` enum with a variant per
implementation is the shape that fails this test: it re-dispatches every trait method by
hand and grows with the set.

Base rule 7.3 still holds inside a component. Do not invent indirection that a single call
site does not need. It does not licence the narrow build at a platform seam, because there
the plurality is the product, and the seam is already past the three-call-site test.

Fail loudly and by name when a configured selection is not compiled in, or is unavailable.
Name what was asked for and what is actually present. Silent degradation to a lesser
backend hides the problem from the one person who could fix it.
