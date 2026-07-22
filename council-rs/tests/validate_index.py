#!/usr/bin/env python3
"""
Validate index.jsonl entries against the v2 schema.

Usage:
    python3 tests/validate_index.py             # validate default index
    python3 tests/validate_index.py --strict     # fail on v1 entries
"""
import json
import sys
from pathlib import Path

SESSIONS_DIR = Path(__file__).parent.parent / "sessions"
INDEX_FILE = SESSIONS_DIR / "index.jsonl"


def validate_entry(entry: dict, line_num: int, strict: bool) -> list[str]:
    """Validate a single index entry. Returns list of errors."""
    errors = []
    prefix = f"line {line_num}"

    # Schema version
    sv = entry.get("schema_version")
    if sv is None and strict:
        errors.append(f"{prefix}: missing schema_version (v1 — run migration)")
    elif sv is not None and sv != 2:
        errors.append(f"{prefix}: unexpected schema_version={sv}")

    # Required fields (accept legacy alternatives)
    field_aliases = {
        "id": ["session_id"],
        "ts": ["timestamp"],
        "ruling_digest": ["digest"],
    }
    for field in ["id", "ts", "topic", "keywords", "ruling_digest"]:
        if field not in entry:
            aliases = field_aliases.get(field, [])
            if not any(a in entry for a in aliases):
                errors.append(f"{prefix}: missing required '{field}'")

    # ID format (12-char hex preferred, UUID-style with dashes accepted)
    eid = entry.get("id", entry.get("session_id", ""))
    if eid:
        clean = eid.replace("-", "")
        if not all(c in "0123456789abcdef" for c in clean):
            errors.append(f"{prefix}: id '{eid}' contains non-hex characters")

    # Keywords must be array of strings
    kw = entry.get("keywords", [])
    if not isinstance(kw, list):
        errors.append(f"{prefix}: keywords must be array")
    elif not all(isinstance(k, str) for k in kw):
        errors.append(f"{prefix}: keywords must be strings")

    # Convergence
    conv = entry.get("convergence")
    if conv is not None and (not isinstance(conv, (int, float)) or conv < 0 or conv > 1):
        errors.append(f"{prefix}: convergence must be 0.0-1.0, got {conv}")

    # Confidence
    conf = entry.get("confidence")
    valid_conf = {"HIGH", "MEDIUM-HIGH", "MEDIUM", "LOW", "UNKNOWN"}
    if conf is not None and conf not in valid_conf:
        errors.append(f"{prefix}: invalid confidence '{conf}'")

    # v2 fields
    if strict:
        tier = entry.get("tier")
        if tier is not None and tier not in ("best", "sovereign", "strict_sovereign"):
            errors.append(f"{prefix}: invalid tier '{tier}'")

        # ReasoningBank fields
        fs = entry.get("failure_status")
        if fs is not None and isinstance(fs, dict):
            wm = fs.get("weight_multiplier")
            if wm is not None and (not isinstance(wm, (int, float)) or wm < 0 or wm > 1):
                errors.append(f"{prefix}: weight_multiplier must be 0.0-1.0")

    return errors


def main():
    strict = "--strict" in sys.argv

    if not INDEX_FILE.exists():
        print(f"Index file not found: {INDEX_FILE}")
        return 1

    lines = INDEX_FILE.read_text().strip().split("\n")
    print(f"Validating {len(lines)} index entries {'(strict v2)' if strict else '(lenient)'}")
    print()

    total_errors = 0
    valid_count = 0

    for i, line in enumerate(lines, 1):
        if not line.strip():
            continue
        try:
            entry = json.loads(line)
        except json.JSONDecodeError as e:
            print(f"  ❌ line {i}: invalid JSON — {e}")
            total_errors += 1
            continue

        errors = validate_entry(entry, i, strict=strict)
        if errors:
            for err in errors:
                print(f"  ❌ {err}")
            total_errors += len(errors)
        else:
            topic_short = entry.get("topic", "")[:60]
            sv = entry.get("schema_version", 1)
            print(f"  ✅ line {i} [{entry.get('id','')}] (v{sv}) {topic_short}")
            valid_count += 1

    print(f"\n{'=' * 50}")
    print(f"Results: {valid_count} valid, {total_errors} errors across {len(lines)} entries")

    return 1 if total_errors > 0 else 0


if __name__ == "__main__":
    sys.exit(main())
