#!/usr/bin/env bash
# Convenient launcher for a source checkout.
# Prefer an existing optimized build, then a debug build, and finally Cargo.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

if [[ -x "$SCRIPT_DIR/target/release/orca-tui" ]]; then
  exec "$SCRIPT_DIR/target/release/orca-tui" "$@"
fi

if [[ -x "$SCRIPT_DIR/target/debug/orca-tui" ]]; then
  exec "$SCRIPT_DIR/target/debug/orca-tui" "$@"
fi

if command -v cargo >/dev/null 2>&1; then
  exec cargo run --manifest-path "$SCRIPT_DIR/Cargo.toml" --bin orca-tui -- "$@"
fi

echo "orca-tui: 未找到已构建的二进制，且 cargo 不在 PATH 中" >&2
exit 127
