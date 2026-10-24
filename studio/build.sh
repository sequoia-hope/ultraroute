#!/usr/bin/env bash
# Builds the studio: the server, the core as WASM, and the web client.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
nice -n 15 cargo build --release -p studio-server
nice -n 15 wasm-pack build core --release --target web --out-dir ../web/src/core --out-name studio_core --no-pack -- --features wasm
cd web
[ -d node_modules ] || npm ci --no-audit --no-fund
nice -n 15 npx vite build
