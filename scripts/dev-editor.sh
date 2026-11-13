#!/usr/bin/env bash
# Rebuild and restart nova-edit whenever its source (or the crates it uses) changes.
#   scripts/dev-editor.sh [project-dir-or-file]     (default: the current directory)
# Needs: cargo install cargo-watch --locked
set -euo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
target="$(realpath "${1:-$PWD}")"   # cargo runs from the repo root, so pass an absolute path
cd "$(dirname "$0")/.."
exec cargo watch --quiet --clear --delay 0.5 \
  -w crates/novacode-editor -w crates/novacode-ai -w crates/novacode-base -w crates/novacode-types \
  -x "run -p novacode-editor -- '$target'"
