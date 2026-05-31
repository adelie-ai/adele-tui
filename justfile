set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# --- Local verification ("local CI") -----------------------------------------
# We run these locally instead of GitHub Actions. `install-hooks` wires `check`
# into a git pre-push hook so it runs automatically before every push.

# Full local gate: formatting, lints, build, tests (on the pinned toolchain)
check: fmt-check lint build test

# Verify formatting without modifying files
fmt-check:
    cargo fmt --check

# Apply formatting
fmt:
    cargo fmt

# Clippy; warnings are errors
lint:
    cargo clippy --all-targets -- -D warnings

# Build
build:
    cargo build

# Run the test suite (excludes #[ignore] integration tests)
test:
    cargo test

# Real-Secret-Service integration tests (needs a live session bus; mutates + cleans keyring)
test-integration:
    cargo test -- --ignored

# Rebase onto latest origin/main then run the gate (catches clean-rebase-but-broken-build)
premerge:
    git fetch origin
    git rebase origin/main
    just check

# Install git hooks (pre-push runs `just check`). Local config; run once per clone.
install-hooks:
    git config core.hooksPath .githooks
    @echo "pre-push hook active — bypass once with: git push --no-verify"
