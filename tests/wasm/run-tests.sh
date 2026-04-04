#!/usr/bin/env bash
# Run WASM tests for the zenoh-wasm port.
#
# Usage:
#   ./run-tests.sh              # run all tests (needs zenohd for session tests)
#   ./run-tests.sh basic        # run only basic (no-network) tests
#   ./run-tests.sh session      # run only session (network) tests
#
# Prerequisites: geckodriver (or chromedriver), firefox, wasm-pack, cargo
# On NixOS:  nix-shell -p geckodriver --run "./run-tests.sh"

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

ROUTER_PID=""
SUITE="${1:-all}"

cleanup() {
    if [ -n "$ROUTER_PID" ] && kill -0 "$ROUTER_PID" 2>/dev/null; then
        echo "Stopping zenohd (PID $ROUTER_PID)..."
        kill "$ROUTER_PID" 2>/dev/null || true
        wait "$ROUTER_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

run_basic() {
    echo "=== Running basic WASM tests (no network) ==="
    wasm-pack test --headless --firefox -- --test basic
    echo "=== Basic tests passed ==="
}

run_session() {
    echo "=== Building zenohd ==="
    cargo build --release -p zenohd --manifest-path ../../Cargo.toml

    echo "=== Starting zenohd on ws/127.0.0.1:7448 ==="
    ../../target/release/zenohd -l ws/127.0.0.1:7448 &
    ROUTER_PID=$!
    # Wait for router to be ready
    sleep 2

    if ! kill -0 "$ROUTER_PID" 2>/dev/null; then
        echo "ERROR: zenohd failed to start"
        exit 1
    fi

    echo "=== Running session WASM tests (with router) ==="
    wasm-pack test --headless --firefox -- --test session
    echo "=== Session tests passed ==="
}

case "$SUITE" in
    basic)   run_basic ;;
    session) run_session ;;
    all)     run_basic; run_session ;;
    *)       echo "Unknown suite: $SUITE (use basic, session, or all)"; exit 1 ;;
esac

echo "All requested tests passed."
