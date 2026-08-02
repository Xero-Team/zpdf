#!/usr/bin/env bash
# Build the zpdf wasm demo: compile for wasm32-unknown-unknown and generate
# the ES-module bindings into www/pkg. Serve www/ with any static server, e.g.
#   python -m http.server -d crates/zpdf-wasm/www 8080
#
# Requires: rustup target add wasm32-unknown-unknown
#           cargo install wasm-bindgen-cli --version <wasm-bindgen version in Cargo.lock>
set -euo pipefail
cd "$(dirname "$0")/../.."

cargo build -p zpdf-wasm --target wasm32-unknown-unknown --release
wasm-bindgen target/wasm32-unknown-unknown/release/zpdf_wasm.wasm \
  --target web --out-dir crates/zpdf-wasm/www/pkg --no-typescript
ls -la crates/zpdf-wasm/www/pkg
