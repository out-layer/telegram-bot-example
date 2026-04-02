#!/bin/bash
set -e
cd "$(dirname "$0")"
RUSTFLAGS='-C link-arg=-s' cargo build --target wasm32-unknown-unknown --release
ls -lh ../target/wasm32-unknown-unknown/release/deposit_helper.wasm
