---
title: IRIN Comms Contract
type: adr-contract
status: v0.1
date: 2026-05-07
adopts: sentinel, council-rs, gateway, sovereign-librarian
supersedes: placeholder-open-shape
superseded_by: null
---

# IRIN Comms Contract v0.1

**North Star:** Watch is cheap. Thought is rare. Action is final. Every message must prove why the next tier earned attention.

## Decision

The four adopting products are **Sentinel**, **Council**, **Gateway**, and **Librarian**.

`Worker` is not a fifth product. A Worker is an ephemeral execution role created by a Directive and killed by its boundary. `Sovereign` is not a product either; the Sovereign is the human authority each tenant scope serves.

v0.1 locks the smallest shared shape. Domain payloads remain product-owned and
additive; the contract spine does not.

## Message Spine

Every cross-product message carries the same minimal spine (CloudEvents 1.0 profile):

| Field | Meaning |
|---|---|
| `id` | (CloudEvents) stable message id for audit correlation |
| `source` | (CloudEvents) URI of the sender |
| `type` | (CloudEvents) `irin.escalation.v0.1` or `irin.directive.v0.1` |
| `time` | (CloudEvents) timestamp of emission |
| `data.contract` | `irin.comms.v0.1` |
| `data.kind` | `Escalation` or `Directive` (mirrors `type`) |
| `data.tenant` | one Sovereign scope; no cross-tenant implication |
| `data.ttl_seconds` | when the message expires if ignored |
| `data.budget_hint` | maximum attention/cost the sender believes is justified |
| `data.reply_to` | callback or audit address for outcome correlation |
| `data.payload` | additive, product-owned body |

This is CloudEvents-compatible in spirit, but not a commitment to a CloudEvents library. The binding can be HTTP, Gateway-native, queue, or local call. The spine does not change.

## Message Intents

### `Escalation`

Sent by Sentinel or Gateway when cheap evidence says attention may be earned.

Rules:
- Must name the observed evidence, threshold crossed, and proposed next tier.
- Must not contain LLM inference masquerading as observation.
- Identity-altitude escalations must set `payload.escalation_target = "sovereign_only"`.

Normal path:

`Sentinel -> Gateway -> Council` when ambiguity needs judgment.

Fast path:

`Sentinel -> Gateway -> Worker` only when the action is deterministic, bounded, and already authorized by policy.

### `Directive`

Sent by Council, through Gateway, when judgment has earned action.

Rules:
- Must name the job, scope, stop condition, and return expectation.
- Must not grant persistence, looping, or schedule authority to the Worker.
- Must route any identity or recall write through Librarian approval.

Normal path:

`Council -> Gateway -> Worker -> Librarian commit proposal`

Librarian may accept, reject, or ask for Sovereign review. Librarian does not become a Worker, and Worker output does not become memory by default.

## Product Duties

| Product | Contract duty |
|---|---|
| **Sentinel** | Watches typed evidence cheaply and emits `Escalation` only when a rule fires. |
| **Gateway** | Owns transport binding, budget enforcement, ledger correlation, and policy routing; it does not infer meaning. |
| **Council** | Adjudicates ambiguous `Escalation`s and emits bounded `Directive`s; it does not execute. |
| **Librarian** | Owns tenant identity, recall, and commit approval; no identity-context crosses tenant scope. |

## Implementation notes

Crate-level behavior (outer envelope wrapper, CE reject paths, JCS edges,
fence corpus ownership, provenance types) is documented in
[`docs/protocol-implementation.md`](docs/protocol-implementation.md).
That page does not change this v0.1 spine.

## Adoption Pointers

- `sentinel`: Adopt `COMMS_CONTRACT.md` as the v0.1 inter-product boundary; Sentinel emits only `Escalation`.
- `council-rs`: Adopt `COMMS_CONTRACT.md` as the v0.1 inter-product boundary; Council consumes `Escalation` and emits `Directive`.
- `gateway`: Adopt `COMMS_CONTRACT.md` as the v0.1 inter-product boundary; Gateway binds, meters, routes, and audits both message kinds.
- `sovereign-librarian`: Adopt `COMMS_CONTRACT.md` as the v0.1 inter-product boundary; Librarian supplies scoped identity context and gates commits.

## Deliberate Gaps

These are intentionally left open in v0.1:

- **Transport binding:** Gateway chooses the first binding. The contract is the spine, not HTTP vs queue vs local IPC.
- **Payload schemas:** Product-owned payloads are additive until a real demo path proves which fields are load-bearing.
- **Worker return shape:** v0.1 requires a return expectation, not a universal result schema.
- **Librarian commit policy:** Accept/reject/review rules remain Librarian-owned and tenant-scoped.
- **Sentinel registry mechanics:** Registration, temperature, cooldown, and admission control remain Gateway-owned ADRs.
- **Failure semantics:** `ttl` plus audit correlation is enough for v0.1. No retry-storm protocol until runtime evidence demands it.

## Versioning

This file is frozen as **v0.1 intent**. Future semantic changes supersede it by ADR and update `superseded_by`; do not mutate the contract in place except for spelling or broken links.

Additive payload fields do not require a new contract version. New message kinds, changed duties, cross-tenant behavior, retry semantics, or a new transport requirement do.
