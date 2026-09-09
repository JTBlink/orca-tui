#!/usr/bin/env bash
set -euo pipefail

official_url="https://github.com/stablyai/orca.git"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
project_root="$(cd -- "$script_dir/../../../.." && pwd)"
repo_dir="${ORCA_REPO_DIR:-$project_root/../orca}"

if [[ "${1:-}" == "--path" ]]; then
  if [[ $# -lt 2 || -z "${2:-}" ]]; then
    echo "usage: ensure-orca.sh [--path PATH]" >&2
    exit 2
  fi
  repo_dir="$2"
  shift 2
fi

if [[ $# -ne 0 ]]; then
  echo "usage: ensure-orca.sh [--path PATH]" >&2
  exit 2
fi

if [[ -e "$repo_dir" && ! -d "$repo_dir" ]]; then
  echo "error: target exists but is not a directory: $repo_dir" >&2
  exit 1
fi

if [[ -d "$repo_dir/.git" ]]; then
  revision="$(git -C "$repo_dir" rev-parse HEAD 2>/dev/null || printf '%s' unknown)"
  remote="$(git -C "$repo_dir" remote get-url origin 2>/dev/null || printf '%s' unset)"
  version="unknown"
  if [[ -f "$repo_dir/package.json" ]] && command -v node >/dev/null 2>&1; then
    version="$(node -p "try { require(process.argv[1]).version || 'unknown' } catch (_) { 'unknown' }" "$repo_dir/package.json" 2>/dev/null || printf '%s' unknown)"
  fi
  printf 'Orca source ready\nPackage version: %s\nCommit: %s\nOrigin: %s\n' "$version" "$revision" "$remote"
  exit 0
fi

if [[ -e "$repo_dir" ]]; then
  echo "error: target exists but is not a Git repository: $repo_dir" >&2
  echo "refusing to overwrite it; choose another path with --path" >&2
  exit 1
fi

parent_dir="$(dirname -- "$repo_dir")"
mkdir -p "$parent_dir"
echo "Cloning Orca official source"
git clone --depth 1 "$official_url" "$repo_dir"
revision="$(git -C "$repo_dir" rev-parse HEAD)"
version="unknown"
if [[ -f "$repo_dir/package.json" ]] && command -v node >/dev/null 2>&1; then
  version="$(node -p "try { require(process.argv[1]).version || 'unknown' } catch (_) { 'unknown' }" "$repo_dir/package.json" 2>/dev/null || printf '%s' unknown)"
fi
printf 'Orca source ready\nPackage version: %s\nCommit: %s\nOrigin: %s\n' "$version" "$revision" "$official_url"
