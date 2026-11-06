#!/usr/bin/env bash
# Starts an isolated proxy instance with an empty data directory and verifies
# the local lifecycle without using any WorkBuddy account or upstream quota.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${WB_BIN:-$ROOT/target/debug/workbuddy-proxy}"
PORT="${WB_SMOKE_PORT:-18317}"
BASE="http://127.0.0.1:$PORT"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/wb-proxy-smoke.XXXXXX")"
LOG="$TMP_DIR/proxy.log"
PID=""

cleanup() {
  if [[ -n "$PID" ]] && kill -0 "$PID" 2>/dev/null; then
    kill -TERM "$PID" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 "$PID" 2>/dev/null || break
      sleep 0.1
    done
    kill -KILL "$PID" 2>/dev/null || true
  fi
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

if [[ ! -x "$BIN" ]]; then
  echo "proxy binary not found: $BIN (run 'cargo build' first)" >&2
  exit 1
fi

WB_DATA_DIR="$TMP_DIR/data" \
WB_CONFIG="$TMP_DIR/missing.toml" \
WB_PROXY_BIND="127.0.0.1:$PORT" \
RUST_LOG="workbuddy_proxy=warn" \
  "$BIN" >"$LOG" 2>&1 &
PID=$!

for _ in $(seq 1 100); do
  if curl -fsS "$BASE/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "$PID" 2>/dev/null; then
    cat "$LOG" >&2
    echo "proxy exited before becoming healthy" >&2
    exit 1
  fi
  sleep 0.1
done

curl -fsS "$BASE/health" >/dev/null
if [[ ! -s "$TMP_DIR/data/admin_token.txt" ]]; then
  cat "$LOG" >&2
  echo "health endpoint responded but the admin token was not created" >&2
  exit 1
fi
ADMIN="$(tr -d '[:space:]' <"$TMP_DIR/data/admin_token.txt")"
AUTH=(-H "Authorization: Bearer $ADMIN")

curl -fsS "${AUTH[@]}" "$BASE/admin/api/info" >/dev/null
curl -fsS "${AUTH[@]}" "$BASE/admin/api/settings" >/dev/null
curl -fsS "${AUTH[@]}" "$BASE/v1/models" >/dev/null
test "$(curl -sS -o /dev/null -w '%{http_code}' "$BASE/admin/api/info")" = "401"
test "$(curl -sS -o /dev/null -w '%{http_code}' "$BASE/v1/models")" = "401"
test "$(curl -sS "${AUTH[@]}" -o /dev/null -w '%{http_code}' "$BASE/v1/not-a-route")" = "404"

# Exercise the configured default key through the OpenAI-compatible auth path.
KEY="$(curl -fsS "${AUTH[@]}" "$BASE/admin/api/keys" |
  sed -n 's/.*"key":"\([^"]*\)".*/\1/p' | head -n 1)"
if [[ -n "$KEY" ]]; then
  curl -fsS -H "Authorization: Bearer $KEY" "$BASE/v1/models" >/dev/null
fi

# SIGTERM must flush a loadable state file and exit cleanly.
kill -TERM "$PID"
for _ in $(seq 1 100); do
  if ! kill -0 "$PID" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
if kill -0 "$PID" 2>/dev/null; then
  echo "proxy did not stop after SIGTERM" >&2
  exit 1
fi
PID=""
test -s "$TMP_DIR/data/state.json"
test "$(stat -c '%a' "$TMP_DIR/data/state.json")" = "600"

echo "Local lifecycle smoke test passed."
