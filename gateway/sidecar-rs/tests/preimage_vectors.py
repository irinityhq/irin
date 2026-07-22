#!/usr/bin/env python3
"""Phase 3 watch audit preimage corpus reference encoder.

This file is intentionally small and deterministic. It mirrors the Phase 2
watch_fires v3 text preimage format:

    len(tenant):tenant|len(sentinel):sentinel|...

Lengths are UTF-8 byte lengths. The NUL byte is not appended or counted.

W3 item 3 — version-tagged preimage. The `watch_fires.preimage_version`
column is the SELECTOR for the field set; it is NEVER itself a hashed field
(hashing the discriminator would be circular). Two versions exist:

  * v3 (`envelope_json = None`) — the original 6 fields, byte-for-byte
    unchanged. The 8 legacy canary rows (backfilled to preimage_version = 3)
    keep verifying with no rewrite.
  * v4 (`envelope_json = Some(bytes)`) — v3's 6 fields, then the VERBATIM
    stored envelope_json APPENDED AT END, length-prefixed (`|len:env`). An
    empty/NULL envelope encodes as `0:`. Because the envelope is appended, a
    v4 preimage over an empty envelope is still NOT equal to the v3 preimage:
    the version tag, not byte-equality, is the discriminator.

Mirrors `src/watch/db.rs::compute_watch_fire_preimage`.
"""

from __future__ import annotations

import hashlib
import json
from typing import Any


ANCHOR_PREV_HASH = "0ffed28740318eb8e9fa37cc3d034394c1eea87a273ecdbfcdcb937af502acba"
ANCHOR_HASH = "37b45ae77c065d081827a1bef87fe12d6e1771f16de25d11f8254f990c9dad09"
ANCHOR_FIRED_AT = 1747166531000

EVENT_TYPES = [
    "escalation_received",
    "escalation_replay_detected",
    "escalation_replay_terminal",
    "escalation_origin_invalid",
    "escalation_schema_rejected",
    "escalation_expired",
    "escalation_unknown_sentinel",
    "escalation_channel_dropped",
    "escalation_rate_limited",
    "escalation_dispatched",
    "escalation_recovered_pre_response",
    "escalation_recovered_response_intact",
    "escalation_recovered_resume_outbox",
    "escalation_failed",
    "dispatch_dead_lettered",
    "directive_received",
    "directive_parse_failed",
    "directive_correlation_failed",
    "directive_authority_rejected",
    "directive_tenant_mismatch",
    "directive_verdict_invalid",
    "directive_staged",
    "directive_dismissed",
    "directive_acked",
    "directive_expired_in_outbox",
    "outbox_recovered_from_restart",
    "escalation_recovery_max_iterations",
    "escalation_watchdog_wedged",
    "escalation_watchdog_recovered_response",
    "escalation_watchdog_drained_staged",
    "directive_cost_excessive",
    "directive_ack_tenant_mismatch",
    "escalation_unparseable_envelope",
    "directive_clock_skew_normalized",
]


def canonical_json(value: dict[str, Any]) -> str:
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def _field(value: str) -> str:
    return f"{len(value.encode('utf-8'))}:{value}"


def build_watch_preimage(
    tenant: str,
    sentinel: str,
    fired_at: int,
    state_json: str,
    reason: str,
    prev_hash: str,
) -> str:
    """v3 base: the original 6-field preimage. envelope_json = None."""
    fields = [tenant, sentinel, str(fired_at), state_json, reason, prev_hash]
    return "|".join(_field(field) for field in fields)


def build_watch_preimage_v4(
    tenant: str,
    sentinel: str,
    fired_at: int,
    state_json: str,
    reason: str,
    prev_hash: str,
    envelope_json: str,
) -> str:
    """v4: v3 base, then the verbatim envelope_json appended, length-prefixed.

    An empty envelope ("") encodes as the trailing "|0:" — still distinct from
    v3 by the preimage_version tag, never by byte-equality of the preimage.
    """
    base = build_watch_preimage(tenant, sentinel, fired_at, state_json, reason, prev_hash)
    return f"{base}|{_field(envelope_json)}"


def _hash(preimage: str) -> str:
    return hashlib.sha256(preimage.encode("utf-8")).hexdigest()


def _state_for(event_type: str, index: int) -> dict[str, Any]:
    if event_type == "escalation_received":
        return {
            "event_type": "escalation_received",
            "escalation_id": "deadbeefcafebabedeadbeefcafebabe",
        }

    if event_type == "directive_dismissed":
        return {
            "event_type": event_type,
            "directive_id": "dir-dismiss-fixture",
            "payload": {
                "schema": "irin.directive.payload.v1",
                "in_response_to": "esc-dismiss-fixture",
                "authority": "recommend",
                "verdict": "Dismiss",
                "rationale": "fixture dismissal",
                "tenant": "acme",
                "council_session_id": "fixture-session",
                "council_cost_usd": 0.01,
            },
        }

    if event_type == "directive_clock_skew_normalized":
        return {
            "event_type": event_type,
            "directive_id": "dir-skew-fixture",
            "tenant": "acme",
            "original_ms": ANCHOR_FIRED_AT + index,
            "normalized_ms": ANCHOR_FIRED_AT + index + 1,
        }

    if event_type == "outbox_recovered_from_restart":
        return {
            "event_type": event_type,
            "directive_id": "dir-recovered-fixture",
            "tenant": "acme",
            "in_response_to": "esc-recovered-fixture",
        }

    if event_type.startswith("directive_"):
        return {
            "event_type": event_type,
            "directive_id": f"dir-fixture-{index:02d}",
            "tenant": "acme",
            "fixture": "phase3-ac15c",
        }

    return {
        "event_type": event_type,
        "escalation_id": f"esc-fixture-{index:02d}",
        "tenant": "_dispatch_anon" if event_type == "escalation_unparseable_envelope" else "acme",
        "fixture": "phase3-ac15c",
    }


def vector_for(event_type: str, index: int) -> dict[str, Any]:
    state_json = canonical_json(_state_for(event_type, index))

    if event_type == "escalation_received":
        tenant = "acme"
        sentinel = "queue-depth-watch"
        fired_at = ANCHOR_FIRED_AT
        reason = "escalation_received: deadbeefcafebabedeadbeefcafebabe"
        prev_hash = ANCHOR_PREV_HASH
    else:
        tenant = "_dispatch_anon" if event_type == "escalation_unparseable_envelope" else "acme"
        sentinel = "queue-depth-watch"
        fired_at = ANCHOR_FIRED_AT + (index * 1000)
        reason = f"{event_type}: fixture"
        prev_hash = ANCHOR_HASH

    preimage = build_watch_preimage(tenant, sentinel, fired_at, state_json, reason, prev_hash)
    return {
        "event_type": event_type,
        "tenant": tenant,
        "sentinel": sentinel,
        "fired_at": fired_at,
        "state_json": state_json,
        "reason": reason,
        "prev_hash": prev_hash,
        # v3 corpus: the version SELECTOR is 3 and there is no appended
        # envelope. Recorded explicitly so the committed corpus carries the
        # field the W3 schema added; null === "no envelope === v3 field set".
        "preimage_version": 3,
        "envelope_json": None,
        "hash": _hash(preimage),
    }


# --- W3 item 3 reference vectors (v3 / v4 / version-tag-not-in-preimage) -------

# A single envelope used by the v4 demonstration vectors below. Verbatim bytes;
# the encoder hashes it exactly as stored (insert writes JCS-canonical, verify
# never re-canonicalizes).
W3_ENVELOPE = (
    '{"schema":"irin.comms.v0.1","kind":"directive",'
    '"in_response_to":"esc-w3-fixture","tenant":"acme"}'
)


def w3_reference_vectors() -> list[dict[str, Any]]:
    """v3 / v4 / empty-envelope-v4 vectors plus the version-tag invariant.

    These are NOT part of the spec audit-table corpus (proptest_preimage.rs
    consumes only the v3 corpus); they pin the W3 encoder so the Rust and
    Python sides cannot drift on the envelope-append mechanics.
    """
    tenant = "acme"
    sentinel = "queue-depth-watch"
    fired_at = ANCHOR_FIRED_AT
    state_json = canonical_json({"event_type": "directive_staged", "tenant": tenant})
    reason = "directive_staged: w3 reference"
    prev_hash = ANCHOR_HASH

    v3_preimage = build_watch_preimage(tenant, sentinel, fired_at, state_json, reason, prev_hash)
    v4_preimage = build_watch_preimage_v4(
        tenant, sentinel, fired_at, state_json, reason, prev_hash, W3_ENVELOPE
    )
    v4_empty_preimage = build_watch_preimage_v4(
        tenant, sentinel, fired_at, state_json, reason, prev_hash, ""
    )

    # Invariant 1: the v4 preimage is exactly the v3 preimage with the
    # length-prefixed envelope appended — nothing rewritten in the base.
    assert v4_preimage == f"{v3_preimage}|{_field(W3_ENVELOPE)}", "v4 must be v3 + appended envelope"

    # Invariant 2: a v4-over-empty-envelope preimage is NOT equal to the v3
    # preimage. v3 vs v4 is decided by the version tag, never byte-equality.
    assert v4_empty_preimage != v3_preimage, "empty-envelope v4 must differ from v3 (trailing |0:)"
    assert v4_empty_preimage.endswith("|0:"), "empty envelope must encode as trailing |0:"

    # Invariant 3: the preimage_version SELECTOR never appears in the hashed
    # bytes. Hashing the discriminator would be circular; assert neither the
    # literal "3"/"4" tag nor a "preimage_version" token leaks into either
    # preimage. (We check the structural marker; the digits 3/4 legitimately
    # appear inside length prefixes, so we assert the token, not the char.)
    for label, pre in (("v3", v3_preimage), ("v4", v4_preimage)):
        assert "preimage_version" not in pre, f"{label} preimage must not embed the version selector"

    return [
        {
            "label": "w3_v3",
            "preimage_version": 3,
            "envelope_json": None,
            "preimage": v3_preimage,
            "hash": _hash(v3_preimage),
        },
        {
            "label": "w3_v4",
            "preimage_version": 4,
            "envelope_json": W3_ENVELOPE,
            "preimage": v4_preimage,
            "hash": _hash(v4_preimage),
        },
        {
            "label": "w3_v4_empty_envelope",
            "preimage_version": 4,
            "envelope_json": "",
            "preimage": v4_empty_preimage,
            "hash": _hash(v4_empty_preimage),
        },
    ]


def corpus() -> list[dict[str, Any]]:
    vectors = [vector_for(event_type, index) for index, event_type in enumerate(EVENT_TYPES)]
    anchor = vectors[0]
    if anchor["hash"] != ANCHOR_HASH:
        raise AssertionError(f"escalation_received anchor drifted: {anchor['hash']}")
    return vectors


if __name__ == "__main__":
    # corpus() raises if the v3 anchor drifts; w3_reference_vectors() raises if
    # any of the three version-tag invariants break. Running both as part of
    # __main__ makes `python3 tests/preimage_vectors.py` a self-checking encoder.
    out = {
        "v3_corpus": corpus(),
        "w3_reference_vectors": w3_reference_vectors(),
    }
    print(json.dumps(out, indent=2, sort_keys=False))
