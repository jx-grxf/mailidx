#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
mkdir -p dist
lipo -create -output dist/mailidx \
  target/aarch64-apple-darwin/release/mailidx \
  target/x86_64-apple-darwin/release/mailidx
cd dist
tar -czf mailidx-macos-universal.tar.gz mailidx
shasum -a 256 mailidx-macos-universal.tar.gz | tee mailidx-macos-universal.tar.gz.sha256
