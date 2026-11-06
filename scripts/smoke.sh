#!/usr/bin/env bash
# Smoke test for a running WorkBuddy Proxy instance.
#
#   ./scripts/smoke.sh
#   BASE=http://127.0.0.1:8317 WB_SMOKE_MODEL=deepseek-v4.1-flash ./scripts/smoke.sh
#
# The chat tests consume a small amount of upstream quota (max_tokens: 8).
set -euo pipefail

# Local checks must never go through a corporate/http proxy.
export no_proxy="127.0.0.1,localhost${no_proxy:+,$no_proxy}"
export NO_PROXY="$no_proxy"

BASE="${BASE:-http://127.0.0.1:8317}"
DATA_DIR="${WB_DATA_DIR:-data}"
MODEL="${WB_SMOKE_MODEL:-auto}"
ADMIN="${WB_ADMIN_TOKEN:-}"

if [[ -z "$ADMIN" && -f "$DATA_DIR/admin_token.txt" ]]; then
  ADMIN="$(tr -d '[:space:]' < "$DATA_DIR/admin_token.txt")"
fi
if [[ -z "$ADMIN" ]]; then
  echo "admin token not found; set WB_ADMIN_TOKEN or run from the project root" >&2
  exit 1
fi

failures=0
pass() { echo "  PASS  $1"; }
fail() { echo "  FAIL  $1"; failures=$((failures + 1)); }

check() {
  local name="$1"
  shift
  if "$@" >/dev/null 2>&1; then
    pass "$name"
  else
    fail "$name"
  fi
}

echo "Smoke testing $BASE"
check "health" curl -fsS "$BASE/health"
check "admin info (bearer)" curl -fsS -H "Authorization: Bearer $ADMIN" "$BASE/admin/api/info"
check "admin overview" curl -fsS -H "Authorization: Bearer $ADMIN" "$BASE/admin/api/overview"
check "models" curl -fsS -H "Authorization: Bearer $ADMIN" "$BASE/v1/models"
check "admin token as x-api-key" curl -fsS -H "x-api-key: $ADMIN" "$BASE/v1/models"
check "count_tokens" curl -fsS \
  -H "Authorization: Bearer $ADMIN" \
  -H "anthropic-version: 2023-06-01" \
  -H "Content-Type: application/json" \
  -d '{"model":"auto","messages":[{"role":"user","content":"hello"}]}' \
  "$BASE/v1/messages/count_tokens"
check "openai completion" curl -fsS --max-time 120 \
  -H "Authorization: Bearer $ADMIN" \
  -H "Content-Type: application/json" \
  -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Reply with OK\"}],\"max_tokens\":8}" \
  "$BASE/v1/chat/completions"
check "anthropic messages" curl -fsS --max-time 120 \
  -H "Authorization: Bearer $ADMIN" \
  -H "anthropic-version: 2023-06-01" \
  -H "Content-Type: application/json" \
  -d "{\"model\":\"$MODEL\",\"max_tokens\":8,\"messages\":[{\"role\":\"user\",\"content\":\"Reply with OK\"}]}" \
  "$BASE/v1/messages"

responses_non_stream() {
  curl -fsS --max-time 120 \
    -H "Authorization: Bearer $ADMIN" \
    -H "Content-Type: application/json" \
    -d "{\"model\":\"$MODEL\",\"input\":\"Reply with OK\",\"max_output_tokens\":16}" \
    "$BASE/v1/responses" | grep '"object":"response"' >/dev/null
}

responses_stream() {
  # Read the whole stream: `grep -q` would close the pipe early and make curl
  # fail with SIGPIPE under `set -o pipefail`.
  curl -fsS -N --max-time 120 \
    -H "Authorization: Bearer $ADMIN" \
    -H "Content-Type: application/json" \
    -d "{\"model\":\"$MODEL\",\"input\":\"Reply with OK\",\"stream\":true}" \
    "$BASE/v1/responses" | awk '/^event: response.completed/{found=1} END{exit !found}'
}

check "responses (non-stream)" responses_non_stream
check "responses (stream)" responses_stream

if [[ "$failures" -ne 0 ]]; then
  echo
  echo "$failures smoke check(s) failed" >&2
  exit 1
fi
echo
echo "All smoke checks passed."
