#!/usr/bin/env bash
# Builds the studio: the server, the MIT core's router, the studio core as WASM, the router as
# WASM for routing in the tab, and the web client.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
nice -n 15 cargo build --release -p studio-server
(cd ../core && nice -n 15 cargo build --release -p ultraroute)
nice -n 15 wasm-pack build core --release --target web --out-dir ../web/src/core --out-name studio_core --no-pack -- --features wasm
# The client bundles its router worker on every build, local or not.
(cd ../core && nice -n 15 wasm-pack build crates/web --release --target web \
  --out-dir ../../../studio/web/src/router --out-name router --no-pack)
cd web
# Installs again when the lockfile has moved on, such as to a new board-draw commit.
[ node_modules/.package-lock.json -nt package-lock.json ] || npm ci --no-audit --no-fund
nice -n 15 npx vite build
