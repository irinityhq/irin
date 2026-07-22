#!/usr/bin/env python3
"""
Validate council session JSON files against the v2 schema.

Usage:
    python3 tests/validate_session.py                       # validate all sessions
    python3 tests/validate_session.py sessions/council_*.json  # validate specific files
    python3 tests/validate_session.py --strict               # fail on v1 (unmigrated) files
"""
import json
import sys
from pathlib import Path


SCHEMA_PATH = Path(__file__).parent.parent / "schemas" / "session.v2.schema.json"
SESSIONS_DIR = Path(__file__).parent.parent / "sessions"


def load_schema():
    with open(SCHEMA_PATH) as f:
        return json.load(f)


def validate_session_basic(session: dict, path: str, strict: bool = False) -> list[str]:
    """Validate a session dict against v2 schema without jsonschema dependency.
    Returns list of error strings (empty = valid)."""
    errors = []

    # Check schema version
    sv = session.get("schema_version")
    if sv is None:
        if strict:
            errors.append(f"{path}: missing schema_version (v1 — run migration)")
        # Non-strict: allow v1 sessions
    elif sv != 2:
        errors.append(f"{path}: unexpected schema_version={sv}")

    # Required fields
    for field in ["session_id", "topic", "cabinet_name", "rounds", "timestamp"]:
        if field not in session:
            errors.append(f"{path}: missing required field '{field}'")

    # Validate session_id format (12-char hex preferred, UUID-style with dashes accepted)
    sid = session.get("session_id", "")
    if sid:
        clean = sid.replace("-", "")
        if not all(c in "0123456789abcdef" for c in clean):
            errors.append(f"{path}: session_id '{sid}' contains non-hex characters")

    # Validate tier if present
    tier = session.get("tier")
    if tier is not None and tier not in ("best", "sovereign", "strict_sovereign"):
        errors.append(f"{path}: invalid tier '{tier}'")

    # Validate mode if present
    mode = session.get("mode")
    # Direct-fire one-shots (Phase 6 feature contract): contrarian/munger/kiss/specops/premortem
    direct_fire_modes = {"contrarian", "munger", "kiss", "specops", "premortem"}
    valid_modes = {
        "normal", "blind", "pathfind", "teardown", "harden", "recall",
        "wargame", "unknown",
    } | direct_fire_modes
    if mode is not None and mode not in valid_modes:
        errors.append(f"{path}: invalid mode '{mode}'")

    # Validate rounds — direct-fire sessions are single-shot (no council rounds)
    rounds = session.get("rounds", [])
    if not isinstance(rounds, list) or (len(rounds) == 0 and mode not in direct_fire_modes):
        errors.append(f"{path}: rounds must be a non-empty array")
    else:
        for i, rnd in enumerate(rounds):
            rnd_errors = validate_round(rnd, f"{path}:round[{i}]", strict)
            errors.extend(rnd_errors)

    # Validate numeric fields
    for field in ["total_tokens", "total_latency_ms"]:
        val = session.get(field, 0)
        if not isinstance(val, (int, float)) or val < 0:
            errors.append(f"{path}: {field} must be non-negative number, got {val}")

    cost = session.get("total_cost_usd", 0)
    if not isinstance(cost, (int, float)) or cost < 0:
        errors.append(f"{path}: total_cost_usd must be non-negative, got {cost}")

    return errors


def validate_round(rnd: dict, prefix: str, strict: bool) -> list[str]:
    """Validate a single RoundResult."""
    errors = []

    if "round_num" not in rnd:
        errors.append(f"{prefix}: missing round_num")

    responses = rnd.get("responses", [])
    if not isinstance(responses, list):
        errors.append(f"{prefix}: responses must be array")
    else:
        for j, resp in enumerate(responses):
            for field in ["seat_name", "provider", "model", "text", "round_num"]:
                if field not in resp:
                    errors.append(f"{prefix}:response[{j}]: missing '{field}'")

    # Convergence score
    cs = rnd.get("convergence_score", 0)
    if not isinstance(cs, (int, float)) or cs < 0 or cs > 1:
        errors.append(f"{prefix}: convergence_score must be 0.0-1.0, got {cs}")

    # v2 fields (warn if strict, skip if not)
    if strict:
        for field in ["judge_provider", "judge_assessment", "flip_flop_hash"]:
            if field not in rnd:
                errors.append(f"{prefix}: missing v2 field '{field}'")

    # Validate judge_assessment structure if present
    ja = rnd.get("judge_assessment")
    if ja is not None and isinstance(ja, dict):
        ja_errors = validate_judge_assessment(ja, f"{prefix}:judge_assessment")
        errors.extend(ja_errors)

    return errors


def validate_judge_assessment(ja: dict, prefix: str) -> list[str]:
    """Validate structured judge output."""
    errors = []

    conv = ja.get("convergence")
    if conv is None:
        errors.append(f"{prefix}: missing convergence")
    elif not isinstance(conv, (int, float)) or conv < 0 or conv > 1:
        errors.append(f"{prefix}: convergence must be 0.0-1.0, got {conv}")

    if "intent_aligned" not in ja:
        errors.append(f"{prefix}: missing intent_aligned")

    rec = ja.get("recommendation")
    if rec is None:
        errors.append(f"{prefix}: missing recommendation")
    elif rec not in ("continue", "converged", "escalate", "reframe"):
        errors.append(f"{prefix}: invalid recommendation '{rec}'")

    conf = ja.get("confidence")
    if conf is not None and (not isinstance(conf, (int, float)) or conf < 0 or conf > 1):
        errors.append(f"{prefix}: confidence must be 0.0-1.0, got {conf}")

    qf = ja.get("quality_flag")
    if qf is not None and qf not in ("thin", "circular", "off_topic"):
        errors.append(f"{prefix}: invalid quality_flag '{qf}'")

    return errors


def main():
    strict = "--strict" in sys.argv
    args = [a for a in sys.argv[1:] if not a.startswith("--")]

    if args:
        files = [Path(a) for a in args]
    else:
        files = sorted(SESSIONS_DIR.glob("council_*.json"))

    print(f"Validating {len(files)} session files {'(strict v2)' if strict else '(lenient)'}")
    print()

    total_errors = 0
    valid_count = 0

    for path in files:
        try:
            with open(path) as f:
                session = json.load(f)
        except json.JSONDecodeError as e:
            print(f"  ❌ {path.name}: invalid JSON — {e}")
            total_errors += 1
            continue

        errors = validate_session_basic(session, path.name, strict=strict)
        if errors:
            for err in errors:
                print(f"  ❌ {err}")
            total_errors += len(errors)
        else:
            sv = session.get("schema_version", 1)
            print(f"  ✅ {path.name} (v{sv})")
            valid_count += 1

    print(f"\n{'=' * 50}")
    print(f"Results: {valid_count} valid, {total_errors} errors across {len(files)} files")

    return 1 if total_errors > 0 else 0


if __name__ == "__main__":
    sys.exit(main())
