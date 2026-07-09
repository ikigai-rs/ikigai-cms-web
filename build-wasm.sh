#!/usr/bin/env bash
# Build the browser wire codec to wasm and generate the JS bindings into dist/.
# Needs the wasm32 target (`rustup target add wasm32-unknown-unknown`) and
# wasm-bindgen-cli matching the pinned wasm-bindgen (`cargo install wasm-bindgen-cli
# --version 0.2.108`).
set -euo pipefail
cd "$(dirname "$0")"
cargo build --release --lib --target wasm32-unknown-unknown
wasm-bindgen --target web --out-dir dist \
  target/wasm32-unknown-unknown/release/ikigai_cms_web.wasm
echo "built dist/ikigai_cms_web.js + dist/ikigai_cms_web_bg.wasm"
echo "serve dist/ and open index.html with #cert=<hash> from the running cms-server"
