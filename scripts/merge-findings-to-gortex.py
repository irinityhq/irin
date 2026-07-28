#!/usr/bin/env python3
"""merge-findings-to-gortex.py — SARIF → JSONL findings with Gortex symbol map.

Phase 1F of the tier-1 security spine: join static-analysis findings
(Opengrep and any other SARIF producer) to Gortex symbol IDs so agents can
query findings against the graph spine.

Usage
-----
  python3 scripts/merge-findings-to-gortex.py path/to/findings.sarif
  python3 scripts/merge-findings-to-gortex.py path/to/findings.sarif \\
      --out .irin-tools/findings/merged.jsonl
  python3 scripts/merge-findings-to-gortex.py path/to/findings.sarif --no-gortex
  python3 scripts/merge-findings-to-gortex.py path/to/findings.sarif --tool codeql

Defaults
--------
  --out  .irin-tools/findings/merged-<UTC-ts>.jsonl  (gitignored via .irin-tools/)

JSONL row schema
----------------
  {
    "tool": "opengrep",
    "rule_id": str,
    "severity": str,          # SARIF level: error|warning|note|none|…
    "path": str,               # repo-relative
    "start_line": int,
    "end_line": int|null,
    "message": str,
    "symbol_id": str|null,
    "symbol_name": str|null,
    "run_id": str,
    "ts": str                  # ISO-8601 UTC
  }

Gortex symbol resolution (best-effort, offline CLI)
---------------------------------------------------
Requires `gortex` on PATH and a daemon that tracks this worktree
(`scripts/gortex-worktree.sh doctor` / `gortex track <path> --as-worktree`).

Resolution order per finding (file, line):

  1. Prefer `gortex call symbols_for_ranges` when the daemon mounts it
     (single-file form: path + start_line + end_line). As of Gortex 0.61.x
     the tool is catalogued but often not call-mounted; failure is ignored.
  2. Fallback: `gortex call search_symbols` with the file basename, filter
     results to the same relative path, then `gortex node <id> -f json` to
     obtain start_line/end_line. Pick the tightest *enclosing* symbol
     (start_line <= line <= end_line). Never assign a non-enclosing
     "nearest" symbol — prefer null over wrong attribution.
  3. On any failure: emit the finding with symbol_id/symbol_name null.
     Path and line are always populated; the merger must not crash.

Python 3 stdlib only. No pip deps.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import uuid
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import unquote, urlparse

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_FINDINGS_DIR = ROOT / ".irin-tools" / "findings"

# ---------------------------------------------------------------------------
# SARIF parse
# ---------------------------------------------------------------------------


def _now_ts() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _now_file_ts() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def _message_text(msg: Any) -> str:
    if msg is None:
        return ""
    if isinstance(msg, str):
        return msg
    if isinstance(msg, dict):
        text = msg.get("text")
        if isinstance(text, str):
            return text
        markdown = msg.get("markdown")
        if isinstance(markdown, str):
            return markdown
    return str(msg)


def _uri_to_path(uri: str | None, repo_root: Path) -> str | None:
    """Normalize a SARIF artifact URI to a repo-relative POSIX path."""
    if not uri:
        return None
    raw = unquote(uri.strip())
    if raw.startswith("file:"):
        parsed = urlparse(raw)
        # file:///abs/path or file://localhost/abs/path
        path = unquote(parsed.path or "")
        if sys.platform == "darwin" or sys.platform.startswith("linux"):
            # On Unix, urlparse('file:///Users/...') -> path='/Users/...'
            pass
        elif sys.platform == "win32" and path.startswith("/"):
            # file:///C:/... -> /C:/... strip leading slash for drive paths
            if len(path) >= 3 and path[2] == ":":
                path = path[1:]
        raw = path
    p = Path(raw)
    try:
        if p.is_absolute():
            rel = p.resolve().relative_to(repo_root.resolve())
            return rel.as_posix()
    except (ValueError, OSError):
        # Not under repo_root — fall through and strip known prefixes.
        pass
    # Already relative, or absolute outside repo: strip leading ./ and repo name.
    s = raw.replace("\\", "/")
    while s.startswith("./"):
        s = s[2:]
    if s.startswith("/"):
        # Last-resort: if the absolute path contains the repo root string, slice.
        root_s = str(repo_root.resolve()).replace("\\", "/")
        if s.startswith(root_s + "/"):
            return s[len(root_s) + 1 :]
        return s.lstrip("/")
    return s


def _base_uri_map(run: dict[str, Any]) -> dict[str, str]:
    """Map SARIF uriBaseId → base URI string from originalUriBaseIds."""
    out: dict[str, str] = {}
    bases = run.get("originalUriBaseIds") or {}
    if not isinstance(bases, dict):
        return out
    for key, val in bases.items():
        if not isinstance(key, str):
            continue
        if isinstance(val, str) and val:
            out[key] = val
            continue
        if isinstance(val, dict):
            uri = val.get("uri")
            if isinstance(uri, str) and uri:
                out[key] = uri
    return out


def _resolve_artifact_uri(
    artifact: dict[str, Any],
    base_uris: dict[str, str],
) -> str | None:
    """Combine uri / uriBaseId per SARIF 2.1.0; never treat base id as a path."""
    uri = artifact.get("uri")
    base_id = artifact.get("uriBaseId")
    uri_s = uri if isinstance(uri, str) and uri else None
    base_id_s = base_id if isinstance(base_id, str) and base_id else None

    if uri_s and not base_id_s:
        return uri_s

    if base_id_s:
        base = base_uris.get(base_id_s)
        if not base:
            # Unknown base: only usable if uri is already absolute/file.
            if uri_s and (uri_s.startswith("file:") or uri_s.startswith("/")):
                return uri_s
            return None
        if not uri_s:
            return base
        # Join base + relative uri (avoid double slashes).
        if uri_s.startswith("file:") or uri_s.startswith("/"):
            return uri_s
        if base.endswith("/") or base.endswith("\\"):
            return base + uri_s
        return base + "/" + uri_s

    return uri_s


def parse_sarif(sarif_path: Path, repo_root: Path) -> list[dict[str, Any]]:
    """Extract flat findings from a SARIF 2.1.0 document."""
    data = json.loads(sarif_path.read_text(encoding="utf-8"))
    findings: list[dict[str, Any]] = []
    for run in data.get("runs") or []:
        if not isinstance(run, dict):
            continue
        base_uris = _base_uri_map(run)
        for result in run.get("results") or []:
            if not isinstance(result, dict):
                continue
            rule_id = result.get("ruleId") or result.get("rule", {}).get("id") or "unknown"
            level = (result.get("level") or "warning").lower()
            message = _message_text(result.get("message"))
            locations = result.get("locations") or []
            if not locations:
                findings.append(
                    {
                        "rule_id": rule_id,
                        "severity": level,
                        "path": None,
                        "start_line": None,
                        "end_line": None,
                        "start_column": None,
                        "message": message,
                    }
                )
                continue
            for loc in locations:
                phys = (loc or {}).get("physicalLocation") or {}
                artifact = phys.get("artifactLocation") or {}
                if not isinstance(artifact, dict):
                    artifact = {}
                region = phys.get("region") or {}
                resolved_uri = _resolve_artifact_uri(artifact, base_uris)
                path = _uri_to_path(resolved_uri, repo_root)
                start_line = region.get("startLine")
                end_line = region.get("endLine") or start_line
                start_col = region.get("startColumn")
                findings.append(
                    {
                        "rule_id": rule_id,
                        "severity": level,
                        "path": path,
                        "start_line": int(start_line) if start_line is not None else None,
                        "end_line": int(end_line) if end_line is not None else None,
                        "start_column": int(start_col) if start_col is not None else None,
                        "message": message,
                    }
                )
    return findings


# ---------------------------------------------------------------------------
# Gortex resolution
# ---------------------------------------------------------------------------


class GortexResolver:
    """Best-effort file:line → enclosing symbol via the Gortex CLI."""

    def __init__(
        self,
        repo_root: Path,
        gortex_bin: str = "gortex",
        timeout_s: float = 15.0,
        enabled: bool = True,
    ) -> None:
        self.repo_root = repo_root
        self.gortex_bin = gortex_bin
        self.timeout_s = timeout_s
        self.enabled = enabled
        self._file_symbols: dict[str, list[dict[str, Any]]] = {}
        self._node_cache: dict[str, dict[str, Any] | None] = {}
        self._available: bool | None = None
        self._ranges_supported: bool | None = None
        self.stats = Counter()

    def _run(self, args: list[str]) -> tuple[int, str, str]:
        try:
            proc = subprocess.run(
                [self.gortex_bin, *args],
                cwd=str(self.repo_root),
                capture_output=True,
                text=True,
                timeout=self.timeout_s,
                env={**os.environ, "NO_COLOR": "1", "TERM": "dumb"},
            )
            return proc.returncode, proc.stdout or "", proc.stderr or ""
        except FileNotFoundError:
            return 127, "", "gortex not found on PATH"
        except subprocess.TimeoutExpired:
            return 124, "", "gortex timed out"

    def available(self) -> bool:
        if not self.enabled:
            return False
        if self._available is not None:
            return self._available
        code, out, _ = self._run(["version"])
        self._available = code == 0 and bool(out.strip())
        if not self._available:
            self.stats["gortex_unavailable"] += 1
        return self._available

    def _call_json(self, tool: str, payload: dict[str, Any]) -> Any | None:
        code, out, err = self._run(
            [
                "call",
                tool,
                "--index",
                str(self.repo_root),
                "--json",
                json.dumps(payload),
                "--format",
                "json",
            ]
        )
        if code != 0:
            self.stats["call_fail"] += 1
            return None
        text = out.strip()
        if not text:
            return None
        try:
            return json.loads(text)
        except json.JSONDecodeError:
            # Some tools may print a banner line; take the last JSON object.
            for line in reversed(text.splitlines()):
                line = line.strip()
                if line.startswith("{") or line.startswith("["):
                    try:
                        return json.loads(line)
                    except json.JSONDecodeError:
                        continue
            self.stats["json_parse_fail"] += 1
            return None

    def _symbols_for_ranges(self, path: str, start_line: int, end_line: int) -> dict[str, str] | None:
        if self._ranges_supported is False:
            return None
        data = self._call_json(
            "symbols_for_ranges",
            {
                "path": path,
                "start_line": start_line,
                "end_line": end_line,
            },
        )
        if data is None:
            # Probe once; subsequent findings skip this path if unsupported.
            if self._ranges_supported is None:
                # Distinguish "not found" from empty result via a dry call.
                code, _, err = self._run(
                    [
                        "call",
                        "symbols_for_ranges",
                        "--index",
                        str(self.repo_root),
                        "--arg",
                        f"path={path}",
                        "--arg",
                        f"start_line={start_line}",
                        "--format",
                        "json",
                    ]
                )
                if code != 0 and "not found" in (err or "").lower():
                    self._ranges_supported = False
                    self.stats["ranges_unsupported"] += 1
                    return None
                self._ranges_supported = True
            return None
        self._ranges_supported = True
        # Accept a few possible response shapes.
        symbols = (
            data.get("symbols")
            if isinstance(data, dict)
            else data
            if isinstance(data, list)
            else None
        )
        if not symbols:
            # Single-symbol object?
            if isinstance(data, dict) and data.get("id"):
                return {
                    "symbol_id": str(data["id"]),
                    "symbol_name": str(data.get("name") or data["id"].split("::")[-1]),
                }
            return None
        # Prefer tightest (last) symbol if ordered; else first.
        best = None
        for s in symbols:
            if not isinstance(s, dict):
                continue
            sid = s.get("id") or s.get("symbol_id")
            if not sid:
                continue
            name = s.get("name") or s.get("symbol_name") or str(sid).split("::")[-1]
            start = s.get("start_line") or 0
            if best is None or start >= best[0]:
                best = (start, str(sid), str(name))
        if best is None:
            return None
        return {"symbol_id": best[1], "symbol_name": best[2]}

    def _path_matches(self, candidate_path: str, target: str) -> bool:
        c = candidate_path.replace("\\", "/")
        t = target.replace("\\", "/")
        if c == t or c.endswith("/" + t) or c.endswith(t):
            return True
        # Strip repo-prefix/ alias: "repo@branch/path" → "path"
        if "/" in c:
            # Drop first segment if it looks like a repo prefix.
            parts = c.split("/", 1)
            if len(parts) == 2 and ("@" in parts[0] or parts[0] in ("irin", "xmcp")):
                c2 = parts[1]
                if c2 == t or c2.endswith("/" + t):
                    return True
        return False

    def _list_file_symbols(self, path: str) -> list[dict[str, Any]]:
        if path in self._file_symbols:
            return self._file_symbols[path]
        basename = Path(path).name
        # Query by basename; filter client-side to the exact relative path.
        data = self._call_json(
            "search_symbols",
            {"query": basename, "limit": 200},
        )
        rows: list[dict[str, Any]] = []
        results = []
        if isinstance(data, dict):
            results = data.get("results") or []
        elif isinstance(data, list):
            results = data
        for r in results:
            if not isinstance(r, dict):
                continue
            fp = r.get("file_path") or r.get("absolute_file_path") or r.get("path") or ""
            if not self._path_matches(str(fp), path):
                continue
            sid = r.get("id")
            if not sid:
                continue
            start = r.get("start_line")
            if start is None:
                continue
            rows.append(
                {
                    "id": str(sid),
                    "name": str(r.get("name") or str(sid).split("::")[-1]),
                    "start_line": int(start),
                    "kind": r.get("kind"),
                }
            )
        rows.sort(key=lambda x: x["start_line"])
        self._file_symbols[path] = rows
        self.stats["file_symbol_lists"] += 1
        self.stats["file_symbols_cached"] += len(rows)
        return rows

    def _node(self, symbol_id: str) -> dict[str, Any] | None:
        if symbol_id in self._node_cache:
            return self._node_cache[symbol_id]
        code, out, _ = self._run(
            [
                "node",
                symbol_id,
                "--index",
                str(self.repo_root),
                "-f",
                "json",
            ]
        )
        if code != 0 or not out.strip():
            self._node_cache[symbol_id] = None
            self.stats["node_fail"] += 1
            return None
        try:
            data = json.loads(out.strip())
        except json.JSONDecodeError:
            self._node_cache[symbol_id] = None
            self.stats["node_fail"] += 1
            return None
        self._node_cache[symbol_id] = data if isinstance(data, dict) else None
        return self._node_cache[symbol_id]

    def resolve(self, path: str | None, start_line: int | None, end_line: int | None) -> dict[str, str | None]:
        empty = {"symbol_id": None, "symbol_name": None}
        if not path or start_line is None:
            self.stats["skip_no_location"] += 1
            return empty
        if not self.available():
            self.stats["unmapped"] += 1
            return empty

        line = int(start_line)
        end = int(end_line) if end_line is not None else line

        # 1) symbols_for_ranges (when mounted)
        hit = self._symbols_for_ranges(path, line, end)
        if hit:
            self.stats["mapped_ranges"] += 1
            self.stats["mapped"] += 1
            return hit

        # 2) search_symbols + node end_line
        candidates = self._list_file_symbols(path)
        if not candidates:
            self.stats["unmapped"] += 1
            return empty

        # Innermost-first: candidates with start_line <= line, descending start.
        prior = [c for c in candidates if c["start_line"] <= line]
        prior.sort(key=lambda c: c["start_line"], reverse=True)
        for c in prior:
            node = self._node(c["id"])
            if not node:
                continue
            n_start = node.get("start_line", c["start_line"])
            n_end = node.get("end_line")
            try:
                n_start_i = int(n_start)
            except (TypeError, ValueError):
                n_start_i = c["start_line"]
            try:
                n_end_i = int(n_end) if n_end is not None else n_start_i
            except (TypeError, ValueError):
                n_end_i = n_start_i
            if n_start_i <= line <= n_end_i:
                self.stats["mapped_search"] += 1
                self.stats["mapped"] += 1
                name = node.get("name") or c["name"]
                return {"symbol_id": str(node.get("id") or c["id"]), "symbol_name": str(name)}

        # Prefer null over attributing a finding to a non-enclosing symbol.
        self.stats["unmapped"] += 1
        return empty


# ---------------------------------------------------------------------------
# Merge + output
# ---------------------------------------------------------------------------


def merge(
    findings: list[dict[str, Any]],
    resolver: GortexResolver,
    tool: str,
    run_id: str,
    ts: str,
) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for f in findings:
        sym = resolver.resolve(f.get("path"), f.get("start_line"), f.get("end_line"))
        rows.append(
            {
                "tool": tool,
                "rule_id": f.get("rule_id") or "unknown",
                "severity": f.get("severity") or "warning",
                "path": f.get("path"),
                "start_line": f.get("start_line"),
                "end_line": f.get("end_line"),
                "message": f.get("message") or "",
                "symbol_id": sym["symbol_id"],
                "symbol_name": sym["symbol_name"],
                "run_id": run_id,
                "ts": ts,
            }
        )
    return rows


def write_jsonl(path: Path, rows: list[dict[str, Any]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n")


def print_summary(rows: list[dict[str, Any]], resolver: GortexResolver, out_path: Path) -> None:
    by_sev = Counter(r.get("severity") or "unknown" for r in rows)
    mapped = sum(1 for r in rows if r.get("symbol_id"))
    unmapped = len(rows) - mapped
    print(f"wrote {len(rows)} findings → {out_path}")
    print("severity:")
    for sev, n in sorted(by_sev.items(), key=lambda kv: (-kv[1], kv[0])):
        print(f"  {sev}: {n}")
    print(f"symbol map: {mapped} mapped, {unmapped} unmapped")
    if resolver.stats:
        detail = ", ".join(f"{k}={v}" for k, v in sorted(resolver.stats.items()))
        print(f"resolver: {detail}")


def build_arg_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        description="Merge SARIF findings into JSONL rows joined to Gortex symbols.",
    )
    p.add_argument("sarif", type=Path, help="Input SARIF file path")
    p.add_argument(
        "--out",
        type=Path,
        default=None,
        help="Output JSONL path (default: .irin-tools/findings/merged-<ts>.jsonl)",
    )
    p.add_argument(
        "--tool",
        default="opengrep",
        help="Tool name stamped on each row (default: opengrep)",
    )
    p.add_argument(
        "--repo",
        type=Path,
        default=ROOT,
        help=f"Repository root for path normalization + gortex --index (default: {ROOT})",
    )
    p.add_argument(
        "--no-gortex",
        action="store_true",
        help="Skip Gortex symbol resolution (always emit symbol_id null)",
    )
    p.add_argument(
        "--gortex-bin",
        default=os.environ.get("GORTEX_BIN", "gortex"),
        help="Gortex CLI binary (default: gortex or $GORTEX_BIN)",
    )
    p.add_argument(
        "--run-id",
        default=None,
        help="Run id stamped on rows (default: random UUID)",
    )
    p.add_argument(
        "--timeout",
        type=float,
        default=15.0,
        help="Per-gortex-invocation timeout seconds (default: 15)",
    )
    return p


def main(argv: list[str] | None = None) -> int:
    args = build_arg_parser().parse_args(argv)
    sarif_path = args.sarif.expanduser().resolve()
    if not sarif_path.is_file():
        print(f"error: SARIF not found: {sarif_path}", file=sys.stderr)
        return 2

    repo_root = args.repo.expanduser().resolve()
    out_path = (
        args.out.expanduser().resolve()
        if args.out
        else (DEFAULT_FINDINGS_DIR / f"merged-{_now_file_ts()}.jsonl").resolve()
    )

    try:
        findings = parse_sarif(sarif_path, repo_root)
    except (OSError, json.JSONDecodeError, ValueError) as exc:
        print(f"error: failed to parse SARIF: {exc}", file=sys.stderr)
        return 2

    if not findings:
        print("warning: no results in SARIF", file=sys.stderr)

    run_id = args.run_id or str(uuid.uuid4())
    ts = _now_ts()
    resolver = GortexResolver(
        repo_root=repo_root,
        gortex_bin=args.gortex_bin,
        timeout_s=args.timeout,
        enabled=not args.no_gortex,
    )
    rows = merge(findings, resolver, tool=args.tool, run_id=run_id, ts=ts)
    write_jsonl(out_path, rows)
    print_summary(rows, resolver, out_path)
    return 0


if __name__ == "__main__":
    sys.exit(main())
