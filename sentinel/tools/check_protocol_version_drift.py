#!/usr/bin/env python3
"""Workspace sovereign-protocol version drift guard.

The `sovereign-protocol` crate is the comms-contract SSOT. Its version is declared
once in `sentinel/sovereign-protocol/Cargo.toml` (`[package] version`) and pinned
by each consumer (gateway, council-rs) in its `Cargo.toml` dependency line.

Because the consumers use a PATH dependency with a caret requirement (e.g.
`version = "0.1.1"` == `^0.1.1`), a `0.1.1 -> 0.1.2` bump in the SSOT *silently
satisfies* an un-bumped consumer requirement: every `cargo build` stays green
while the workspace packages disagree about which contract version they speak.
That can allow a contract field change to misroute silently across consumers.

This guard extracts the EXACT version string from the SSOT and from each
consumer's `sovereign-protocol` dependency line and fails if any consumer that is
present diverges from the SSOT. Exact-equality (not semver-satisfies) is the
point: catching the within-`0.1.x` drift that the caret hides.

Layout resolution:
  - SSOT is found relative to this file: <sentinel>/sovereign-protocol/Cargo.toml
  - consumers are searched at sibling component paths under the workspace root:
    <workspace>/gateway/sidecar-rs/Cargo.toml and
    <workspace>/council-rs/Cargo.toml.
  - any consumer not present is reported SKIPPED, never failed, so the check is
    safe to run from a sentinel-only checkout.

Override paths explicitly with: check_protocol_version_drift.py SSOT C1 C2 ...
(the first arg is the SSOT Cargo.toml, the rest are consumer Cargo.tomls).

NOT doing here (scoped out by design):
  - Cargo.lock parsing — the declared-version check above is the meaningful SSOT
    drift; tightening to lockfile pins can come later if path deps become git deps.
  - COMMS_CONTRACT.md staleness, which requires a separate documentation check.

Rollback: CI-only guard — revert the workflow wiring and/or delete this file; no
runtime impact.
"""

import os
import re
import sys

# --- version extractors ----------------------------------------------------

_PACKAGE_HEADER = re.compile(r"^\s*\[package\]\s*$")
_NEXT_SECTION = re.compile(r"^\s*\[")
_VERSION_LINE = re.compile(r'^\s*version\s*=\s*"([^"]+)"')
# inline-table form: `sovereign-protocol = { ... version = "X" ... }`
_DEP_INLINE = re.compile(
    r'^\s*sovereign-protocol\s*=\s*\{[^}]*?\bversion\s*=\s*"([^"]+)"'
)
# bare form: `sovereign-protocol = "X"`
_DEP_BARE = re.compile(r'^\s*sovereign-protocol\s*=\s*"([^"]+)"')
# section-table header, any prefix: `[dependencies.sovereign-protocol]`,
# `[dev-dependencies.sovereign-protocol]`, `[workspace.dependencies...]`,
# `[target.'cfg(...)'.dependencies.sovereign-protocol]`, etc.
_DEP_SECTION = re.compile(r"^\s*\[.*\bdependencies\.sovereign-protocol\]\s*$")


def ssot_version(cargo_toml_path):
    """Return the `[package] version` string from the crate's own Cargo.toml."""
    in_package = False
    with open(cargo_toml_path, encoding="utf-8") as fh:
        for line in fh:
            if _PACKAGE_HEADER.match(line):
                in_package = True
                continue
            if in_package:
                if _NEXT_SECTION.match(line):
                    break  # left [package] without finding version
                m = _VERSION_LINE.match(line)
                if m:
                    return m.group(1)
    raise ValueError(f"no [package] version found in {cargo_toml_path}")


def consumer_version(cargo_toml_path):
    """Return the sovereign-protocol dependency version pinned by a consumer.

    Handles all three Cargo manifest forms for the dependency:
      inline:  sovereign-protocol = { version = "X", path = "..." }
      bare:    sovereign-protocol = "X"
      section: [dependencies.sovereign-protocol]\n version = "X"
    """
    in_dep_section = False
    with open(cargo_toml_path, encoding="utf-8") as fh:
        for line in fh:
            # single-line forms first
            m = _DEP_INLINE.match(line) or _DEP_BARE.match(line)
            if m:
                return m.group(1)
            # section-table form: enter on the header, read its version, stop at
            # the next section.
            if _DEP_SECTION.match(line):
                in_dep_section = True
                continue
            if in_dep_section:
                if _NEXT_SECTION.match(line):
                    break  # left the section without a version line
                vm = _VERSION_LINE.match(line)
                if vm:
                    return vm.group(1)
    raise ValueError(
        f"no sovereign-protocol dependency with an explicit version found in "
        f"{cargo_toml_path}"
    )


# --- path resolution -------------------------------------------------------


def default_paths():
    """(ssot_path, [(label, consumer_path), ...]) for the standard layout."""
    sentinel_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    workspace = os.path.dirname(sentinel_root)
    ssot = os.path.join(sentinel_root, "sovereign-protocol", "Cargo.toml")
    consumers = [
        ("gateway", os.path.join(workspace, "gateway", "sidecar-rs", "Cargo.toml")),
        ("council-rs", os.path.join(workspace, "council-rs", "Cargo.toml")),
    ]
    return ssot, consumers


def main(argv):
    if len(argv) > 1:
        ssot_path = argv[1]
        consumers = [(os.path.basename(os.path.dirname(p)), p) for p in argv[2:]]
    else:
        ssot_path, consumers = default_paths()

    if not os.path.exists(ssot_path):
        print(f"FAIL: sovereign-protocol SSOT not found at {ssot_path}")
        return 1

    ssot = ssot_version(ssot_path)
    print(f"sovereign-protocol SSOT version: {ssot}  ({ssot_path})")

    checked = 0
    drift = []
    for label, path in consumers:
        if not os.path.exists(path):
            print(f"  SKIP {label}: not present ({path})")
            continue
        got = consumer_version(path)
        checked += 1
        if got == ssot:
            print(f"  OK   {label}: {got}")
        else:
            print(f"  DRIFT {label}: {got} != SSOT {ssot}  ({path})")
            drift.append((label, got))

    if drift:
        names = ", ".join(f"{lbl} pins {ver}" for lbl, ver in drift)
        print(
            f"\nFAIL: sovereign-protocol version drift — SSOT is {ssot} but "
            f"{names}. Bump every consumer's dependency in lockstep with the "
            f"crate version."
        )
        return 1

    if checked == 0:
        print(
            "\nNOTE: no consumers were present to check (sentinel-only checkout). "
            "Drift guard is a no-op here; it enforces when all workspace "
            "components are present."
        )
    else:
        print(f"\nPASS: {checked} consumer(s) aligned on sovereign-protocol {ssot}.")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
