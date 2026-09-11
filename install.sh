#!/usr/bin/env bash
# Install the orca-tui command from this checkout.
# The PTY injection helper is optional: pass --with-inject when diagnosing
# terminal rendering or recording/replaying PTY byte streams.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
WITH_INJECT=0

if ! command -v cargo >/dev/null 2>&1; then
  echo "orca-tui: 未找到 cargo，请先安装 Rust（https://rustup.rs/）" >&2
  exit 127
fi

CARGO_ARGS=(--path "$SCRIPT_DIR" --locked)
while (($# > 0)); do
  case "$1" in
    --force)
      CARGO_ARGS+=(--force)
      shift
      ;;
    --with-inject)
      WITH_INJECT=1
      shift
      ;;
    --root)
      if (($# < 2)); then
        echo "用法：$0 [--force] [--root <目录>]" >&2
        exit 2
      fi
      CARGO_ARGS+=(--root "$2")
      shift 2
      ;;
    -h|--help)
      echo "用法：$0 [--force] [--with-inject] [--root <目录>]"
      echo "从当前源码安装 orca-tui；--with-inject 额外安装调试工具。"
      exit 0
      ;;
    *)
      echo "未知选项：$1" >&2
      echo "用法：$0 [--force] [--with-inject] [--root <目录>]" >&2
      exit 2
      ;;
  esac
done

echo "正在安装 orca-tui..."
cargo install "${CARGO_ARGS[@]}" --bin orca-tui

if ((WITH_INJECT)); then
  echo "正在安装可选调试工具 orca-tui-inject..."
  cargo install "${CARGO_ARGS[@]}" --features inject --bin orca-tui-inject
fi

if ((WITH_INJECT)); then
  echo "安装完成：可运行 orca-tui --help；调试工具为 orca-tui-inject"
else
  echo "安装完成：可运行 orca-tui --help（调试工具可用 --with-inject 安装）"
fi
