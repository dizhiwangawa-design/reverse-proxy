#!/usr/bin/env python3
"""Dump every model the upstream publishes, with its reasoning declaration.

The gateway's own catalogue is the only authoritative source for effort
levels: `models[].reasoning.supportedEfforts` says which rungs each model
honours, and it differs per model (5 models expose low/high/max, two expose
five rungs, three expose only `high`, 16 expose a single `effort` value and
9 have no `reasoning` object at all). Never hard-code one ladder.

Usage:
    python3 scripts/dump-model-efforts.py                  # table for all models
    python3 scripts/dump-model-efforts.py --summary         # group by effort set
    python3 scripts/dump-model-efforts.py --json out.json   # keep raw catalogue

Environment:
    WB_DATA_DIR   directory holding state.json (default: ./data)
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.request
from collections import Counter, defaultdict
from pathlib import Path

DEFAULT_ENDPOINT = "https://copilot.tencent.com"


def state_path() -> Path:
    base = Path(os.environ.get("WB_DATA_DIR") or "data")
    return base / "state.json"


def pick_account() -> tuple[str, str, str]:
    """Prefer the account with the furthest expiry — it is least likely to be stale."""
    path = state_path()
    if not path.exists():
        raise SystemExit(f"state.json not found at {path}")
    state = json.load(open(path))
    best = None
    for a in state.get("accounts") or []:
        if not a.get("enabled") or a.get("invalid"):
            continue
        if not a.get("access_token"):
            continue
        if best is None or (a.get("expires_at") or 0) > (best.get("expires_at") or 0):
            best = a
    if best is None:
        raise SystemExit("no usable account in state.json")
    endpoint = (best.get("endpoint") or DEFAULT_ENDPOINT).rstrip("/")
    return best["access_token"], best.get("uid") or "", endpoint


def fetch(url: str, token: str, uid: str) -> dict:
    headers = {"accept": "application/json", "authorization": "Bearer " + token}
    if uid:
        headers["x-user-id"] = uid
    req = urllib.request.Request(url, headers=headers, method="GET")
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read().decode())


def flatten(payload):
    """The catalogue nests differently across builds; find the model list."""
    if isinstance(payload, list):
        return payload
    if isinstance(payload, dict):
        for key in ("data", "models", "list", "result"):
            if key in payload:
                return flatten(payload[key])
    return []


def effort_set(model: dict) -> tuple[str, str, str, str]:
    """Return (supportedEfforts, defaultEffort, canDisableThinking, form)."""
    r = model.get("reasoning")
    if not isinstance(r, dict):
        return ("-", "-", "-", "no reasoning object")
    efforts = r.get("supportedEfforts")
    if isinstance(efforts, list):
        return (
            "|".join(str(e) for e in efforts),
            str(r.get("defaultEffort", "-")),
            str(r.get("canDisableThinking")),
            "supportedEfforts",
        )
    if "effort" in r:
        return (f"single:{r.get('effort')}", "-", "-", "reasoning.effort")
    return ("-", "-", "-", "reasoning object without efforts")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--json", help="write the raw catalogue here")
    ap.add_argument("--summary", action="store_true", help="group by effort set")
    args = ap.parse_args()

    token, uid, endpoint = pick_account()
    url = endpoint + "/v2/enterprises/personal/models"
    print(f"GET {url}")
    try:
        payload = fetch(url, token, uid)
    except urllib.error.HTTPError as e:
        print(f"HTTP {e.code}: {e.read().decode()[:400]}", file=sys.stderr)
        return 1
    except Exception as e:  # noqa: BLE001
        print(f"request failed: {e}", file=sys.stderr)
        return 1

    if args.json:
        with open(args.json, "w") as f:
            json.dump(payload, f, ensure_ascii=False, indent=1)
        print(f"raw catalogue -> {args.json}")

    models = [m for m in flatten(payload) if isinstance(m, dict)]
    print(f"models: {len(models)}\n")
    if not models:
        print(json.dumps(payload, ensure_ascii=False)[:1200])
        return 1

    rows = []
    for m in models:
        mid = m.get("id") or m.get("name") or "?"
        efforts, default, can_disable, form = effort_set(m)
        rows.append((mid, efforts, default, can_disable, form))

    if args.summary:
        groups = defaultdict(list)
        for mid, efforts, default, can_disable, form in rows:
            groups[(efforts, default, can_disable, form)].append(mid)
        for (efforts, default, can_disable, form), ids in sorted(
            groups.items(), key=lambda kv: -len(kv[1])
        ):
            print(f"[{efforts}]  default={default}  canDisable={can_disable}  ({form})")
            print(f"  {len(ids)} models: {', '.join(ids)}\n")
        return 0

    print(f"{'model':30} {'supportedEfforts':30} {'default':8} {'canDisable':11} form")
    print("-" * 105)
    for mid, efforts, default, can_disable, form in rows:
        print(f"{mid:30} {efforts:30} {default:8} {can_disable:11} {form}")

    counts = Counter(e for _, e, _, _, _ in rows)
    print("\ndistinct supportedEfforts:")
    for label, n in counts.most_common():
        print(f"  {label:34} {n} models")
    return 0


if __name__ == "__main__":
    sys.exit(main())
