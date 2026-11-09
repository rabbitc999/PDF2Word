#!/usr/bin/env bash
set -euo pipefail
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROFILE=debug
EMBED=1
for arg in "$@"; do
  case "$arg" in
    --release) PROFILE=release ;;
    --no-embed) EMBED=0 ;;
    -h|--help) echo "Usage: ./build.sh [--release] [--no-embed]"; exit 0 ;;
    *) echo "Unknown option: $arg" >&2; exit 2 ;;
  esac
done
if [[ "$EMBED" == 1 && ! -f "$ROOT_DIR/vendor/pandoc.exe" ]]; then
  powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$ROOT_DIR/scripts/build.ps1" -Release
fi
ARGS=(build --bin pdf_to_word --bin pdf_to_word_gui)
if [[ "$EMBED" == 1 ]]; then ARGS+=(--features embed-pandoc); else ARGS+=(--no-default-features); fi
[[ "$PROFILE" == release ]] && ARGS+=(--release)
(cd "$ROOT_DIR" && cargo "${ARGS[@]}")
