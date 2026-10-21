#!/usr/bin/env bash
# Run the same checks as CI (.github/workflows/ci.yml) before pushing:
# formatting, clippy with warnings as errors, and the unit tests.
# Pass --fix to let rustfmt and clippy apply their fixes first.
# On Windows only the client builds, so clippy and the tests cover it alone.
set -euo pipefail
cd "$(dirname "$0")/.."

packages=(--workspace)
case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) packages=(-p gliff) ;;
esac

if [ "${1:-}" = "--fix" ]; then
    cargo fmt --all
    cargo clippy --fix "${packages[@]}" --all-targets --allow-dirty --allow-staged --locked
fi

echo "== fmt"
cargo fmt --all --check
echo "== clippy"
cargo clippy "${packages[@]}" --all-targets --locked -- -D warnings
echo "== test"
cargo test "${packages[@]}" --locked
echo "CHECK PASS"
