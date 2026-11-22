#!/usr/bin/env python3
"""Probe the upstream chat endpoint directly, bypassing the proxy.

Bypassing the proxy is what separates "the upstream did not think" from "our
proxy failed to record that it thought": this sees the raw SSE frames and the
upstream's own `usage`.

It reports two independent signals on purpose. `reasoning_tokens` is the
usually-authoritative one, but some models (e.g. step-5-preview) always report
0 while still streaming `delta.reasoning_content`, so `rsn_chars` has to be
read alongside it — neither alone tells you whether thinking happened.

Usage:
    python3 scripts/probe-upstream.py --model space-bunny --repeat 2
    python3 scripts/probe-upstream.py --model deepseek-v4.1-flash --levels low,high,max

Environment:
    WB_DATA_DIR   directory holding state.json (default: ./data)
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
import urllib.error
import urllib.request
from pathlib import Path

DEFAULT_ENDPOINT = "https://copilot.tencent.com"
ALL_LEVELS = [None, "none", "minimal", "low", "medium", "high", "xhigh", "max"]
# Short and identical every time: differences must come from the knob, not the prompt.
PROMPT = "用三句话说明为什么 TCP 需要三次握手，不要用列表。"


def account() -> tuple[str, str, str]:
    base = Path(os.environ.get("WB_DATA_DIR") or "data")
    path = base / "state.json"
    if not path.exists():
        raise SystemExit(f"state.json not found at {path}")
    state = json.load(open(path))
    best = None
    for a in state.get("accounts") or []:
        if not a.get("enabled") or a.get("invalid") or not a.get("access_token"):
            continue
        if best is None or (a.get("expires_at") or 0) > (best.get("expires_at") or 0):
            best = a
    if best is None:
        raise SystemExit("no usable account in state.json")
    endpoint = (best.get("endpoint") or DEFAULT_ENDPOINT).rstrip("/")
    return best["access_token"], best.get("uid") or "", endpoint


def call(token: str, uid: str, endpoint: str, model: str, effort: str | None):
    body = {
        "model": model,
        "messages": [{"role": "user", "content": PROMPT}],
        "stream": True,
        "max_tokens": 2048,
    }
    if effort is not None:
        body["reasoning_effort"] = effort
    headers = {
        "content-type": "application/json",
        "authorization": "Bearer " + token,
        "accept": "text/event-stream",
    }
    if uid:
        headers["x-user-id"] = uid
    req = urllib.request.Request(
        endpoint + "/v2/chat/completions",
        data=json.dumps(body).encode(),
        headers=headers,
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=240) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except Exception as e:  # noqa: BLE001
        return 0, f"request failed: {e}"


def summarize(raw: str) -> dict:
    out = {"reasoning_chars": 0, "content_chars": 0, "usage": None, "finish": None}
    for line in raw.splitlines():
        if not line.startswith("data:"):
            continue
        chunk = line[5:].strip()
        if not chunk or chunk == "[DONE]":
            continue
        try:
            ev = json.loads(chunk)
        except ValueError:
            continue
        if not isinstance(ev, dict):
            continue
        ch = (ev.get("choices") or [{}])[0]
        delta = ch.get("delta") or {}
        # Upstream spells it `reasoning_content`, not `reasoning`.
        for key in ("reasoning_content", "reasoning"):
            if isinstance(delta.get(key), str):
                out["reasoning_chars"] += len(delta[key])
        if isinstance(delta.get("content"), str):
            out["content_chars"] += len(delta["content"])
        if ch.get("finish_reason"):
            out["finish"] = ch["finish_reason"]
        if ev.get("usage"):
            out["usage"] = ev["usage"]
    return out


def reasoning_tokens(usage: dict | None) -> int:
    if not usage:
        return 0
    details = usage.get("completion_tokens_details") or {}
    return details.get("reasoning_tokens") or 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="deepseek-v4.1-flash")
    ap.add_argument("--repeat", type=int, default=1, help="samples per level")
    ap.add_argument("--levels", help="comma-separated levels (default: all eight)")
    args = ap.parse_args()

    token, uid, endpoint = account()
    levels: list[str | None] = ALL_LEVELS
    if args.levels:
        levels = [lv.strip() for lv in args.levels.split(",")]
        levels = [None if lv == "absent" else lv for lv in levels]

    print(f"endpoint={endpoint}  model={args.model}  repeat={args.repeat}")
    print(f"{'effort':<10} {'http':<5} {'rsn_chars':<11} {'txt_chars':<11} {'r_tokens':<9} finish")
    print("-" * 70)

    samples: dict[str, list[int]] = {}
    for lv in levels:
        for i in range(args.repeat):
            status, raw = call(token, uid, endpoint, args.model, lv)
            label = str(lv) if lv is not None else "absent"
            if status != 200:
                print(f"{label:<10} {status:<5} {raw[:90]}")
                break
            s = summarize(raw)
            n = reasoning_tokens(s["usage"])
            samples.setdefault(label, []).append(n)
            tag = f"#{i + 1}" if args.repeat > 1 else ""
            print(
                f"{label + tag:<10} {status:<5} {s['reasoning_chars']:<11} "
                f"{s['content_chars']:<11} {n:<9} {s['finish']}"
            )

    print()
    print("median reasoning_tokens per level:")
    for label, vals in samples.items():
        med = statistics.median(vals) if vals else 0
        print(f"  {label:<10} n={len(vals)}  median={med:g}  values={vals}")

    # Explicit warning: zero usage does not imply zero thinking.
    zero_usage_but_text = [
        label for label, vals in samples.items() if vals and max(vals) == 0
    ]
    if zero_usage_but_text:
        print(
            "\nNOTE: these levels reported reasoning_tokens=0 — check rsn_chars before "
            f"concluding thinking was off: {', '.join(zero_usage_but_text)}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
