#!/usr/bin/env python3
"""Compare two CouncilSession JSON files — key quality signals."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional


def load(path: Path) -> Dict[str, Any]:
    return json.loads(path.read_text())


def verdict_hist(report: Optional[List[Dict[str, Any]]]) -> Dict[str, int]:
    if not report:
        return {}
    out: Dict[str, int] = {}
    for v in report:
        k = v.get("verdict", "?")
        out[k] = out.get(k, 0) + 1
    return out


def seat_snippet(responses: List[Dict[str, Any]], name: str, n: int = 140) -> str:
    for r in responses:
        if r.get("seat_name") == name:
            t = (r.get("text") or "").replace("\n", " ")
            return t[:n] + ("…" if len(t) > n else "")
    return "(missing)"


def frame_signals(text: str) -> List[str]:
    flags = []
    lower = text.lower()
    if any(x in lower for x in ("postgresql", "elasticsearch", "explain analyze", "pg_trgm")):
        flags.append("sql/search-engine frame")
    if any(x in lower for x in ("mutable legal", "legal data", "consistency sla")):
        flags.append("legal-data frame")
    if "index.jsonl" in lower or "precedent/mod.rs" in lower:
        flags.append("code-grounded")
    return flags


def summarize(path: Path, label: str) -> Dict[str, Any]:
    s = load(path)
    rounds = s.get("rounds") or []
    r1 = rounds[0] if rounds else {}
    ja = r1.get("judge_assessment") or {}
    vr = r1.get("validation_report")
    responses = r1.get("responses") or []
    return {
        "label": label,
        "path": str(path),
        "session_id": s.get("session_id"),
        "timestamp": s.get("timestamp"),
        "cabinet": s.get("cabinet_name"),
        "rounds_run": len(rounds),
        "cabinet_rounds": s.get("cabinet_rounds", 2),
        "early_stop": len(rounds) == 1 and (s.get("cabinet_rounds") or 2) > 1,
        "convergence": r1.get("convergence_score"),
        "converged": r1.get("converged"),
        "judge_provider": r1.get("judge_provider"),
        "homogeneity": ja.get("homogeneity_score"),
        "quality_flag": ja.get("quality_flag"),
        "intent_aligned": ja.get("intent_aligned"),
        "recommendation": ja.get("recommendation"),
        "validation_claims": len(vr or []),
        "validation_verdicts": verdict_hist(vr),
        "total_cost": s.get("total_cost_usd"),
        "seat_frames": {
            r.get("seat_name"): frame_signals(r.get("text") or "")
            for r in responses
        },
        "seat_snippets": {
            name: seat_snippet(responses, name)
            for name in ("Skeptic", "Mirror", "Tao", "Strategist")
        },
        "synthesis_head": (s.get("synthesis") or "")[:400],
    }


def print_compare(a: Dict[str, Any], b: Dict[str, Any]) -> None:
    keys = [
        ("rounds_run", "Rounds executed"),
        ("early_stop", "Early stop at R1"),
        ("convergence", "R1 convergence"),
        ("homogeneity", "R1 homogeneity"),
        ("quality_flag", "Quality flag"),
        ("judge_provider", "Judge provider"),
        ("validation_claims", "Sheldon claims (R1)"),
        ("validation_verdicts", "Sheldon verdicts"),
        ("total_cost", "Total cost USD"),
    ]
    print(f"\n{'Metric':<22} {'BEFORE':<28} {'AFTER':<28}")
    print("-" * 78)
    for key, title in keys:
        print(f"{title:<22} {str(a.get(key)):<28} {str(b.get(key)):<28}")

    print("\n--- Seat frame signals (hallucinated substrate detector) ---")
    for seat in ("Skeptic", "Mirror", "Tao", "Strategist"):
        print(f"  {seat}:")
        print(f"    BEFORE: {a['seat_frames'].get(seat, [])}")
        print(f"    AFTER:  {b['seat_frames'].get(seat, [])}")

    print("\n--- Synthesis lead ---")
    print(f"BEFORE: {a['synthesis_head']}…")
    print(f"AFTER:  {b['synthesis_head']}…")


def main() -> None:
    ap = argparse.ArgumentParser(description="Diff two council session JSON files")
    ap.add_argument("before")
    ap.add_argument("after")
    ap.add_argument("--json", action="store_true")
    args = ap.parse_args()
    before = summarize(Path(args.before), "BEFORE")
    after = summarize(Path(args.after), "AFTER")
    if args.json:
        print(json.dumps({"before": before, "after": after}, indent=2))
    else:
        print(f"BEFORE: {before['path']} ({before['session_id']})")
        print(f"AFTER:  {after['path']} ({after['session_id']})")
        print_compare(before, after)


if __name__ == "__main__":
    main()
