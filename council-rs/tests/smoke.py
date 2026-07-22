#!/usr/bin/env python3
"""
Council smoke tests — minimal verification that the engine runs.

Usage:
    python3 tests/smoke.py           # run all smoke tests, isolated session dir
    python3 tests/smoke.py --quick   # schema validation only (no API calls)

Tests:
  1. Schema files parse as valid JSON Schema
  2. All existing sessions validate (lenient mode)
  3. Index entries validate (lenient mode)
  4. Migration script runs in dry-run mode without errors
  5. (default only) Run a --quick deliberation in a temp session dir and validate output
"""
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).parent.parent
SCHEMAS_DIR = ROOT / "schemas"
SESSIONS_DIR = ROOT / "sessions"
SCRIPTS_DIR = ROOT / "scripts"


class SmokeResult:
    def __init__(self):
        self.passed = 0
        self.failed = 0
        self.skipped = 0
        self.errors = []

    def ok(self, name):
        self.passed += 1
        print(f"  ✅ {name}")

    def fail(self, name, msg):
        self.failed += 1
        self.errors.append(f"{name}: {msg}")
        print(f"  ❌ {name}: {msg}")

    def skip(self, name, reason):
        self.skipped += 1
        print(f"  ⏭️  {name}: {reason}")

    def summary(self):
        total = self.passed + self.failed + self.skipped
        print(f"\n{'=' * 50}")
        print(f"Smoke tests: {self.passed}/{total} passed, "
              f"{self.failed} failed, {self.skipped} skipped")
        if self.errors:
            print("\nFailures:")
            for e in self.errors:
                print(f"  - {e}")
        return 1 if self.failed > 0 else 0


def test_schemas_parse(r: SmokeResult):
    """Verify all schema files are valid JSON."""
    for schema_file in sorted(SCHEMAS_DIR.glob("*.json")):
        try:
            with open(schema_file) as f:
                schema = json.load(f)
            # Basic structure checks
            assert "$schema" in schema, "missing $schema"
            assert "type" in schema, "missing type"
            r.ok(f"schema: {schema_file.name}")
        except Exception as e:
            r.fail(f"schema: {schema_file.name}", str(e))


def test_sessions_validate(r: SmokeResult):
    """Run session validator in lenient mode."""
    result = subprocess.run(
        [sys.executable, "tests/validate_session.py"],
        capture_output=True, text=True, cwd=ROOT
    )
    if result.returncode == 0:
        # Count validated sessions from output
        lines = result.stdout.strip().split("\n")
        r.ok(f"sessions validate (lenient) — {len([l for l in lines if '✅' in l])} files")
    else:
        r.fail("sessions validate", result.stdout[-200:] if result.stdout else result.stderr[-200:])


def test_index_validates(r: SmokeResult):
    """Run index validator in lenient mode."""
    index_file = SESSIONS_DIR / "index.jsonl"
    if not index_file.exists():
        r.skip("index validate", "no index.jsonl found")
        return

    result = subprocess.run(
        [sys.executable, "tests/validate_index.py"],
        capture_output=True, text=True, cwd=ROOT
    )
    if result.returncode == 0:
        lines = result.stdout.strip().split("\n")
        r.ok(f"index validates (lenient) — {len([l for l in lines if '✅' in l])} entries")
    else:
        r.fail("index validate", result.stdout[-200:] if result.stdout else result.stderr[-200:])


def test_migration_dryrun(r: SmokeResult):
    """Run migration script in dry-run mode."""
    result = subprocess.run(
        [sys.executable, "scripts/migrate_sessions_v2.py"],
        capture_output=True, text=True, cwd=ROOT
    )
    if result.returncode == 0:
        r.ok("migration dry-run")
    else:
        r.fail("migration dry-run", result.stdout[-200:] if result.stdout else result.stderr[-200:])


def test_deliberation(r: SmokeResult):
    """Run a --quick deliberation in an isolated session dir and validate output."""
    # Check if at least 2 providers are available
    available = 0
    for key in ["XAI_API_KEY", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"]:
        if os.environ.get(key):
            available += 1

    # Check for Claude CLI
    try:
        result = subprocess.run(["claude", "--version"], capture_output=True, timeout=5)
        if result.returncode == 0:
            available += 1
    except Exception:
        pass

    if available < 2:
        r.skip("deliberation", f"need ≥2 providers, found {available}")
        return

    print("  ⏳ Running --quick deliberation...")
    start = time.time()
    binary = ROOT / "target" / "release" / "council"
    if not binary.exists():
        r.fail("deliberation", f"binary not found at {binary}")
        return
    with tempfile.TemporaryDirectory(prefix="council-smoke-") as tmp:
        tmp_root = Path(tmp)
        sessions_dir = tmp_root / "sessions"
        runs_dir = tmp_root / "runs"
        sessions_dir.mkdir()
        runs_dir.mkdir()

        env = os.environ.copy()
        env["COUNCIL_SESSIONS_DIR"] = str(sessions_dir)
        env["COUNCIL_RUNS_DIR"] = str(runs_dir)

        result = subprocess.run(
            [str(binary), "--quick", "-q", "What is 2+2?"],
            capture_output=True, text=True, cwd=ROOT, timeout=120, env=env
        )
        elapsed = time.time() - start

        if result.returncode != 0:
            r.fail("deliberation", f"exit code {result.returncode}: {result.stderr[-200:]}")
            return

        session_files = sorted(sessions_dir.glob("council_*.json"))
        if not session_files:
            r.fail("deliberation", "no session file created in isolated smoke dir")
            return

        latest = session_files[-1]
        try:
            with open(latest) as f:
                session = json.load(f)
            assert "session_id" in session
            assert "synthesis" in session
            assert len(session.get("rounds", [])) >= 1

            strict = subprocess.run(
                [sys.executable, "tests/validate_session.py", "--strict", str(latest)],
                capture_output=True, text=True, cwd=ROOT
            )
            if strict.returncode != 0:
                msg = strict.stdout[-300:] if strict.stdout else strict.stderr[-300:]
                r.fail("deliberation", f"isolated session strict validation failed: {msg}")
                return

            r.ok(f"deliberation ({elapsed:.1f}s) — isolated {latest.name}")
        except Exception as e:
            r.fail("deliberation", f"session validation: {e}")


def main():
    quick = "--quick" in sys.argv
    print(f"Council Smoke Tests {'(schema-only)' if quick else '(full)'}")
    print()

    r = SmokeResult()

    test_schemas_parse(r)
    test_sessions_validate(r)
    test_index_validates(r)
    test_migration_dryrun(r)

    if not quick:
        test_deliberation(r)
    else:
        r.skip("deliberation", "quick mode — run without --quick to execute")

    return r.summary()


if __name__ == "__main__":
    sys.exit(main())
