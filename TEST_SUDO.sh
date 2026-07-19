#!/usr/bin/env bash
# Run filesync's REAL-root elevation tests (tests/sudo_elevation.rs).
#
# Usage:            sudo ./TEST_SUDO.sh
#
# These tests must START as root (they build root-owned fixtures and exercise the actual
# privilege-drop + per-operation re-escalation of a sudo-launched filesync). A plain `cargo test`
# skips them (they're #[ignore]d); this script is the only intended way to run them.
#
# What it does, and why:
#   1. Builds the test binary AS YOUR USER (never as root — target/ stays user-owned).
#   2. Runs only the ignored, root-requiring tests, serially, as root.
set -euo pipefail
cd "$(dirname "$0")"

if [[ $(id -u) -ne 0 || -z ${SUDO_UID:-} ]]; then
    echo "usage: sudo ./TEST_SUDO.sh   (run via sudo from your regular user account)" >&2
    exit 1
fi

echo "==> building filesync + the sudo_elevation test binary as uid ${SUDO_UID} (not as root)"
sudo -u "#${SUDO_UID}" -H -- bash -lc "cd \"$PWD\" && cargo build && cargo test --test sudo_elevation --no-run" >/dev/null

BIN=$(ls -t target/debug/deps/sudo_elevation-* 2>/dev/null | grep -v '\.d$' | head -1)
if [[ -z ${BIN} ]]; then
    echo "error: sudo_elevation test binary not found under target/debug/deps" >&2
    exit 1
fi

echo "==> running the root-requiring tests: ${BIN}"
# --ignored: run exactly the tests a normal `cargo test` skips.
# --test-threads=1: privilege state is process-global; keep the runs serial and readable.
exec "${BIN}" --ignored --test-threads=1 --nocapture
