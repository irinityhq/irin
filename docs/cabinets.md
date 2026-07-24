# Cabinets

A cabinet is Council's complete deliberation configuration: a chair, one or
more seats, a round count, and — for the CLI — a mode flag that controls how
seats reason (tear-down, pathfind, harden). Every deliberation, from the CLI
or War Room, runs one cabinet.

## Where cabinets live

Cabinet definitions are YAML files under `council-rs/cabinets/`, loaded by
registry key (the file stem). Shipped cabinets include general-purpose ones
(`standard`, `quick`, `duo`, `checklist-duo`, `sovereign`, `wargame`,
`reflection`, `heritage`, `code-verify`, `freeride`, `warroom`, `trinity`,
`triage`), a set of
`triad-*` domain cabinets (architecture, debugging, product, risk, shipping,
strategy) that War Room groups separately as "Domain triads", and
`starter-nvidia`, described below. The directory also contains the internal,
host-specific `triage.canary-novertex` overlay; Council deliberately excludes
any `*canary*` stem from the normal registry and War Room picker.

Run one from the CLI:

```bash
./target/release/council --base-dir council-rs --cabinet standard "Topic"
```

## Selection in War Room

War Room's Cabinet selector (`GET /api/cabinets`) lists every registry
cabinet as a chip, split into embedded cabinets and domain triads. Each chip
shows seat count, round count, and — when compared against the normalized
Discover inventory (`GET /api/discover`) — which providers that cabinet needs
that are not currently available. Unavailable chips stay selectable and show a
muted `(need …)` note; danger styling is reserved for real action or system
errors (for example next to Convene when the selected cabinet cannot run).

### Default cabinet on first load

The documented default name is `standard`. On an **untouched** Deliberate
first load — once cabinets and the Discover inventory are known — War Room
applies this stable rule once:

1. Keep `standard` when every seat and chair transport is available.
2. Otherwise select the first runnable cabinet in API list order, preferring
   non-triad (embedded) cabinets before domain triads.
3. If no cabinet is runnable, keep the current selection and show one
   actionable explanation near Convene (not a grid of danger-red cards).

Runnability comes from the Discover inventory, which actually probes CLI
transports; `/api/health` is a cheap liveness probe and deliberately reports
host CLI transports as unavailable. An explicit `initialCabinet` from the
Cabinets editor, any manual chip click, and the result of that one-shot auto
decision are all locked for the rest of the idle mount. Discovery flaps must
not re-auto-switch the selection.

## Customization

The Cabinets tab lets an operator edit a cabinet's label, rounds, seats, and
chair, and save it back with `POST /api/cabinets/save`. The registry key
(file stem) must match `^[a-z0-9][a-z0-9_-]{0,63}$`; the server is the
authority on YAML validity, refuses to overwrite a built-in cabinet, and
allows an operator-created cabinet to be saved again under its existing key.
The browser-side checks are pre-flight only. Every seat needs a
name, an exact provider transport, and a model; the chair needs a provider
and a model. See [`council-rs/docs/providers.md`](../council-rs/docs/providers.md)
for the exact transport IDs a seat's `provider` field can use.

## Partial-seat behavior

A cabinet that references a provider transport health does not currently
list is not hidden — it stays visible, shown at reduced emphasis with a muted
"need `<provider>`" note listing exactly which seats are missing. Council
does not silently drop or reroute a missing seat to a different provider.
Convene stays blocked for that selection with one warning near the action.
Fix availability (export the missing API key, authenticate the CLI, or edit
the cabinet to a provider you have) before running that cabinet live. For a
local CLI, Discover proves only that the supported binary is present; the
first real call proves that its authentication and model entitlement work.

## Optional NVIDIA starter

`starter-nvidia` is an optional first-live-deliberation cabinet built around
one free-tier NVIDIA NIM API key (`NVIDIA_API_KEY`). It is a courtesy option
for an operator who already wants that path, not a prerequisite or a
specially supported provider for IRIN generally. No other cabinet defaults
to NVIDIA.

Its seat models must be present in
`council-rs/config/nim-invokable-allowlist.txt`; the cabinet's own comments
record real operational churn in what NVIDIA's hosted catalog makes
invokable for a given account (for example, a model can be catalog-listed
but not actually callable, or a chair model can hit an upstream timeout in
practice and get swapped for one that reliably completes). Treat
`starter-nvidia`'s exact models as a snapshot, not a guarantee — confirm with
`--discover` and a `--smoke-provider nvidia` call before relying on a
specific model in that cabinet continuing to work.

## Model entitlement churn generally

Provider catalogs, CLI-exposed model lists, and per-account entitlements
change over time and are not uniform across operators. `council --discover`
reports detected transports and available model catalogs where the transport
exposes one; the installed routing YAML (`claude_routing.yaml`,
`grok_routing.yaml`, `agy_routing.yaml`, `gemini_routing.yaml`) supplies
curated CLI choices. Neither proves entitlement. Only a real, bounded
provider call confirms that a particular account can invoke a particular
model now.
