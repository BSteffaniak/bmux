#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE_FILE="$ROOT_DIR/terminfo/bmux-256color.terminfo"

if ! command -v tic >/dev/null 2>&1; then
  echo "error: tic command not found; install ncurses terminfo tools first"
  exit 1
fi

if [[ ! -f "$SOURCE_FILE" ]]; then
  echo "error: terminfo source file not found at $SOURCE_FILE"
  exit 1
fi

tic -x "$SOURCE_FILE"
echo "installed terminfo entry: bmux-256color"
