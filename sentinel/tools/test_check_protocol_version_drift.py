#!/usr/bin/env python3
"""Unit tests for the sovereign-protocol version drift guard.

Dependency-free (stdlib assert + tempfile). Run: python3 tools/test_check_protocol_version_drift.py

Covers the version extractors and the end-to-end main() exit code: aligned
consumers PASS (0), any divergence FAILs (1), absent consumers SKIP (not fail).
"""

from __future__ import annotations

import os
import sys
import tempfile

from check_protocol_version_drift import consumer_version, main, ssot_version

SSOT = '[package]\nname = "sovereign-protocol"\nversion = "0.1.1"\n\n[dependencies]\nserde = "1"\n'
DEP_INLINE = '[dependencies]\nsovereign-protocol = { version = "0.1.1", path = "../x" }\n'
DEP_BARE = '[dependencies]\nsovereign-protocol = "0.1.1"\n'


def _write(d, name, body):
    p = os.path.join(d, name)
    with open(p, "w", encoding="utf-8") as fh:
        fh.write(body)
    return p


def run() -> int:
    failures: list[str] = []

    def expect(cond: bool, msg: str) -> None:
        if not cond:
            failures.append(msg)

    with tempfile.TemporaryDirectory() as d:
        ssot = _write(d, "ssot.toml", SSOT)
        # extractor: SSOT [package] version, not the [dependencies] serde version
        expect(ssot_version(ssot) == "0.1.1", "ssot_version should read [package] version")

        inline = _write(d, "inline.toml", DEP_INLINE)
        bare = _write(d, "bare.toml", DEP_BARE)
        expect(consumer_version(inline) == "0.1.1", "inline dep version parse")
        expect(consumer_version(bare) == "0.1.1", "bare dep version parse")

        # Section-table form must parse without crashing.
        section = _write(
            d, "section.toml",
            '[dependencies.sovereign-protocol]\nversion = "0.1.1"\npath = "../x"\n',
        )
        expect(consumer_version(section) == "0.1.1", "section-table dep version parse")
        # dev/target-prefixed section header also matches
        dev_section = _write(
            d, "dev_section.toml",
            '[dev-dependencies.sovereign-protocol]\npath = "../x"\nversion = "0.1.1"\n',
        )
        expect(
            consumer_version(dev_section) == "0.1.1",
            "dev-dependencies section-table parse",
        )
        # section present but no version line -> ValueError (not a silent wrong answer)
        no_ver = _write(
            d, "nover.toml",
            '[dependencies.sovereign-protocol]\npath = "../x"\n\n[other]\nx = 1\n',
        )
        try:
            consumer_version(no_ver)
            expect(False, "section without version must raise ValueError")
        except ValueError:
            pass

        # aligned consumers (all three manifest forms) -> PASS (0)
        expect(
            main(["prog", ssot, inline, bare, section]) == 0,
            "aligned consumers (inline+bare+section) must PASS",
        )

        # one diverged consumer -> FAIL (1)
        drifted = _write(
            d, "drift.toml",
            '[dependencies]\nsovereign-protocol = { version = "0.1.2", path = "../x" }\n',
        )
        expect(main(["prog", ssot, inline, drifted]) == 1, "divergence must FAIL")

        # absent consumer -> SKIP, still PASS overall
        absent = os.path.join(d, "does-not-exist.toml")
        expect(main(["prog", ssot, inline, absent]) == 0, "absent consumer must SKIP not FAIL")

        # missing SSOT -> FAIL
        expect(main(["prog", absent, inline]) == 1, "missing SSOT must FAIL")

    if failures:
        print("FAIL:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("ok: all drift-guard tests passed")
    return 0


if __name__ == "__main__":
    sys.exit(run())
