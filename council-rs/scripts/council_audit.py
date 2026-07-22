#!/usr/bin/env python3
"""
council_audit.py — Corpus-scale health report for council sessions.

Crunches sessions/index.jsonl + full JSON files offline (no API spend).
Replaces one-off heritage/duo runs for prevalence questions.

Usage:
  python3 scripts/council_audit.py
  python3 scripts/council_audit.py --mode teardown,harden
  python3 scripts/council_audit.py --json > audit.json
  python3 scripts/council_audit.py --since 2026-06-01
  python3 scripts/council_audit.py --validate-stats
  python3 scripts/council_audit.py --validate-stats-only --json
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import Counter, defaultdict
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, List, Optional, Set, Tuple

ROOT = Path(__file__).resolve().parent.parent
SESSIONS_DIR = ROOT / "sessions"
INDEX_PATH = SESSIONS_DIR / "index.jsonl"
MODELS_PATH = ROOT / "models.yaml"
ROLES_PATH = ROOT / "roles.yaml"

CLI_PREFIXES = (
    "codex-cli-",
    "claude-cli-",
    "grok-cli-",
    "gemini-cli-",
    "agy-cli-",
    "agy-default",
)


def load_registered_model_ids() -> Set[str]:
    ids: Set[str] = set()
    if not MODELS_PATH.exists():
        return ids
    current = None
    for line in MODELS_PATH.read_text().splitlines():
        line = line.strip()
        if line.startswith("#") or not line:
            continue
        if line.endswith(":") and not line.startswith(" "):
            current = line[:-1]
        if "id:" in line and current:
            val = line.split("id:", 1)[1].strip().strip("\"'")
            if val:
                ids.add(val)
    return ids


def load_roles_config_simple() -> Dict[str, List[Dict[str, str]]]:
    """Minimal roles.yaml reader without PyYAML."""
    if not ROLES_PATH.exists():
        return {}
    roles: Dict[str, List[Dict[str, str]]] = {}
    current_role = None
    for line in ROLES_PATH.read_text().splitlines():
        line = line.strip()
        if line.startswith("convergence_judge:"):
            current_role = "convergence_judge"
            roles[current_role] = []
        elif line.startswith("frame_check:"):
            current_role = "frame_check"
            roles[current_role] = []
        elif current_role and line.startswith("- provider:"):
            prov = line.split("provider:", 1)[1].strip()
            roles[current_role].append({"provider": prov, "model": ""})
        elif current_role and line.startswith("model:") and roles[current_role]:
            roles[current_role][-1]["model"] = line.split("model:", 1)[1].strip()
    return roles


def canonical_provider_slug(provider: Optional[str]) -> str:
    if not provider:
        return "(unknown)"
    p = provider.strip().lower()
    return "nvidia" if p == "nim" else p


def collect_judge_provider_hits() -> Tuple[Counter, Counter]:
    """Historical judge_provider + judge_model from full session JSON."""
    providers: Counter = Counter()
    models: Counter = Counter()
    for p in SESSIONS_DIR.glob("council_*.json"):
        try:
            data = json.loads(p.read_text())
        except Exception:
            continue
        for rnd in data.get("rounds", []):
            prov = rnd.get("judge_provider")
            if prov:
                providers[canonical_provider_slug(prov)] += 1
            model = rnd.get("judge_model")
            if model:
                models[model] += 1
    return providers, models


def collect_validation_stats() -> Dict[str, Any]:
    """Sheldon claim validator stats from validation_report in full session JSONs.

    Counts sessions/rounds with reports, verdict dist (SUPPORTED/CONSISTENT/NO_EVIDENCE/CONTRADICTED),
    _overridden_from rate (citation override), per-round judge_provider proxy for validator,
    CONTRADICTED rate, early gate usage via redaction markers.

    Efficient: raw-text prefilter before json.loads (skip non-val sessions).
    Follows style of collect_judge_provider_hits.
    """
    sessions_with: Set[str] = set()
    rounds_with = 0
    total_claims = 0
    verdict_counts: Counter = Counter()
    overridden = 0
    provider_rounds: Counter = Counter()
    early_gate_rounds = 0
    contradict_sessions = 0
    prov_verdicts: Dict[str, Counter] = {}

    for p in SESSIONS_DIR.glob("council_*.json"):
        try:
            raw = p.read_text(encoding="utf-8", errors="ignore")
            if "validation_report" not in raw:
                continue
            data = json.loads(raw)
            sid = data.get("session_id") or data.get("id") or p.name
            sess_has = False
            sess_contr = False
            for rnd in data.get("rounds", []) or []:
                vr = rnd.get("validation_report")
                if isinstance(vr, list) and len(vr) > 0:
                    rounds_with += 1
                    sess_has = True
                    prov = canonical_provider_slug(rnd.get("judge_provider"))
                    provider_rounds[prov] += 1
                    if prov not in prov_verdicts:
                        prov_verdicts[prov] = Counter()
                    # early gate detect: responses redacted on CONTRADICTED when validate_gate active
                    responses = rnd.get("responses", []) or []
                    has_redact = any(
                        "[REDACTED" in str((resp or {}).get("text", ""))
                        or "CONTRADICTED by evidence" in str((resp or {}).get("text", ""))
                        for resp in responses if isinstance(resp, dict)
                    )
                    if has_redact:
                        early_gate_rounds += 1
                    for item in vr:
                        if not isinstance(item, dict):
                            continue
                        total_claims += 1
                        v = str(item.get("verdict") or "").upper().strip()
                        if v in ("SUPPORTED", "VERIFIED"):
                            nv = "SUPPORTED"
                        elif v in ("CONSISTENT", "PLAUSIBLE"):
                            nv = "CONSISTENT"
                        elif v in ("NO_EVIDENCE", "UNVERIFIED"):
                            nv = "NO_EVIDENCE"
                        elif v == "CONTRADICTED":
                            nv = "CONTRADICTED"
                        else:
                            nv = "NO_EVIDENCE"
                        verdict_counts[nv] += 1
                        prov_verdicts[prov][nv] += 1
                        if item.get("_overridden_from") is not None:
                            overridden += 1
                        if nv == "CONTRADICTED":
                            sess_contr = True
            if sess_has:
                sessions_with.add(sid)
            if sess_contr:
                contradict_sessions += 1
        except Exception:
            continue

    totalc = max(1, total_claims)
    return {
        "sessions_with_validation_report": len(sessions_with),
        "rounds_with_validation_report": rounds_with,
        "total_claims_validated": total_claims,
        "verdict_distribution": {
            "SUPPORTED": verdict_counts["SUPPORTED"],
            "CONSISTENT": verdict_counts["CONSISTENT"],
            "NO_EVIDENCE": verdict_counts["NO_EVIDENCE"],
            "CONTRADICTED": verdict_counts["CONTRADICTED"],
        },
        "overridden_from_count": overridden,
        "overridden_from_rate": round(overridden / totalc, 4),
        "rounds_by_judge_provider": dict(provider_rounds.most_common()),
        "verdicts_by_provider": {p: dict(c) for p, c in prov_verdicts.items()},
        "contradicted_claims": verdict_counts["CONTRADICTED"],
        "contradicted_rate": round(verdict_counts["CONTRADICTED"] / totalc, 4),
        "early_gate_rounds_detected": early_gate_rounds,
        "sessions_with_at_least_one_contradicted": contradict_sessions,
    }


def matches_registry(model: str, registered: Set[str]) -> bool:
    if not model:
        return False
    if model in registered:
        return True
    best_len = 0
    matched = False
    for reg in registered:
        if model.startswith(reg) and len(reg) > best_len:
            matched = True
            best_len = len(reg)
        elif reg.startswith(model) and len(reg) > best_len:
            matched = True
            best_len = len(reg)
    return matched


def classify_model(model: str, registered: Set[str]) -> str:
    if not model:
        return "empty"
    if matches_registry(model, registered):
        if any(model.startswith(p) for p in CLI_PREFIXES):
            return "cli_registered"
        return "registered"
    if any(model.startswith(p) for p in CLI_PREFIXES):
        return "cli_unregistered"
    return "unregistered"


def load_index() -> List[Dict[str, Any]]:
    if not INDEX_PATH.exists():
        print("No index.jsonl found", file=sys.stderr)
        return []
    out = []
    for line in INDEX_PATH.read_text().splitlines():
        line = line.strip()
        if line:
            out.append(json.loads(line))
    return out


def find_full_session(sid: str) -> Optional[Path]:
    for pattern in (f"{sid}.json", f"council_{sid}.json"):
        p = SESSIONS_DIR / pattern
        if p.exists():
            return p
    for p in SESSIONS_DIR.glob(f"*{sid}*.json"):
        return p
    return None


def load_full(sid: str) -> Optional[Dict[str, Any]]:
    p = find_full_session(sid)
    if not p:
        return None
    try:
        return json.loads(p.read_text())
    except Exception:
        return None


def session_ts(entry: Dict[str, Any], full: Optional[Dict[str, Any]]) -> str:
    for src in (entry, full or {}):
        for key in ("ts", "timestamp", "created_at"):
            v = src.get(key)
            if isinstance(v, str) and v:
                return v[:10]
    return ""


def analyze_session(
    entry: Dict[str, Any], full: Optional[Dict[str, Any]], registered: Set[str]
) -> Dict[str, Any]:
    sid = entry.get("id") or entry.get("session_id") or ""
    stats: Dict[str, Any] = {
        "id": sid,
        "cabinet": entry.get("cabinet") or (full or {}).get("cabinet_name"),
        "mode": entry.get("mode") or (full or {}).get("mode"),
        "topic": (entry.get("topic") or (full or {}).get("topic") or "")[:80],
        "ts": session_ts(entry, full),
        "model_classes": Counter(),
        "errors": [],
        "homos": [],
        "early_stop": False,
        "rounds_planned": None,
        "rounds_run": 0,
        "unregistered_models": set(),
    }
    if not full:
        return stats

    stats["rounds_planned"] = full.get("rounds_planned") or len(full.get("rounds", []))
    stats["rounds_run"] = len(full.get("rounds", []))
    if stats["rounds_planned"] and stats["rounds_run"] < stats["rounds_planned"]:
        last = full["rounds"][-1] if full.get("rounds") else {}
        if last.get("converged"):
            stats["early_stop"] = True

    for rnd in full.get("rounds", []):
        ja = rnd.get("judge_assessment") or {}
        if isinstance(ja, dict) and ja.get("homogeneity_score") is not None:
            stats["homos"].append(float(ja["homogeneity_score"]))
        for resp in rnd.get("responses", []):
            model = resp.get("model") or ""
            cls = classify_model(model, registered)
            stats["model_classes"][cls] += 1
            if cls == "unregistered" and model:
                stats["unregistered_models"].add(model)
            if resp.get("error"):
                stats["errors"].append(
                    {
                        "seat": resp.get("seat_name"),
                        "model": model or "(empty)",
                        "error": str(resp["error"])[:120],
                    }
                )
    return stats


def main() -> None:
    ap = argparse.ArgumentParser(description="Corpus audit for council sessions")
    ap.add_argument("--mode", help="Comma-separated modes")
    ap.add_argument("--cabinet", help="Substring filter on cabinet name")
    ap.add_argument("--since", help="ISO date YYYY-MM-DD — only sessions on/after")
    ap.add_argument("--limit", type=int, default=0, help="Max sessions (0=all)")
    ap.add_argument("--homo-threshold", type=float, default=0.75)
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--bad-models-only", action="store_true")
    ap.add_argument("--validate-stats", action="store_true", help="Compute and show Sheldon claim validator (validation_report) stats")
    ap.add_argument("--validate-stats-only", action="store_true", help="Only emit validation stats (skip full corpus analysis)")
    args = ap.parse_args()

    registered = load_registered_model_ids()
    index = load_index()

    if args.mode:
        wanted = {m.strip() for m in args.mode.split(",")}
        index = [e for e in index if e.get("mode") in wanted]
    if args.cabinet:
        sub = args.cabinet.lower()
        index = [e for e in index if sub in str(e.get("cabinet", "")).lower()]
    if args.since:
        index = [e for e in index if session_ts(e, None) >= args.since]
    if args.limit:
        index = index[: args.limit]

    # Sheldon validation stats (cheap selective scan; prefilter + only on demand)
    validation_stats = collect_validation_stats()

    if args.validate_stats_only:
        if args.json:
            print(json.dumps(validation_stats, indent=2))
            return
        print("=== Sheldon claim validator stats ===\n")
        vs = validation_stats
        print(f"Sessions with validation_report: {vs['sessions_with_validation_report']}")
        print(f"Rounds with validation_report:   {vs['rounds_with_validation_report']}")
        print(f"Total claims validated:          {vs['total_claims_validated']}")
        print("Verdict distribution:")
        for v in ("SUPPORTED", "CONSISTENT", "NO_EVIDENCE", "CONTRADICTED"):
            c = vs["verdict_distribution"].get(v, 0)
            pct = 100.0 * c / max(1, vs["total_claims_validated"])
            print(f"  {v:12s} {c:5d}  ({pct:.1f}%)")
        print(f"_overridden_from (citation overrides): {vs['overridden_from_count']} (rate={vs['overridden_from_rate']})")
        print("Rounds with reports by judge_provider (validator proxy):")
        for prov, n in vs.get("rounds_by_judge_provider", {}).items():
            print(f"  {prov:12s} {n:5d}")
        print(f"CONTRADICTED claims: {vs['contradicted_claims']} (rate {vs['contradicted_rate']})")
        print(f"Early gate rounds (redaction markers): {vs['early_gate_rounds_detected']}")
        print(f"Sessions w/ >=1 CONTRADICTED: {vs['sessions_with_at_least_one_contradicted']}")
        if vs.get("verdicts_by_provider"):
            print("Per-provider verdict counts (sample):")
            for prov, vd in list(vs["verdicts_by_provider"].items())[:3]:
                print(f"  {prov}: {vd}")
        return

    # Full corpus analysis — every session gets a full file load
    per_session: List[Dict[str, Any]] = []
    model_usage: Counter = Counter()
    model_class_totals: Counter = Counter()
    error_msgs: Counter = Counter()
    error_seats: Counter = Counter()
    early_stops = 0
    high_homo: List[Dict[str, Any]] = []
    all_homos: List[float] = []
    missing_full = 0

    for entry in index:
        sid = entry.get("id") or entry.get("session_id")
        if not sid:
            continue
        full = load_full(sid)
        if not full:
            missing_full += 1
            continue
        stats = analyze_session(entry, full, registered)
        if args.bad_models_only and not stats["unregistered_models"]:
            continue
        per_session.append(stats)

        for cls, n in stats["model_classes"].items():
            model_class_totals[cls] += n
        for m in stats["unregistered_models"]:
            model_usage[m] += 1
        for err in stats["errors"]:
            error_msgs[err["error"][:60]] += 1
            error_seats[err["seat"]] += 1
        if stats["early_stop"]:
            early_stops += 1
        for h in stats["homos"]:
            all_homos.append(h)
            if h >= args.homo_threshold:
                high_homo.append(
                    {"id": sid, "homo": h, "cabinet": stats["cabinet"], "topic": stats["topic"]}
                )

    modes = Counter(e.get("mode") for e in index)
    cabs = Counter(e.get("cabinet") for e in index)
    bad_sessions = [s for s in per_session if s["unregistered_models"]]
    roles_cfg = load_roles_config_simple()
    judge_providers, judge_models = collect_judge_provider_hits()

    report = {
        "sessions_in_index": len(index),
        "sessions_analyzed": len(per_session),
        "missing_full_json": missing_full,
        "registered_model_ids": len(registered),
        "mode_counts": dict(modes),
        "top_cabinets": dict(cabs.most_common(8)),
        "seat_response_classes": dict(model_class_totals),
        "sessions_with_unregistered_models": len(bad_sessions),
        "unregistered_model_ids": sorted(model_usage.keys()),
        "unregistered_model_session_counts": dict(model_usage.most_common()),
        "seat_errors_total": sum(len(s["errors"]) for s in per_session),
        "top_error_messages": dict(error_msgs.most_common(8)),
        "top_error_seats": dict(error_seats.most_common(8)),
        "early_stop_sessions": early_stops,
        "homogeneity": {
            "samples": len(all_homos),
            "mean": round(sum(all_homos) / len(all_homos), 4) if all_homos else None,
            "high_count": len(high_homo),
            "threshold": args.homo_threshold,
        },
        "roles_configured": roles_cfg,
        "historical_judge_providers": dict(judge_providers.most_common()),
        "historical_judge_models": dict(judge_models.most_common(12)),
        "validation_report_stats": validation_stats,
    }

    if args.json:
        print(json.dumps(report, indent=2))
        return

    print(f"=== Council corpus audit ({report['sessions_analyzed']}/{report['sessions_in_index']} sessions) ===\n")
    print(f"Registered model IDs: {report['registered_model_ids']}")
    if missing_full:
        print(f"⚠️  {missing_full} index entries missing full JSON\n")

    print("Modes:", report["mode_counts"])
    print("Top cabinets:", report["top_cabinets"])

    print("\n--- Utility roles (roles.yaml) vs corpus judge history ---")
    if roles_cfg:
        for role_name in ("convergence_judge", "frame_check"):
            steps = roles_cfg.get(role_name, [])
            if steps:
                print(f"  Configured {role_name}:")
                for i, step in enumerate(steps, 1):
                    prov = canonical_provider_slug(step.get("provider"))
                    print(f"    {i}. {prov}/{step.get('model', '?')}")
    else:
        print("  (roles.yaml not found — council uses built-in defaults)")
    if judge_providers:
        print("  Historical convergence judge providers (all rounds):")
        total = sum(judge_providers.values())
        for prov, n in judge_providers.most_common():
            pct = 100.0 * n / max(1, total)
            print(f"    {prov:12s} {n:5d}  ({pct:.1f}%)")
        top_models = judge_models.most_common(5)
        if top_models:
            print("  Top historical judge models:")
            for m, n in top_models:
                print(f"    {m}: {n}")
    else:
        print("  No judge_provider recorded in corpus yet")

    print("\n--- Seat response classification (all rounds) ---")
    for cls, n in model_class_totals.most_common():
        pct = 100.0 * n / max(1, sum(model_class_totals.values()))
        print(f"  {cls:20s} {n:5d}  ({pct:.1f}%)")

    print("\n--- Unregistered runtime model strings ---")
    if bad_sessions:
        print(f"  {len(bad_sessions)} sessions still hit unregistered IDs:")
        for m, c in model_usage.most_common():
            print(f"    {m}: {c} sessions")
    else:
        print("  ✅ All runtime model strings resolve to models.yaml")

    print("\n--- Seat errors (historical failures) ---")
    print(f"  Total error responses: {report['seat_errors_total']}")
    if error_msgs:
        print("  Top messages:")
        for msg, c in error_msgs.most_common(5):
            print(f"    [{c:3d}] {msg}")
        print("  Top seats:")
        for seat, c in error_seats.most_common(5):
            print(f"    [{c:3d}] {seat}")

    print("\n--- Early convergence (stopped before planned rounds) ---")
    print(f"  {early_stops} sessions early-stopped")

    print("\n--- Homogeneity (post-WIP field; sparse in historical corpus) ---")
    h = report["homogeneity"]
    if h["samples"]:
        print(f"  {h['samples']} round samples, mean={h['mean']}, >= {h['threshold']}: {h['high_count']}")
        for s in high_homo[:5]:
            print(f"    {s['id'][:12]}  homo={s['homo']:.3f}  {s.get('cabinet','')}")
    else:
        print("  No homogeneity_score in corpus yet (feature is forward-looking)")

    if args.validate_stats:
        print("\n--- Sheldon claim validator (validation_report) stats ---")
        vs = validation_stats
        print(f"  Sessions with reports: {vs['sessions_with_validation_report']}")
        print(f"  Rounds with reports:   {vs['rounds_with_validation_report']}")
        print(f"  Total claims validated: {vs['total_claims_validated']}")
        print("  Verdict distribution:")
        for v in ("SUPPORTED", "CONSISTENT", "NO_EVIDENCE", "CONTRADICTED"):
            c = vs["verdict_distribution"].get(v, 0)
            pct = 100.0 * c / max(1, vs["total_claims_validated"])
            print(f"    {v:12s} {c:5d}  ({pct:.1f}%)")
        print(f"  _overridden_from (citation override kicked in): {vs['overridden_from_count']} / {vs['total_claims_validated']} (rate={vs['overridden_from_rate']})")
        print("  Rounds by judge_provider (validator proxy):")
        for prov, n in vs.get("rounds_by_judge_provider", {}).items():
            print(f"    {prov:12s} {n:5d}")
        print(f"  CONTRADICTED rate: {vs['contradicted_rate']} ({vs['contradicted_claims']} claims)")
        print(f"  Early gate rounds detected (via redactions): {vs['early_gate_rounds_detected']}")
        print(f"  Sessions w/ >=1 CONTRADICTED: {vs['sessions_with_at_least_one_contradicted']}")

    if args.bad_models_only and bad_sessions:
        print("\n--- Bad-model sessions (detail) ---")
        for s in bad_sessions[:15]:
            print(f"  {s['id'][:12]}  {sorted(s['unregistered_models'])}")


if __name__ == "__main__":
    main()
