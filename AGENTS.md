# Agent Instructions — adele-tui

Repo-specific conventions for the ratatui terminal client. Cross-project workflow rules (issue/PR/board sync, parallel worktrees, warnings-are-failures, security review posture, TDD posture) are embedded below under **Cross-project engineering standards**.

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

## Cross-project engineering standards

These apply to every repo under `github.com/adelie-ai`. They're embedded in each repo's `AGENTS.md` (not centralized) so a contributor working in a single repo has them in hand. Operator-specific preferences and machine-specific deploy recipes are intentionally not here.

### Don't break `main`
- `main` is the release: at any commit it must build, test, and run.
- Merge a green change as soon as it's independently shippable — additive, behavior-preserving, or behind a default that preserves the old path. Don't hold green work hostage to a coordinated release.
- Co-dependent changes land together; name the interlock ("blocked-by #X" / "must merge with #Y") so it's visible without reading the diff.
- "Green" is more than CI: review passed, tests cover the new behavior (not just "no panic"), warnings clean, security pass done, change stands on its own. With no active CI in these repos, "green" rests on local `cargo test` + `fmt` + `clippy --all-targets` + `cargo audit`, run by the author (via `just check` where the repo provides it).
- When in doubt, hold. A half-coupled "fix-forward" merge breaks `main` for everyone.

### Tests are spec-driven (TDD)
- Every change carries a Testing section: acceptance criteria as testable assertions, each criterion a named test whose name is legible from test output.
- Write failing tests first, in their own commit before the implementation commit — that commit is the spec.
- Cover all new code: every branch, error path, edge case. Gaps are a review finding.
- Assert the desired outcome, not just that a call returned `Ok`.
- Enumerate unhappy paths deliberately: empty/missing input, boundary/max, concurrent/racy, authorization/tenant boundaries, partial reads/writes/dropped streams, malformed input. A test list with none of these is testing wishes.

### Warnings are failures
- Compiler warnings, clippy lints, formatter diffs, and advisories all count — fix the root cause. If a lint truly doesn't apply, suppress at the narrowest scope with a one-line justification; never crate-wide.
- This repo enforces it **mechanically** via a `[lints]` table denying `rust.warnings` and `clippy.all`, so `cargo build`/`test`/`clippy` hard-fail on a warning — it isn't left to reviewer attention.
- Never `--no-verify` past hooks. If a hook is genuinely broken, fix it in its own commit and explain why.
- Don't `#[ignore]` a test you broke; fix it, or open a tracking issue and reference it from the attribute.
- Pre-existing warnings in a file you touch are yours to address (in-change or a small follow-up) — don't pile new code on an ignored signal.

### Security review before requesting review
- Read your own diff adversarially: untrusted input crossing trust boundaries (network, IPC, D-Bus, MCP tool args), secrets in logs, missing auth checks, panic-on-input, unparameterized SQL/shell.
- Scan dependencies whenever the lockfile changed (`cargo audit` or the `cve-mcp` server) — and scan BEFORE the first build, because build scripts execute attacker-controlled code at build time.
- High/critical CVEs are hard blockers: patch in the same change, prove the path unreachable and document why, or file a tracked follow-up referenced in the change. Never ship past one silently; never pin around an advisory without a comment or tracking issue.

### Maintainability / cognitive load
- Keep each change small enough to land independently with a clear deliverable.
- Don't introduce a new abstraction until ~3 call sites prove the pattern; when one new type unifies several needs, justify the unification explicitly.
- Reuse existing traits and patterns rather than inventing parallel ones; extend an existing crate over adding one unless the seam is obvious.


### Capability-based degradation
- Every reliance on an optional OS/desktop service (logind, screen-lock, KDE/Plasma, PipeWire specifics, any session- or system-bus D-Bus interface) must be capability-detected and degrade gracefully — never a hard dependency that errors or hangs when absent. The product may run headless, in containers, on other DEs, or as a system service.
- Distinguish "is the capability present?" from "did my call succeed?" Three states: absent → disable that feature, log once, fall back to prior behavior; present-and-known → use it; present-but-anomalous → stay conservative / last-known-state and warn. Scope any privacy/safety fail-safe to the last two — a fail-safe correct on the desktop can be pathological headless (e.g. "treat unknown session as inactive" ⇒ mic never opens).
- Detect each optional dependency independently; absence of one never disables the others or aborts startup. Surface the detected capability so an operator sees *why* a feature is on or off.

### GitHub issue / PR / board hygiene
- Self-assign an issue when you start it (or comment to claim it) so parallel work doesn't collide; move the board card to In Progress.
- Link the PR to the issue: `Closes #N` to auto-close, `Refs #N` when it only partially addresses it.
- Keep the board in sync with reality (In Review on open, Done on merge); if you can't move the card, comment the intended status.
- On multi-session work, leave a short status comment before stopping — what landed, what's next, what's blocked — so state is reconstructable without git log.

### Worktrees
- Do code work in a git worktree on its own branch off `origin/main`, never the primary checkout, so concurrent sessions don't collide. Convention: `~/Projects/adelie-ai/.worktrees/<repo>/issue-N-slug/`, branch mirroring the slug.
- Run independent tasks in parallel worktrees, but check first for shared files / shared `Cargo.toml` dep edits / shared migration ordinals — if they overlap, serialize. Brief each parallel agent on its scope ("own crate X, don't touch Y").
