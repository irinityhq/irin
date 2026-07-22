#!/usr/bin/env python3
"""
judge_eval.py — Ground-truth eval harness for convergence judge + frame-check models.

Measures per candidate model:
  1. JSON parse rate on judge.v2 schema (structured convergence prompt)
  2. Frame-check: CLEAN vs ASSUMPTION line format compliance
  3. Calibration: judge convergence vs human-labeled fixture sessions

Usage:
  python3 scripts/judge_eval.py --list-candidates
  python3 scripts/judge_eval.py --list-fixtures
  python3 scripts/judge_eval.py --live --provider nvidia --model z-ai/glm-5.1
  python3 scripts/judge_eval.py --live --all-candidates --role judge
  python3 scripts/judge_eval.py --live --all-candidates --role frame --dry-run

Lab meta-rule: no cascade protocol change on N<5 with noise-overlapping results.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional

ROOT = Path(__file__).resolve().parent.parent
FIXTURES_PATH = ROOT / "eval" / "fixtures" / "judge_eval.json"
MIN_N_FOR_PROTOCOL_CHANGE = 5

CANDIDATES = [
    {"provider": "nvidia", "model": "deepseek-ai/deepseek-v4-pro", "role": "judge_primary"},
    {"provider": "nvidia", "model": "z-ai/glm-5.1", "role": "judge_candidate"},
    {"provider": "nvidia", "model": "moonshotai/kimi-k2.6", "role": "judge_candidate"},
    {"provider": "gemini", "model": "gemini-3.5-flash", "role": "judge_candidate"},
    {"provider": "gemini", "model": "gemini-3.1-flash-lite", "role": "judge_candidate"},
    {"provider": "gpt", "model": "gpt-5.4-nano", "role": "judge_fallback_unvalidated"},
    {"provider": "grok", "model": "grok-4.3", "role": "frontier_reference"},
    {"provider": "grok", "model": "grok-4.20-0309-reasoning", "role": "frontier_reference"},
]


def council_bin() -> Path:
    release = ROOT / "target" / "release" / "council"
    debug = ROOT / "target" / "debug" / "council"
    if release.exists():
        return release
    if debug.exists():
        return debug
    raise SystemExit(
        "council binary not found — run: cargo build --release"
    )


def load_fixtures() -> Dict[str, Any]:
    if FIXTURES_PATH.exists():
        return json.loads(FIXTURES_PATH.read_text())
    raise SystemExit(f"fixtures not found: {FIXTURES_PATH}")


def run_council_eval(
    *,
    provider: str,
    model: str,
    role: str,
    fixture_id: Optional[str] = None,
    base_dir: Path,
    timeout: int,
) -> Dict[str, Any]:
    env = os.environ.copy()
    judge_role = role in ("judge", "both", "convergence", "convergence_judge")

    cmd = [
        str(council_bin()),
        "--judge-eval",
        "--judge-eval-role",
        role,
        "--base-dir",
        str(base_dir),
        "--quiet",
    ]
    if fixture_id:
        cmd.extend(["--judge-eval-fixture", fixture_id])
    if provider:
        cmd.extend(["--eval-provider", provider])
    if model:
        cmd.extend(["--eval-model", model])

    proc = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        timeout=timeout,
        env=env,
        cwd=str(ROOT),
    )
    if proc.returncode != 0:
        raise RuntimeError(
            proc.stderr.strip() or proc.stdout.strip() or f"exit {proc.returncode}"
        )
    return json.loads(proc.stdout)


def aggregate_candidate(report: Dict[str, Any], provider: str, model: str) -> Dict[str, Any]:
    summary = report.get("summary", {})
    results = report.get("results", [])
    judge_runs = [r for r in results if r.get("role") == "convergence_judge"]
    frame_runs = [r for r in results if r.get("role") == "frame_check"]
    return {
        "provider": provider,
        "model": model,
        "json_parse_ok": summary.get("json_parse_ok", 0),
        "judge_fixtures": len(judge_runs),
        "frame_format_ok": summary.get("frame_format_ok", 0),
        "frame_expect_match": summary.get("frame_expect_match", 0),
        "frame_fixtures": len(frame_runs),
        "mean_calibration_error": summary.get("mean_calibration_error"),
        "provider_errors": summary.get("provider_errors", 0),
        "total_cost_usd": round(sum(r.get("cost_usd", 0.0) for r in results), 6),
        "results": results,
    }


def print_live_summary(rows: List[Dict[str, Any]]) -> None:
    print("\n=== Judge eval summary ===")
    for row in rows:
        print(
            f"  {row['provider']}/{row['model']}: "
            f"judge_json={row['json_parse_ok']}/{row['judge_fixtures']} "
            f"frame_ok={row['frame_expect_match']}/{row['frame_fixtures']} "
            f"cal_err={row['mean_calibration_error']} "
            f"cost=${row['total_cost_usd']:.4f}"
        )
    scored = [r for r in rows if r["judge_fixtures"] or r["frame_fixtures"]]
    if len(scored) >= MIN_N_FOR_PROTOCOL_CHANGE:
        print(
            f"\n  N={len(scored)} candidates scored "
            f"(≥{MIN_N_FOR_PROTOCOL_CHANGE} — results may inform protocol change)"
        )
    else:
        print(
            f"\n  N={len(scored)} candidates scored "
            f"(<{MIN_N_FOR_PROTOCOL_CHANGE} — lab meta-rule: no protocol change yet)"
        )


def main() -> None:
    ap = argparse.ArgumentParser(description="Judge/frame-check model eval harness")
    ap.add_argument("--list-candidates", action="store_true")
    ap.add_argument("--list-fixtures", action="store_true")
    ap.add_argument("--live", action="store_true", help="Call council --judge-eval (spends API $)")
    ap.add_argument("--all-candidates", action="store_true")
    ap.add_argument("--provider")
    ap.add_argument("--model")
    ap.add_argument("--role", default="both", help="judge, frame, or both")
    ap.add_argument("--fixture")
    ap.add_argument("--base-dir", type=Path, default=ROOT)
    ap.add_argument("--timeout", type=int, default=300)
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()

    if args.list_candidates:
        print(json.dumps(CANDIDATES, indent=2))
        return
    if args.list_fixtures:
        print(json.dumps(load_fixtures(), indent=2))
        return

    if not args.live:
        print("judge_eval — use --list-candidates, --list-fixtures, or --live")
        print("Corpus fact: gpt-5.4-nano has 0 historical judge_provider hits in sessions/")
        return

    targets: List[Dict[str, str]]
    if args.all_candidates:
        targets = CANDIDATES
    elif args.provider and args.model:
        targets = [{"provider": args.provider, "model": args.model, "role": "custom"}]
    else:
        raise SystemExit("--live requires --provider + --model, or --all-candidates")

    if args.dry_run:
        for t in targets:
            print(f"would eval {t['provider']}/{t['model']} role={args.role}")
        return

    rows: List[Dict[str, Any]] = []
    for t in targets:
        label = f"{t['provider']}/{t['model']}"
        if not args.json:
            print(f"Evaluating {label}...", file=sys.stderr)
        try:
            report = run_council_eval(
                provider=t["provider"],
                model=t["model"],
                role=args.role,
                fixture_id=args.fixture,
                base_dir=args.base_dir,
                timeout=args.timeout,
            )
            rows.append(aggregate_candidate(report, t["provider"], t["model"]))
        except Exception as exc:
            rows.append(
                {
                    "provider": t["provider"],
                    "model": t["model"],
                    "error": str(exc),
                    "json_parse_ok": 0,
                    "judge_fixtures": 0,
                    "frame_format_ok": 0,
                    "frame_expect_match": 0,
                    "frame_fixtures": 0,
                    "mean_calibration_error": None,
                    "provider_errors": 1,
                    "total_cost_usd": 0.0,
                    "results": [],
                }
            )
            if not args.json:
                print(f"  ⚠️  {label}: {exc}", file=sys.stderr)

    if args.json:
        print(json.dumps({"candidates": rows}, indent=2))
    else:
        print_live_summary(rows)


if __name__ == "__main__":
    main()
