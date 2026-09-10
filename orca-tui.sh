#!/usr/bin/env bash
# Convenient launcher for a source checkout.
# Let Cargo validate source freshness; it reuses an up-to-date build and
# rebuilds automatically after source changes.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

if command -v cargo >/dev/null 2>&1; then
  exec cargo run --locked \
    --manifest-path "$SCRIPT_DIR/Cargo.toml" \
    --bin orca-tui -- "$@"
fi

if [[ -x "$SCRIPT_DIR/target/release/orca-tui" ]]; then
  exec "$SCRIPT_DIR/target/release/orca-tui" "$@"
fi

if [[ -x "$SCRIPT_DIR/target/debug/orca-tui" ]]; then
  exec "$SCRIPT_DIR/target/debug/orca-tui" "$@"
fi

echo "orca-tui: 未找到 Cargo 或可用的已构建二进制" >&2
exit 127
