#!/usr/bin/env bash
# Build the zpdf wasm demo: compile for wasm32-unknown-unknown and generate
# the ES-module bindings into www/pkg. Serve www/ with any static server, e.g.
#   python -m http.server -d crates/zpdf-wasm/www 8080
#
# Requires: rustup target add wasm32-unknown-unknown
#           cargo install wasm-bindgen-cli --version <wasm-bindgen version in Cargo.lock>
#           Node.js (package fingerprint and runtime smoke tests)
set -euo pipefail
cd "$(dirname "$0")/../.."

cargo build -p zpdf-wasm --target wasm32-unknown-unknown --release
wasm-bindgen target/wasm32-unknown-unknown/release/zpdf_wasm.wasm \
  --target web --out-dir crates/zpdf-wasm/www/pkg --no-typescript
node crates/zpdf-wasm/www/package-fingerprint.mjs --write
node --test crates/zpdf-wasm/www/package.test.mjs
ls -la crates/zpdf-wasm/www/pkg
