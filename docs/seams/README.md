# Feature-execution seams

Short **change-class** checklists for implementers and coding seats. These are
not orientation docs (use component READMEs and `docs/architecture.md` for
that). They answer: where a change lands, what must not break, and which proof
is enough.

**Rule:** each checklist **points** at tests, goldens, and contracts. It does
not restate product doctrine. If a checklist and a golden or named test
disagree, the **test or golden wins**.

## When to open which page

| Your PR thrash class | Open |
| --- | --- |
| Capability token wire, mint, authorize, durable replay, or opaque-execute refuse | [`structured-execute-authority.md`](structured-execute-authority.md) |
| Worker tick, quarantine execute, claim/lease/ack, typed nacks | [`worker-authority-quarantine.md`](worker-authority-quarantine.md) |
| Execute receipt projection or War Room receipt parser | [`redacted-execute-receipt.md`](redacted-execute-receipt.md) |

Related but thinner for this set: sequence-watch config thrash lives under
Gateway sentinel registry tests (`watch_sentinels` / `watch_registry`); open a
focused crate test before inventing a new page.

## Before inventing a path

1. Open the seam page for the change class (table above).
2. Confirm the listed modules and proof commands against your diff.
3. Then propose the path and run the proof rank on that page.

## Home

These pages are public and live under `docs/seams/`. Parent READMEs link here
from a short implementation map.
