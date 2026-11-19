#!/usr/bin/env bash
# Build vox-engine (the Kokoro engine) in release mode and put it where
# Tauri's externalBin looks for it: src-tauri/binaries/vox-engine-<target>.
# Then bundle with: tauri build --config src-tauri/tauri.engine.conf.json
#
# Usage: scripts/build-engine.sh [target]   (default: this machine's target)
#        scripts/build-engine.sh universal-apple-darwin
set -euo pipefail
cd "$(dirname "$0")/../src-tauri"

target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
ext=""
case "$target" in *windows*) ext=".exe" ;; esac
mkdir -p binaries

if [ "$target" = universal-apple-darwin ]; then
  for t in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release -p vox-engine --target "$t"
  done
  lipo -create -output binaries/vox-engine-universal-apple-darwin \
    target/aarch64-apple-darwin/release/vox-engine \
    target/x86_64-apple-darwin/release/vox-engine
else
  cargo build --release -p vox-engine --target "$target"
  cp "target/$target/release/vox-engine$ext" "binaries/vox-engine-$target$ext"
fi
echo "Built src-tauri/binaries/vox-engine-$target$ext"
