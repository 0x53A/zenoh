#!/usr/bin/env bash
# Build the threaded WASM test with SharedArrayBuffer support.
#
# Requires: nightly Rust with rust-src component
set -euo pipefail

cd "$(dirname "$0")"

echo "Building with +atomics,+bulk-memory,+shared-memory,+import-memory..."
echo "(First build is slow due to -Zbuild-std)"

cargo build --target wasm32-unknown-unknown --release

echo "Running wasm-bindgen..."
# Find wasm-bindgen in wasm-pack's cache or use nix
WASM_BINDGEN=""
for dir in "$HOME"/.cache/.wasm-pack/wasm-bindgen-*/; do
  if [ -f "$dir/wasm-bindgen" ]; then
    WASM_BINDGEN="$dir/wasm-bindgen"
  fi
done

if [ -z "$WASM_BINDGEN" ]; then
  echo "wasm-bindgen not found. Using nix-shell..."
  nix-shell -p wasm-bindgen-cli --run "wasm-bindgen \
    --target no-modules \
    --out-dir pkg \
    target/wasm32-unknown-unknown/release/zenoh_wasm_threaded_test.wasm"
else
  "$WASM_BINDGEN" \
    --target no-modules \
    --out-dir pkg \
    target/wasm32-unknown-unknown/release/zenoh_wasm_threaded_test.wasm
fi

echo ""
echo "Build complete!"
echo "Run:  python3 serve.py"
echo "Open: http://localhost:8081"
