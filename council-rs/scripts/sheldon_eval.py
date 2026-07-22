#!/usr/bin/env python3
"""
sheldon_eval.py — Ground-truth eval harness for claim_validator (Sheldon).

Measures per provider/candidate (full matrix support):
  1. Local scoping guard (skip_scoped fixtures — no API $)
  2. Live validation: claim extraction, CONTRADICTED rate, citation overrides,
     latency, cost; success on scoped vs live fixtures
  3. Compare the calibrated Hermes primary, native-search Grok Build fallback,
     and the remaining canonical cascade transports

Handles cascade failover from roles.yaml (grok_hermes first) gracefully; reports
provider/step that succeeded for a given fixture when using default/cascade path.

Improved output: per-fixture details + verdict breakdown (claims/contradicted/overrides).

Usage:
  python3 scripts/sheldon_eval.py --list-fixtures
  python3 scripts/sheldon_eval.py --list-candidates
  python3 scripts/sheldon_eval.py --scoped-only
  python3 scripts/sheldon_eval.py --live --provider grok_hermes --fixture public_fact
  python3 scripts/sheldon_eval.py --live --provider grok_build --fixture public_fact
  python3 scripts/sheldon_eval.py --live --all-candidates --dry-run
  python3 scripts/sheldon_eval.py --live-all --report-file /tmp/sheldon-live.json
  python3 scripts/sheldon_eval.py --full-matrix --json

Citation override default: COUNCIL_SHELDON_CITATION_OVERRIDE=contradicted
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
FIXTURES_PATH = ROOT / "eval" / "fixtures" / "sheldon_eval.json"

CANDIDATES = [
    {"provider": "grok_hermes", "model": "grok-4.20-0309-reasoning", "role": "claim_validator_primary"},
    {"provider": "grok_build", "model": "grok-cli-default", "role": "claim_validator_native_tools_fallback"},
    {"provider": "claude_code", "model": "claude-opus-4-6", "role": "claim_validator_fallback"},
    {"provider": "codex_cli", "model": "gpt-5.6-sol", "role": "claim_validator_fallback"},
]


def council_bin() -> Path:
    candidates = [
        ROOT.parent / "target" / "release" / "council",
        ROOT.parent / "target" / "debug" / "council",
        ROOT / "target" / "release" / "council",
        ROOT / "target" / "debug" / "council",
    ]
    for candidate in candidates:
        if candidate.exists():
            return candidate
    raise SystemExit("council binary not found — run: cargo build --release")


def load_fixtures() -> Dict[str, Any]:
    if FIXTURES_PATH.exists():
        return json.loads(FIXTURES_PATH.read_text())
    raise SystemExit(f"fixtures not found: {FIXTURES_PATH}")


def run_council_eval(
    *,
    provider: str,
    model: str,
    fixture_id: Optional[str] = None,
    live: bool,
    scoped_only: bool,
    base_dir: Path,
    timeout: int,
) -> Dict[str, Any]:
    cmd = [
        str(council_bin()),
        "--sheldon-eval",
        "--base-dir",
        str(base_dir),
        "--quiet",
    ]
    if live:
        cmd.append("--sheldon-eval-live")
    if scoped_only:
        cmd.append("--sheldon-eval-scoped-only")
    if fixture_id:
        cmd.extend(["--sheldon-eval-fixture", fixture_id])
    if provider:
        cmd.extend(["--eval-provider", provider])
    if model:
        cmd.extend(["--eval-model", model])

    proc = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        timeout=timeout,
        env=os.environ.copy(),
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
    # Compute extra aggregates for improved output
    total_overrides = sum(r.get("override_count", 0) for r in results)
    total_latency = sum(r.get("latency_ms", 0) for r in results)
    scoped_results = [r for r in results if r.get("outcome") == "skip_scoped"]
    live_results = [r for r in results if r.get("outcome") in ("report", "no_report")]
    return {
        "provider": provider,
        "model": model,
        "ok": summary.get("ok", 0),
        "total": summary.get("total", 0),
        "skip_scoped_ok": summary.get("skip_scoped_ok", 0),
        "live_report_ok": summary.get("live_report_ok", 0),
        "provider_errors": summary.get("provider_errors", 0),
        "total_cost_usd": round(summary.get("total_cost_usd", 0.0), 6),
        "total_overrides": total_overrides,
        "mean_latency_ms": round(total_latency / max(1, len(results)), 1) if results else 0,
        "scoped_count": len(scoped_results),
        "live_count": len(live_results),
        "results": results,
    }


def print_detailed(rows: List[Dict[str, Any]]) -> None:
    print("\n=== Sheldon eval per-fixture details ===")
    for row in rows:
        label = f"{row['provider']}/{row['model']}"
        if "error" in row:
            print(f"  {label}: ERROR — {row['error']}")
            continue
        print(f"  {label} (ok={row['ok']}/{row['total']}, cost=${row['total_cost_usd']:.4f}, overrides={row.get('total_overrides',0)}, mean_lat={row.get('mean_latency_ms',0)}ms):")
        for r in row.get("results", []):
            fid = r.get("fixture_id", "?")
            ok = r.get("ok")
            outcome = r.get("outcome")
            claims = r.get("claim_count", 0)
            contrad = r.get("contradicted_count", 0)
            ovr = r.get("override_count", 0)
            lat = r.get("latency_ms", 0)
            cst = r.get("cost_usd", 0.0)
            err = r.get("error")
            succ = r.get("succeeded_provider") or r.get("provider")
            step = r.get("cascade_step")
            via = f"{succ}#{step}" if step is not None else succ
            print(
                f"    • {fid}: ok={ok} outcome={outcome} claims={claims} contrad={contrad} "
                f"overrides={ovr} lat={lat}ms cost=${cst:.4f} via={via}"
                + (f" err={err}" if err else "")
            )


def print_summary(rows: List[Dict[str, Any]]) -> None:
    print("\n=== Sheldon eval summary ===")
    for row in rows:
        if "error" in row:
            print(f"  {row['provider']}/{row['model']}: ERROR — {row['error']}")
            continue
        scoped = row.get('skip_scoped_ok', 0)
        live = row.get('live_report_ok', 0)
        tot = row.get('total', 0)
        print(
            f"  {row['provider']}/{row['model']}: "
            f"ok={row['ok']}/{tot} "
            f"scoped_ok={scoped}/{row.get('scoped_count',0)} "
            f"live_ok={live}/{row.get('live_count',0)} "
            f"overrides={row.get('total_overrides',0)} "
            f"cost=${row['total_cost_usd']:.4f} "
            f"mean_lat={row.get('mean_latency_ms',0)}ms"
        )


def main() -> None:
    ap = argparse.ArgumentParser(description="Sheldon claim-validator eval harness")
    ap.add_argument("--list-candidates", action="store_true")
    ap.add_argument("--list-fixtures", action="store_true")
    ap.add_argument("--scoped-only", action="store_true", help="Deterministic skip_scoped fixtures only")
    ap.add_argument("--live", action="store_true", help="Run live validator fixtures (spends API $)")
    ap.add_argument("--all-candidates", action="store_true")
    ap.add_argument("--full-matrix", action="store_true", help="Run scoped + live matrix across candidates")
    ap.add_argument("--live-all", action="store_true", help="Shortcut for --live --all-candidates")
    ap.add_argument("--provider")
    ap.add_argument("--model")
    ap.add_argument("--fixture")
    ap.add_argument("--base-dir", type=Path, default=ROOT)
    ap.add_argument("--timeout", type=int, default=600)
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--report-file", type=Path, help="Write full JSON report to this path")
    args = ap.parse_args()

    if args.list_candidates:
        print(json.dumps(CANDIDATES, indent=2))
        return
    if args.list_fixtures:
        print(json.dumps(load_fixtures(), indent=2))
        return

    if args.full_matrix or args.live_all:
        args.live = True
        args.all_candidates = True
        if args.full_matrix:
            # full-matrix implies also run scoped unless explicitly scoped_only
            pass

    if not args.scoped_only and not args.live:
        print("sheldon_eval — use --list-fixtures, --scoped-only, --live, --live-all or --full-matrix")
        print("Citation override default: COUNCIL_SHELDON_CITATION_OVERRIDE=contradicted")
        return

    if args.all_candidates:
        targets = CANDIDATES
    elif args.provider:
        targets = [
            {
                "provider": args.provider,
                "model": args.model or "grok-4.3",
                "role": "custom",
            }
        ]
    else:
        targets = [CANDIDATES[0]]

    if args.dry_run:
        modes = []
        if args.scoped_only or args.full_matrix:
            modes.append("scoped-only")
        if args.live or args.full_matrix or args.live_all:
            modes.append("live")
        if not modes:
            modes = ["scoped-only"]
        for t in targets:
            for m in modes:
                print(f"would eval {t['provider']}/{t['model']} mode={m}")
        return

    live = (args.live or args.live_all or args.full_matrix) and not args.scoped_only
    scoped_only = args.scoped_only
    # live run (with live=False scoped_only) covers scoped fixtures too; scoped_only mode is pure non-live
    run_scoped_only = args.scoped_only
    run_live = live
    rows: List[Dict[str, Any]] = []
    for t in targets:
        label = f"{t['provider']}/{t['model']}"
        if not args.json:
            print(f"Evaluating {label}...", file=sys.stderr)
        try:
            if run_scoped_only:
                report = run_council_eval(
                    provider=t["provider"],
                    model=t["model"],
                    fixture_id=args.fixture,
                    live=False,
                    scoped_only=True,
                    base_dir=args.base_dir,
                    timeout=args.timeout,
                )
                rows.append(aggregate_candidate(report, t["provider"], t["model"]))
            if run_live:
                report = run_council_eval(
                    provider=t["provider"],
                    model=t["model"],
                    fixture_id=args.fixture,
                    live=True,
                    scoped_only=False,
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
                    "ok": 0,
                    "total": 0,
                    "skip_scoped_ok": 0,
                    "live_report_ok": 0,
                    "provider_errors": 1,
                    "total_cost_usd": 0.0,
                    "total_overrides": 0,
                    "mean_latency_ms": 0,
                    "scoped_count": 0,
                    "live_count": 0,
                    "results": [],
                }
            )
            if not args.json:
                print(f"  ⚠️  {label}: {exc}", file=sys.stderr)

    if args.json:
        out = {"candidates": rows}
        print(json.dumps(out, indent=2))
        if args.report_file:
            args.report_file.write_text(json.dumps(out, indent=2))
    else:
        print_summary(rows)
        if len(rows) > 0 and any(r.get("results") for r in rows if "error" not in r):
            print_detailed(rows)
        if args.report_file:
            # still write even non-json
            full = {"candidates": rows}
            args.report_file.write_text(json.dumps(full, indent=2))
            print(f"Report written to {args.report_file}")


if __name__ == "__main__":
    main()
