#!/usr/bin/env python3
"""
Migrate existing council sessions from v1 (implicit) to v2 schema.

v2 additions per session JSON:
  - schema_version: 2
  - tier: "best" (default for all pre-sovereign sessions)
  - budget: null

v2 additions per RoundResult:
  - judge_provider: null (unknown for legacy sessions)
  - judge_assessment: null (was a naked float)
  - flip_flop_hash: null

v2 additions per index.jsonl entry:
  - schema_version: 2
  - tier: "best"
  - judge_provider: null
  - failure_status: null (ReasoningBank: no failures marked yet)
  - cited_by: []
  - challenged_by: []

Usage:
    python3 scripts/migrate_sessions_v2.py                  # dry-run
    python3 scripts/migrate_sessions_v2.py --apply           # write changes
    python3 scripts/migrate_sessions_v2.py --apply --backup  # backup originals first
"""
import json
import os
import shutil
import sys
from datetime import datetime, timezone
from pathlib import Path

SESSIONS_DIR = Path(os.environ.get("COUNCIL_SESSIONS_DIR", "sessions"))
INDEX_FILE = SESSIONS_DIR / "index.jsonl"

DRY_RUN = "--apply" not in sys.argv
BACKUP = "--backup" in sys.argv


def migrate_session(path: Path) -> tuple[bool, str]:
    """Migrate a single session JSON to v2. Returns (changed, message)."""
    with open(path) as f:
        session = json.load(f)

    already_v2 = session.get("schema_version", 0) >= 2

    # Add v2 fields
    session["schema_version"] = 2
    session.setdefault("tier", "best")
    session.setdefault("budget", None)
    session.setdefault("context_sources", [])

    # Ensure round-level v2 fields exist (even on sessions already marked v2)
    rounds_patched = False
    for rnd in session.get("rounds", []):
        for field in ("judge_provider", "judge_assessment", "flip_flop_hash"):
            if field not in rnd:
                rnd[field] = None
                rounds_patched = True

    if already_v2 and not rounds_patched:
        return False, f"  skip {path.name} (already v2)"

    if not DRY_RUN:
        if BACKUP:
            backup_path = path.with_suffix(".v1.bak")
            if not backup_path.exists():
                shutil.copy2(path, backup_path)

        with open(path, "w") as f:
            json.dump(session, f, indent=2, ensure_ascii=False)
            f.write("\n")

    return True, f"  {'[DRY] ' if DRY_RUN else ''}migrated {path.name}"


def migrate_index() -> tuple[int, int]:
    """Migrate index.jsonl entries to v2. Returns (migrated, skipped)."""
    if not INDEX_FILE.exists():
        print(f"  index.jsonl not found at {INDEX_FILE}")
        return 0, 0

    lines = INDEX_FILE.read_text().strip().split("\n")
    migrated = 0
    skipped = 0
    new_lines = []

    for line in lines:
        if not line.strip():
            continue
        entry = json.loads(line)

        if entry.get("schema_version", 0) >= 2:
            new_lines.append(json.dumps(entry, ensure_ascii=False))
            skipped += 1
            continue

        # Normalize legacy field names to v2 names
        if "session_id" in entry and "id" not in entry:
            entry["id"] = entry.pop("session_id")
        if "digest" in entry and "ruling_digest" not in entry:
            entry["ruling_digest"] = entry.pop("digest")
        if "timestamp" in entry and "ts" not in entry:
            entry["ts"] = entry.pop("timestamp")

        # Add v2 fields
        entry["schema_version"] = 2
        entry.setdefault("tier", "best")
        entry.setdefault("judge_provider", None)
        entry.setdefault("failure_status", None)
        entry.setdefault("cited_by", [])
        entry.setdefault("challenged_by", [])

        new_lines.append(json.dumps(entry, ensure_ascii=False))
        migrated += 1

    if not DRY_RUN and migrated > 0:
        if BACKUP:
            backup_path = INDEX_FILE.with_suffix(".v1.bak")
            if not backup_path.exists():
                shutil.copy2(INDEX_FILE, backup_path)

        INDEX_FILE.write_text("\n".join(new_lines) + "\n")

    return migrated, skipped


def main():
    print(f"{'DRY RUN' if DRY_RUN else 'APPLYING'} — Council Session v1 → v2 Migration")
    print(f"Sessions dir: {SESSIONS_DIR.resolve()}")
    print()

    # Migrate session JSONs
    session_files = sorted(SESSIONS_DIR.glob("council_*.json"))
    print(f"Found {len(session_files)} session files")

    changed = 0
    for path in session_files:
        was_changed, msg = migrate_session(path)
        print(msg)
        if was_changed:
            changed += 1

    print(f"\nSessions: {changed} migrated, {len(session_files) - changed} skipped")

    # Migrate index
    print(f"\nMigrating index.jsonl...")
    idx_migrated, idx_skipped = migrate_index()
    prefix = "[DRY] " if DRY_RUN else ""
    print(f"Index: {prefix}{idx_migrated} migrated, {idx_skipped} skipped")

    # Summary
    print(f"\n{'=' * 50}")
    if DRY_RUN:
        print("DRY RUN complete. Run with --apply to write changes.")
        print("Run with --apply --backup to backup originals first.")
    else:
        print(f"Migration complete. {changed} sessions + {idx_migrated} index entries updated.")
        if BACKUP:
            print("Backups saved as .v1.bak files.")

    return 0


if __name__ == "__main__":
    sys.exit(main())
