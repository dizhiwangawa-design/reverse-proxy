#!/usr/bin/env bash
# Restart the local proxy and smoke test it.
#
#   ./scripts/restart.sh
#   WB_PORT=8318 WB_PROXY_BIND=127.0.0.1:8318 ./scripts/restart.sh
#
# Stops whatever owns the port, starts target/release/workbuddy-proxy with the
# project's data directory, waits for /health, then runs scripts/smoke.sh.
#
# If the port is held by a process in another PID namespace (a container, or a
# terminal this shell cannot signal), the script says so and exits without
# touching the data directory.
set -euo pipefail

cd "$(dirname "$0")/.."

PORT="${WB_PORT:-8317}"
BIND="${WB_PROXY_BIND:-127.0.0.1:$PORT}"
BIN="target/release/workbuddy-proxy"
LOG="${WB_LOG:-proxy.log}"

# Local checks must never go through a corporate/http proxy.
export no_proxy="127.0.0.1,localhost${no_proxy:+,$no_proxy}"
export NO_PROXY="$no_proxy"

if [[ ! -x "$BIN" ]]; then
  echo "missing $BIN; run 'make build' (or 'cargo build --release') first" >&2
  exit 1
fi

pids_on_port() {
  ss -ltnpH "sport = :$PORT" 2>/dev/null | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u | tr '\n' ' '
}

pids_str="$(pids_on_port || true)"
if [[ -z "${pids_str// /}" ]]; then
  pids_str="$(pgrep -f "$BIN" 2>/dev/null | tr '\n' ' ' || true)"
fi
pids=()
for p in $pids_str; do
  [[ "$p" == "$$" ]] || pids+=("$p")
done

if [[ ${#pids[@]} -gt 0 ]]; then
  echo "stopping ${pids[*]}"
  kill "${pids[@]}" 2>/dev/null || true
  for _ in $(seq 1 20); do
    alive=0
    for p in "${pids[@]}"; do kill -0 "$p" 2>/dev/null && alive=1; done
    [[ "$alive" -eq 0 ]] && break
    sleep 0.25
  done
  kill -9 "${pids[@]}" 2>/dev/null || true
  sleep 0.3
fi

if ss -ltnH "sport = :$PORT" 2>/dev/null | grep -q .; then
  echo "port $PORT is still held by a process this shell cannot see or signal." >&2
  echo "stop it where it was started (e.g. Ctrl+C in that terminal, or systemctl --user stop workbuddy-proxy)," >&2
  echo "then rerun this script." >&2
  exit 2
fi

echo "starting $BIN on $BIND"
# The child must keep the same data directory the smoke test reads from.
WB_DATA_DIR="${WB_DATA_DIR:-$PWD/data}" \
  WB_PROXY_BIND="$BIND" nohup "$BIN" >>"$LOG" 2>&1 &
NEW_PID=$!

for _ in $(seq 1 40); do
  if curl -fsS --max-time 1 "http://$BIND/health" >/dev/null 2>&1; then
    echo "up: pid $NEW_PID, log $LOG"
    break
  fi
  if ! kill -0 "$NEW_PID" 2>/dev/null; then
    echo "process exited during startup; last log lines:" >&2
    tail -n 20 "$LOG" >&2
    exit 1
  fi
  sleep 0.25
done

curl -fsS "http://$BIND/health"; echo
echo
BASE="http://$BIND" ./scripts/smoke.sh
