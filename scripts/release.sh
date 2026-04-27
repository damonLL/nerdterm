#!/usr/bin/env bash
# Cut a new release of nerdterm.
#
# Usage:
#   scripts/release.sh <version>          # e.g. scripts/release.sh 0.1.1
#   scripts/release.sh --check <version>  # run pre-flight checks only; do not tag
#
# Pre-flight: verifies you're on a clean main, in sync with origin, that the
# Cargo.toml version matches <version>, that fmt/clippy/test/build all pass,
# and that no obvious secrets are present in the tracked tree. Then prompts
# before tagging and pushing the tag.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# ----- arg parsing ----------------------------------------------------------
DRY_RUN=0
if [[ "${1:-}" == "--check" ]]; then
    DRY_RUN=1
    shift
fi
if [[ $# -lt 1 ]]; then
    echo "usage: $0 [--check] <version>   (e.g. 0.1.1)" >&2
    exit 2
fi
VERSION="${1#v}"   # strip optional leading v
TAG="v$VERSION"

# ----- color helpers --------------------------------------------------------
if [[ -t 1 ]]; then
    RED=$'\033[31m' GREEN=$'\033[32m' YELLOW=$'\033[33m' BOLD=$'\033[1m' OFF=$'\033[0m'
else
    RED='' GREEN='' YELLOW='' BOLD='' OFF=''
fi
red()    { printf "%s%s%s\n" "$RED"    "$*" "$OFF"; }
green()  { printf "%s%s%s\n" "$GREEN"  "$*" "$OFF"; }
yellow() { printf "%s%s%s\n" "$YELLOW" "$*" "$OFF"; }
bold()   { printf "%s%s%s\n" "$BOLD"   "$*" "$OFF"; }

bold "==> Pre-flight checks for $TAG"

# ----- git state ------------------------------------------------------------
BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [[ "$BRANCH" != "main" ]]; then
    red "[fail] not on main (currently on $BRANCH)"; exit 1
fi
green "[ok]  on main"

if ! git diff --quiet || ! git diff --cached --quiet; then
    red "[fail] working tree has uncommitted changes:"
    git status --short
    exit 1
fi
green "[ok]  working tree clean"

git fetch origin main --quiet
LOCAL=$(git rev-parse main)
REMOTE=$(git rev-parse origin/main)
if [[ "$LOCAL" != "$REMOTE" ]]; then
    red   "[fail] main is out of sync with origin/main"
    yellow "       local:  $LOCAL"
    yellow "       remote: $REMOTE"
    exit 1
fi
green "[ok]  main matches origin"

if git rev-parse "$TAG" >/dev/null 2>&1; then
    red "[fail] tag $TAG already exists locally"; exit 1
fi
if git ls-remote --tags origin "refs/tags/$TAG" 2>/dev/null | grep -q "$TAG"; then
    red "[fail] tag $TAG already exists on origin"; exit 1
fi
green "[ok]  tag $TAG is unused"

# ----- Cargo.toml version match --------------------------------------------
CARGO_VERSION=$(awk -F'"' '/^version *= *"/ {print $2; exit}' Cargo.toml)
if [[ "$CARGO_VERSION" != "$VERSION" ]]; then
    red    "[fail] Cargo.toml version is \"$CARGO_VERSION\", but requested \"$VERSION\""
    yellow "       Bump Cargo.toml, run 'cargo check' (regenerates Cargo.lock),"
    yellow "       commit, push, then re-run this script."
    exit 1
fi
green "[ok]  Cargo.toml version = $VERSION"

# ----- Cargo.lock in sync ---------------------------------------------------
if ! cargo check --locked --quiet 2>/dev/null; then
    red    "[fail] cargo check --locked failed (Cargo.lock out of sync?)"
    yellow "       Run 'cargo check' to regenerate, commit, then re-run."
    exit 1
fi
green "[ok]  Cargo.lock in sync"

# ----- formatting -----------------------------------------------------------
if cargo fmt --check >/dev/null 2>&1; then
    green "[ok]  cargo fmt clean"
else
    yellow "[warn] cargo fmt --check found unformatted files (not blocking)"
fi

# ----- clippy ---------------------------------------------------------------
bold "==> cargo clippy --locked -- -D warnings"
cargo clippy --locked --all-targets -- -D warnings
green "[ok]  clippy clean"

# ----- tests ----------------------------------------------------------------
bold "==> cargo test --locked"
cargo test --locked
green "[ok]  tests pass"

# ----- release build --------------------------------------------------------
bold "==> cargo build --release --locked"
cargo build --release --locked
green "[ok]  release build succeeded"

# ----- secret scan ----------------------------------------------------------
bold "==> Secret scan over tracked files"
SCAN_FAILURES=0
scan() {
    local label="$1" pattern="$2"
    local hits
    hits=$(git ls-files -z | xargs -0 grep -lE "$pattern" 2>/dev/null || true)
    if [[ -n "$hits" ]]; then
        red "      ✗ $label found in:"
        echo "$hits" | sed 's/^/          /'
        SCAN_FAILURES=$((SCAN_FAILURES + 1))
    fi
}
scan "GitHub PAT (classic ghp_/gho_/ghs_/ghu_/ghr_)" 'gh[opsur]_[A-Za-z0-9]{36,}'
scan "GitHub PAT (fine-grained github_pat_*)"        'github_pat_[A-Za-z0-9_]{20,}'
scan "OpenAI / Anthropic API key shape (sk-*)"       'sk-[A-Za-z0-9_-]{20,}'
scan "AWS access key (AKIA/ASIA*)"                   '(AKIA|ASIA)[0-9A-Z]{16}'
scan "PEM private-key block"                         'BEGIN (RSA|OPENSSH|EC|DSA|PGP) PRIVATE KEY'

if [[ $SCAN_FAILURES -gt 0 ]]; then
    red "[fail] secret scan found $SCAN_FAILURES match(es). Aborting."
    exit 1
fi
green "[ok]  no obvious secrets in tracked tree"

# ----- summary --------------------------------------------------------------
echo ""
bold "All checks passed for $TAG."

if [[ $DRY_RUN -eq 1 ]]; then
    yellow "[--check] mode: not tagging or pushing."
    exit 0
fi

# ----- confirm + tag + push -------------------------------------------------
read -r -p "Tag and push $TAG to origin? [y/N] " confirm
case "$confirm" in
    y|Y) ;;
    *)   red "Aborted."; exit 1 ;;
esac

bold "==> git tag -a $TAG -m \"$TAG\""
git tag -a "$TAG" -m "$TAG"

bold "==> git push origin $TAG"
git push origin "$TAG"

echo ""
green "Done. Release workflow triggered."
echo "  Actions:  https://github.com/damonLL/nerdterm/actions"
echo "  Release:  https://github.com/damonLL/nerdterm/releases/tag/$TAG"
