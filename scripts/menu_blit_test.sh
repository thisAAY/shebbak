#!/bin/bash
# Verifies an AppleScripted menu open produces a transient blit on the client.
#
# Both host and client MUST run --release: a debug snapshot+PNG pipeline is
# 10-30x slower than release (250-950ms/iter vs 25-28ms) and can't hold the
# blit thread's 10 Hz cadence, so a debug run can pass this script while
# telling you nothing about the ~150ms acceptance budget.
set -euo pipefail
open -a TextEdit; sleep 1
PID=$(pgrep -x TextEdit | head -1)
SRW_SHARE_PIDS=$PID cargo run --release -p srw-host &
HOST_PID=$!; sleep 3
SRW_HOST=http://127.0.0.1:9009/offer cargo run --release -p srw-client > /tmp/srw-client.log 2>&1 &
CLIENT_PID=$!; sleep 5
osascript -e 'tell application "System Events" to tell process "TextEdit" to click menu bar item "Format" of menu bar 1'
sleep 2
osascript -e 'tell application "System Events" to key code 53' # Esc closes the menu
# `kill $CLIENT_PID $HOST_PID` only kills the `cargo run` wrapper processes —
# the actual srw-host/srw-client binaries they exec survive and srw-host
# keeps holding port 9009. Kill the real binaries by name instead.
pkill -f "target/release/srw-host" 2>/dev/null || true
pkill -f "target/release/srw-client" 2>/dev/null || true
kill $CLIENT_PID $HOST_PID 2>/dev/null || true
if grep -q "blit received" /tmp/srw-client.log; then
  echo "PASS: transient blit arrived"
else
  echo "FAIL: no blit in client log"; exit 1
fi
