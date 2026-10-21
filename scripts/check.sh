#!/usr/bin/env bash
# Verification gate: run before every commit. Must pass on a clean tree.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "==> cargo fmt --check"
cargo fmt --all --check

echo "==> cargo clippy"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "==> cargo test"
cargo test --workspace

echo "==> cargo doc"
cargo doc --workspace --no-deps

echo "==> non-ASCII check"
nonascii() {
    if command -v rg >/dev/null 2>&1; then
        rg -nP '[^\x00-\x7F]' --glob '!plans/**' .
    else
        grep -rnP '[^\x00-\x7F]' \
            --exclude-dir=plans --exclude-dir=target \
            --exclude-dir=.git --exclude-dir=.jj .
    fi
}
hits="$(nonascii || true)"
if [ -n "$hits" ]; then
    echo "non-ASCII characters found:" >&2
    echo "$hits" >&2
    exit 1
fi

echo "check.sh: all green"
