# justfile for pty-mcp
# Run `just` to see all available commands.

set shell := ["bash", "-euo", "pipefail", "-c"]

# Default — list recipes.
default:
    @just --list --unsorted

# ─────────────────────────── Build & Test ───────────────────────────

# Build the release binary into ./bin/.
build:
    mkdir -p bin
    cargo build --release
    cp target/release/pty-mcp bin/pty-mcp
    @echo "Built ./bin/pty-mcp"

# Install into ~/.cargo/bin.
install:
    cargo install --path . --force
    @echo "Installed pty-mcp to $(cargo env CARGO_HOME 2>/dev/null || echo ~/.cargo)/bin"

fmt:
    cargo fmt

# Auto-fix formatting, then the full clippy gate (warnings = errors).
lint: fmt
    cargo clippy --all-targets -- -D warnings

# Strict read-only check — same logic CI runs, for local pre-push.
# Fails if formatting would change or clippy fires.
lint-check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

test:
    cargo test

check: lint test sync-flake

clean:
    rm -rf bin/ target/

# ─────────────────────────── Nix ───────────────────────────

nix-build:
    nix build .#pty-mcp

nix-check:
    nix flake check --print-build-logs

# Keep flake.nix's `cargoHash` aligned with the current Cargo.lock.
#
# A sha256 of Cargo.lock is embedded as a `# cargo-lock:` line in
# flake.nix. When the cached digest matches Cargo.lock on disk,
# sync-flake returns immediately without running `nix build`. That
# makes it cheap enough to run on every `just check`.
#
# By default this does NOT touch the version string — release-only
# concern. Pass an explicit `version` argument to also rewrite
# `version = "…"` (used by the release recipes). Pass `--force` to
# bypass the cache and re-run the nix build even if Cargo.lock is
# unchanged.
sync-flake version="":
    #!/usr/bin/env bash
    set -euo pipefail
    ARG="{{version}}"
    FORCE=0
    VERSION=""
    case "$ARG" in
        "")          ;;
        "--force")   FORCE=1 ;;
        *)           VERSION="${ARG#v}" ;;
    esac

    LOCK_HASH=$(sha256sum Cargo.lock | awk '{print $1}')
    CACHED_HASH=$(awk -F': ' '/^[[:space:]]*#[[:space:]]*cargo-lock:/ {print $2; exit}' flake.nix | tr -d ' ')
    CURRENT_VERSION=$(awk -F'"' '/^[[:space:]]*version = "/ {print $2; exit}' flake.nix)

    NEED_HASH=0
    NEED_VERSION=0
    if [ "$FORCE" = "1" ] || [ "$LOCK_HASH" != "$CACHED_HASH" ]; then NEED_HASH=1; fi
    if [ -n "$VERSION" ] && [ "$VERSION" != "$CURRENT_VERSION" ]; then NEED_VERSION=1; fi

    if [ "$NEED_HASH" = "0" ] && [ "$NEED_VERSION" = "0" ]; then
        echo "sync-flake: up-to-date (cargo-lock=$LOCK_HASH version=$CURRENT_VERSION)"
        exit 0
    fi

    echo "sync-flake: refreshing (need_hash=$NEED_HASH need_version=$NEED_VERSION)"

    # Version must be bumped BEFORE computing the hash: cargoHash covers
    # the vendored deps only, but the version rewrite also touches
    # Cargo.toml/Cargo.lock, so do it first for a consistent build.
    if [ "$NEED_VERSION" = "1" ]; then
        sed -i -E 's|^(version = )"[^"]*"|\1"'"$VERSION"'"|' Cargo.toml
        cargo update -p pty-mcp --precise "$VERSION" 2>/dev/null || cargo generate-lockfile
        sed -i -E 's|^(\s*version = )"[^"]*";|\1"'"$VERSION"'";|' flake.nix
        LOCK_HASH=$(sha256sum Cargo.lock | awk '{print $1}')
        echo "sync-flake: version=$VERSION"
    fi

    SENTINEL="sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
    sed -i -E 's|^(\s*cargoHash = )"sha256-[^"]*";|\1"'"$SENTINEL"'";|' flake.nix
    set +e
    OUT=$(nix build .#pty-mcp --no-link 2>&1)
    BUILD_STATUS=$?
    set -e
    NEW_HASH=$(printf '%s\n' "$OUT" | awk '/got:[[:space:]]*sha256-/ {print $2; exit}')
    if [ -z "$NEW_HASH" ]; then
        if [ "$BUILD_STATUS" = "0" ]; then
            echo "sync-flake: unexpected nix build success with sentinel hash" >&2
            echo "$OUT" >&2
            exit 1
        fi
        echo "$OUT" >&2
        echo "sync-flake: nix build failed without printing 'got: sha256-…'" >&2
        exit 1
    fi
    sed -i -E 's|^(\s*cargoHash = )"sha256-[^"]*";|\1"'"$NEW_HASH"'";|' flake.nix
    sed -i -E 's|^(\s*# cargo-lock:).*|\1 '"$LOCK_HASH"'|' flake.nix
    echo "sync-flake: cargoHash=$NEW_HASH cargo-lock=$LOCK_HASH"

    # Hard guard: refuse to leave the sentinel in flake.nix.
    if grep -q 'cargoHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="' flake.nix; then
        echo "sync-flake: refusing to leave sentinel cargoHash in flake.nix" >&2
        exit 1
    fi

    nix build .#pty-mcp --no-link

# ─────────────────────────── Release ───────────────────────────

release-preview:
    #!/usr/bin/env bash
    set -euo pipefail
    CURRENT_TAG=$(git tag -l 'v*.*.*' --sort=-v:refname | head -1)
    CURRENT_TAG=${CURRENT_TAG:-v0.0.0}
    CURRENT_VERSION=${CURRENT_TAG#v}
    MAJOR=$(echo "$CURRENT_VERSION" | cut -d. -f1)
    MINOR=$(echo "$CURRENT_VERSION" | cut -d. -f2)
    PATCH=$(echo "$CURRENT_VERSION" | cut -d. -f3)
    echo "Current tag: $CURRENT_TAG"
    echo "  release-major: v$((MAJOR + 1)).0.0"
    echo "  release-minor: v${MAJOR}.$((MINOR + 1)).0"
    echo "  release-patch: v${MAJOR}.${MINOR}.$((PATCH + 1))"

_release-checks:
    #!/usr/bin/env bash
    set -euo pipefail
    BRANCH=$(git rev-parse --abbrev-ref HEAD)
    DEFAULT_BRANCH=$(git rev-parse --abbrev-ref origin/HEAD 2>/dev/null | sed 's|^origin/||' || true)
    if [ -z "${DEFAULT_BRANCH:-}" ]; then
        DEFAULT_BRANCH=$(git remote show origin 2>/dev/null | awk '/HEAD branch/ {print $NF}' || echo main)
    fi
    if [ "$BRANCH" != "$DEFAULT_BRANCH" ]; then
        echo "Error: not on default branch '$DEFAULT_BRANCH' (currently '$BRANCH')." >&2
        exit 1
    fi
    just check
    if [ -n "$(git status --porcelain)" ]; then
        echo "Formatting/lint produced changes — staging + committing."
        git add -A
        git commit -m "chore: format code for release"
    fi

_release bump:
    #!/usr/bin/env bash
    set -euo pipefail
    just _release-checks
    CURRENT_TAG=$(git tag -l 'v*.*.*' --sort=-v:refname | head -1)
    CURRENT_TAG=${CURRENT_TAG:-v0.0.0}
    CURRENT_VERSION=${CURRENT_TAG#v}
    MAJOR=$(echo "$CURRENT_VERSION" | cut -d. -f1)
    MINOR=$(echo "$CURRENT_VERSION" | cut -d. -f2)
    PATCH=$(echo "$CURRENT_VERSION" | cut -d. -f3)
    case "{{bump}}" in
        major) NEW="$((MAJOR + 1)).0.0" ;;
        minor) NEW="${MAJOR}.$((MINOR + 1)).0" ;;
        patch) NEW="${MAJOR}.${MINOR}.$((PATCH + 1))" ;;
        *) echo "unknown bump kind: {{bump}}"; exit 1 ;;
    esac
    # Bump Cargo.toml + Cargo.lock + flake.nix version and cargoHash,
    # re-validating the build, BEFORE tagging.
    just sync-flake "${NEW}"
    if [ -n "$(git status --porcelain Cargo.toml Cargo.lock flake.nix)" ]; then
        git add Cargo.toml Cargo.lock flake.nix
        git commit -m "chore: bump to v${NEW}"
    fi
    git tag -a "v${NEW}" -m "v${NEW}"
    git push origin HEAD
    git push origin "v${NEW}"
    echo
    echo "Tagged v${NEW}."
    echo "Watch the release build: gh run watch || open https://github.com/stubbedev/pty-mcp/actions"

release-patch: (_release "patch")
release-minor: (_release "minor")
release-major: (_release "major")
