#!/usr/bin/env python3
"""Normal-seat model shootout — intelligence / judgment quality, not speed.

Purpose
-------
Compare candidates for *deliberation seats* by the quality of their answers
to fixed judgment prompts. Latency and pass-rate are diagnostics only.

What went wrong in the first harness (2026-07-13):
  - Leaderboard sorted by pass rate + mean latency.
  - Local Grok Build seats used --max-turns 1; 4.5 tried to ground in-repo
    then hit "Max turns reached" → false FAIL. Hermes chat seats always
    returned text. That was a process bug, not an intelligence verdict.

Winner is NEVER auto-declared. Human reads full answers + rubric scores.

Examples:
  python3 scripts/seat-model-shootout.py \\
    --candidates grok-4.3,grok-4.20-0309-reasoning,grok-4.5,grok-composer-2.5-fast
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
COUNCIL = ROOT / "target" / "release" / "council"

# Judgment prompts — no trivial ACK. Scored for intelligence of outcome.
DEFAULT_PROMPTS: list[dict[str, str]] = [
    {
        "id": "risk_sentence",
        "prompt": (
            "In exactly one sentence, state the main risk of shipping unproven "
            "multi-host recovery claims as if they were production-ready."
        ),
        "expect": "one_sentence_risk",
    },
    {
        "id": "next_actions",
        "prompt": (
            "List exactly three private-only technical next actions after single-host "
            "IRIN confidence is green. Number them 1-3. Prefer concrete, checkable work "
            "over slogans."
        ),
        "expect": "three_numbered_actions",
    },
    {
        "id": "tradeoff",
        "prompt": (
            "A team wants to pin every Grok cabinet seat to the newest local Build model "
            "because it is 'smarter'. In 3-5 sentences, argue for OR against that move "
            "for normal deliberation seats (not Sheldon). Name the strongest counter-risk."
        ),
        "expect": "tradeoff_judgment",
    },
]


@dataclass
class Rubric:
    """Heuristic quality scores 0-2 each. Not a substitute for human judgment."""

    constraint_fit: int = 0  # followed format / length constraints
    specificity: int = 0  # concrete technical claims vs vague
    risk_clarity: int = 0  # names failure mode / blast radius
    actionability: int = 0  # next steps or decision criteria checkable
    notes: str = ""

    @property
    def total(self) -> int:
        return self.constraint_fit + self.specificity + self.risk_clarity + self.actionability


@dataclass
class Trial:
    candidate: str
    provider: str
    prompt_id: str
    ok: bool
    latency_s: float
    model_label: str
    full_text: str
    text_preview: str
    error: str
    exit_code: int
    rubric: Rubric = field(default_factory=Rubric)


def score_answer(expect: str, text: str, ok: bool) -> Rubric:
    if not ok or not text.strip():
        return Rubric(notes="no usable answer (harness/transport failure — not scored as dumb)")

    t = text.strip()
    lower = t.lower()
    r = Rubric()

    # Specificity: technical anchors common in IRIN recovery context
    anchors = (
        "docker",
        "compose",
        "digest",
        "host",
        "vm",
        "backup",
        "restore",
        "verify",
        "smoke",
        "cabinet",
        "hermes",
        "cli",
        "oauth",
        "api",
        "secondary",
        "primary",
        "outage",
        "failover",
        "pin",
        "max-turns",
        "transport",
        "seat",
        "build",
        "agent",
    )
    hit = sum(1 for a in anchors if a in lower)
    r.specificity = 2 if hit >= 3 else (1 if hit >= 1 else 0)

    risk_words = (
        "risk",
        "fail",
        "false",
        "unproven",
        "downtime",
        "data loss",
        "silent",
        "confidence",
        "outage",
        "unrecover",
        "blast",
        "break",
    )
    r.risk_clarity = 2 if sum(1 for w in risk_words if w in lower) >= 2 else (
        1 if any(w in lower for w in risk_words) else 0
    )

    if expect == "one_sentence_risk":
        sentences = [s for s in re.split(r"[.!?]\s+", t) if s.strip()]
        # one primary sentence preferred; allow trailing fragment
        r.constraint_fit = 2 if len(sentences) <= 2 and len(t) < 600 else (
            1 if len(t) < 900 else 0
        )
        r.actionability = 1 if any(x in lower for x in ("when", "if", "before", "without")) else 0
        if "risk" in lower or "fail" in lower:
            r.actionability = min(2, r.actionability + 1)
    elif expect == "three_numbered_actions":
        nums = re.findall(r"(?m)^\s*(?:\d+[\).\]]|\-\s+\*\*)", t)
        if not nums:
            nums = re.findall(r"\b[123][\).\]]\s", t)
        r.constraint_fit = 2 if len(nums) >= 3 else (1 if len(nums) >= 2 else 0)
        verbs = ("run", "pin", "verify", "test", "build", "document", "smoke", "restore", "add", "wire")
        r.actionability = 2 if sum(1 for v in verbs if v in lower) >= 3 else (
            1 if any(v in lower for v in verbs) else 0
        )
    elif expect == "tradeoff_judgment":
        r.constraint_fit = 2 if 2 <= len(re.split(r"[.!?]\s+", t)) <= 8 else 1
        counter = any(
            x in lower
            for x in (
                "however",
                "but",
                "risk",
                "unless",
                "counter",
                "trade",
                "cost",
                "reliability",
                "harness",
                "transport",
            )
        )
        r.actionability = 2 if counter else 0
        r.risk_clarity = max(r.risk_clarity, 1 if counter else 0)

    r.notes = f"heuristic total={r.total}/8 (human still ranks intelligence)"
    return r


def run_trial(
    provider: str, model: str, prompt_meta: dict[str, str], timeout: int
) -> Trial:
    prompt = prompt_meta["prompt"]
    prompt_id = prompt_meta["id"]
    cmd = [
        str(COUNCIL),
        "--base-dir",
        str(ROOT / "council-rs"),
        "--smoke-provider",
        provider,
        "--smoke-model",
        model,
        prompt,
    ]
    env = os.environ.copy()
    env["COUNCIL_VIA_GATEWAY"] = "0"
    t0 = time.time()
    try:
        proc = subprocess.run(
            cmd,
            cwd=str(ROOT),
            env=env,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return Trial(
            candidate=model,
            provider=provider,
            prompt_id=prompt_id,
            ok=False,
            latency_s=time.time() - t0,
            model_label="",
            full_text="",
            text_preview="",
            error=f"timeout after {timeout}s",
            exit_code=-1,
            rubric=Rubric(notes="timeout — transport/harness, not scored"),
        )
    latency = time.time() - t0
    out = (proc.stdout or "") + "\n" + (proc.stderr or "")
    model_label = ""
    for line in out.splitlines():
        if "model=" in line and ("✅" in line or "ms |" in line):
            for part in line.split("|"):
                part = part.strip()
                if part.startswith("model="):
                    model_label = part[len("model=") :].strip()
        if "↪ hermes" in line.lower() or "hermes-cli" in line:
            model_label = model_label or "hermes-path"
    text = (proc.stdout or "").strip()
    err = ""
    ok = proc.returncode == 0 and bool(text) and "Error:" not in (proc.stderr or "")
    if "Max turns reached" in out or "max turns reached" in out.lower():
        ok = False
        err = "Max turns reached (agent budget — not an intelligence score)"
    if not ok and not err:
        for line in out.splitlines():
            if "Error" in line or "❌" in line or "error" in line.lower():
                err = line.strip()[:200]
                break
        if not err:
            err = (proc.stderr or proc.stdout or "failed")[-200:]
    preview = " ".join(text.split())[:160]
    rubric = score_answer(prompt_meta["expect"], text, ok)
    return Trial(
        candidate=model,
        provider=provider,
        prompt_id=prompt_id,
        ok=ok,
        latency_s=latency,
        model_label=model_label,
        full_text=text,
        text_preview=preview,
        error=err,
        exit_code=proc.returncode,
        rubric=rubric,
    )


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--provider", default="grok_cli")
    ap.add_argument(
        "--candidates",
        default="grok-4.3,grok-4.20-0309-reasoning,grok-4.5,grok-composer-2.5-fast",
        help="Comma-separated model ids for this provider",
    )
    ap.add_argument("--timeout", type=int, default=180)
    ap.add_argument("--report", type=Path, default=None)
    args = ap.parse_args()

    if not COUNCIL.is_file():
        print(
            f"missing {COUNCIL}; run: cargo build --release -p council-rs --bin council",
            file=sys.stderr,
        )
        return 2

    candidates = [c.strip() for c in args.candidates.split(",") if c.strip()]
    trials: list[Trial] = []
    print(
        f"QUALITY shootout (not speed) provider={args.provider} "
        f"candidates={candidates} prompts={len(DEFAULT_PROMPTS)}"
    )

    for model in candidates:
        for meta in DEFAULT_PROMPTS:
            print(f"→ {model} [{meta['id']}] …", flush=True)
            t = run_trial(args.provider, model, meta, args.timeout)
            trials.append(t)
            status = "OK" if t.ok else "FAIL"
            print(
                f"  {status} rubric={t.rubric.total}/8 {t.latency_s:.1f}s "
                f"label={t.model_label!r} {t.text_preview[:70]!r} {t.error[:80]}"
            )

    by: dict[str, list[Trial]] = {}
    for t in trials:
        by.setdefault(t.candidate, []).append(t)

    lines: list[str] = [
        "# Seat model shootout — **judgment quality** (not speed)\n\n",
        f"Date: {datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%MZ')}\n",
        f"Provider: `{args.provider}`\n",
        f"Council: `{COUNCIL}`\n\n",
        "## Method\n\n",
        "- Metric: quality of answers to fixed judgment prompts.\n",
        "- Latency / pass-rate: diagnostics only (transport health).\n",
        "- Heuristic rubric (0–2 each): constraint_fit, specificity, "
        "risk_clarity, actionability → total /8. **Human ranks intelligence.**\n",
        "- Winner is **TBD** — do not auto-promote cabinets from this file alone.\n\n",
        "## Diagnostic board (health, not champion)\n\n",
        "| Candidate | usable | n | heuristic Σ | mean_s | labels |\n",
        "|---|---:|---:|---:|---:|---|\n",
    ]

    rows = []
    for cand, ts in by.items():
        n = len(ts)
        usable = sum(1 for t in ts if t.ok)
        hsum = sum(t.rubric.total for t in ts)
        mean = sum(t.latency_s for t in ts) / n if n else 0
        labels = sorted({t.model_label for t in ts if t.model_label})
        # sort key for display: higher heuristic, then more usable — NOT faster
        rows.append((hsum, usable, cand, n, mean, labels))
    rows.sort(reverse=True)
    for hsum, usable, cand, n, mean, labels in rows:
        lines.append(
            f"| `{cand}` | {usable}/{n} | {n} | {hsum} | {mean:.1f} | "
            f"{', '.join(f'`{x}`' for x in labels) or '—'} |\n"
        )

    lines.append(
        "\n**Champion (intelligence): TBD — human** after reading full answers below.\n"
        "Heuristic Σ is a weak proxy; never promote on latency or pass-rate alone.\n\n"
    )

    lines.append("## Full answers (rank these)\n\n")
    for meta in DEFAULT_PROMPTS:
        lines.append(f"### Prompt `{meta['id']}`\n\n")
        lines.append(f"> {meta['prompt']}\n\n")
        for cand, ts in by.items():
            match = next((t for t in ts if t.prompt_id == meta["id"]), None)
            if not match:
                continue
            lines.append(
                f"#### `{cand}` — rubric {match.rubric.total}/8 "
                f"(fit={match.rubric.constraint_fit} spec={match.rubric.specificity} "
                f"risk={match.rubric.risk_clarity} act={match.rubric.actionability}) "
                f"| {match.latency_s:.1f}s | `{match.model_label or '—'}`\n\n"
            )
            if match.ok and match.full_text:
                lines.append("```\n")
                lines.append(match.full_text.rstrip() + "\n")
                lines.append("```\n\n")
            else:
                lines.append(f"*No answer — {match.error or 'failed'}*\n\n")

    lines.append("## Machine trials JSON\n\n```json\n")
    payload = []
    for t in trials:
        d = asdict(t)
        payload.append(d)
    lines.append(json.dumps(payload, indent=2))
    lines.append("\n```\n")

    report = args.report
    if report is None:
        receipt_dir = Path.home() / "Projects/maxaccel/reviews/receipts"
        if receipt_dir.is_dir():
            report = (
                receipt_dir
                / f"IRIN-SEAT-SHOOTOUT-QUALITY-{datetime.now(timezone.utc).strftime('%Y%m%dT%H%MZ')}.md"
            )
        else:
            report = ROOT / "runs" / f"seat-shootout-quality-{int(time.time())}.md"
    report.parent.mkdir(parents=True, exist_ok=True)
    report.write_text("".join(lines))
    print(f"\nwrote {report}")
    print("Champion not declared — rank full answers for judgment quality.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
