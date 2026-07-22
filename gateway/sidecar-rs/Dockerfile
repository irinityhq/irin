# Unified cargo-chef build — ONE Docker build compiles the sidecar with the repo
# `.git` IN CONTEXT, so `build.rs` runs `git status`/`git rev-parse` IN-CONTAINER
# against THIS build's actual tree state and bakes GW_BUILD_DIRTY / GW_BUILD_GIT_SHA
# at compile time (sidecar-rs/build.rs -> cargo:rustc-env -> attest.rs option_env!).
#
# Invariant (warm-cache Phase 3 smoke, reviewed):
#   - cargo-chef caches the DEPENDENCY compile only (leg-invariant: keyed on
#     Cargo.lock, no .git, no leaf source -> poisoning it cannot forge provenance).
#   - The leaf crate `gateway-sidecar` (carries build.rs + watch/attest.rs) is
#     EXCLUDED from every compiler cache: `cargo clean -p gateway-sidecar` forces
#     its build-script to re-execute against THIS leg's copied .git on every build.
#   - Cache-provenance NON-CROSSING invariant: for each leg L in {clean, dirty},
#     the asserted binary is the output of a build.rs run that saw leg L's tree
#     state. No cache may serve the leaf an artifact baked under a different state.
#
# IMPORTANT: build context MUST be the IRIN monorepo ROOT (see docker-compose.yml
# `sidecar.build.context: ..`), NOT gateway/ or gateway/sidecar-rs. Both narrower
# contexts exclude the monorepo .git, so build.rs sees "no git" and bakes
# dirty=true (DARK). The two-leg Phase 3 smoke distinguishes clean vs dirty by the
# .git state present in the context, never by a build-arg (P1-5 forbids it).

# syntax=docker/dockerfile:1.7

FROM rust:alpine@sha256:f87aa870663e2b57ec8c69de82c7eedf7383bee987eef7612c0359635eaadb41 AS chef
# reqwest uses rustls; reqwest 0.13's rustls path
# pulls aws-lc-sys which builds AWS-LC via cmake. Keep builder tools explicit while
# preserving the no-OpenSSL invariant. `git` is required so build.rs can probe the
# in-context .git (the whole point of the unified build).
RUN apk add --no-cache musl-dev cmake make git
# Pin via the crate's own lockfile for a reproducible chef. Version intentionally
# unpinned: chef only caches DEPS — the leaf is force-recompiled regardless, so the
# chef version can never affect the baked provenance bit.
RUN cargo install --locked cargo-chef
WORKDIR /build

# --- planner: compute the dependency recipe -------------------------------------
# The planner mirrors the workspace layout so Cargo resolves the same workspace
# members and path dependencies during dependency cooking and the real build.
FROM chef AS planner
COPY . /build/
# recipe.json lives OUTSIDE the repo tree (/recipe.json, not under sidecar-rs/) so
# it never appears as an untracked file to the in-container build.rs git probe.
RUN cargo chef prepare --recipe-path /recipe.json

# --- builder: cook deps (CACHED), then build the leaf fresh with real git tree ---
FROM chef AS builder
# The committed [profile.release] uses lto=true + codegen-units=1 (RAM heavy).
# Smoke/dev builds pass lighter values; cook AND the real build inherit the SAME
# ENV so the cooked dep rlibs are reused by the final link (mismatched LTO would
# silently bust the dep cache).
ARG CARGO_PROFILE_RELEASE_LTO=true
ARG CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
ENV CARGO_PROFILE_RELEASE_LTO=${CARGO_PROFILE_RELEASE_LTO}
ENV CARGO_PROFILE_RELEASE_CODEGEN_UNITS=${CARGO_PROFILE_RELEASE_CODEGEN_UNITS}

COPY --from=planner /recipe.json /recipe.json
# THE cache layer: deps only. Reused across every build (both smoke legs) until
# Cargo.lock changes. rust:alpine targets x86_64-unknown-linux-musl natively, so
# the default target matches the real build below (consistency keeps the cache warm).
RUN cargo chef cook --release --recipe-path /recipe.json --package gateway-sidecar

# Real source AND the COMPLETE working tree (incl .git) for THIS leg. Linked
# worktrees are staged as normal repository directories by the smoke/demo
# harnesses because Docker cannot follow their out-of-context `.git` pointers.
# The whole
# tree is required, not just sidecar-rs/: build.rs runs `git status --porcelain`
# REPO-WIDE, so a partial copy would leave every other tracked file "deleted" and
# bake dirty=true on a clean checkout. .dockerignore must not exclude tracked
# paths, so the copied tree faithfully matches the index -> a clean checkout is
# clean in-container, a deliberately-staged marker (smoke dirty leg) reads dirty.
# Copying source AFTER cook means a source change never busts the cooked dep layer
# (target/ is .dockerignore'd, so this COPY cannot clobber the cooked artifacts).
COPY . /build/
# Council P0-1 (mechanical leaf re-exec): wipe ONLY the leaf package so its
# build-script fingerprint is gone and re-runs against the just-copied .git. Cooked
# deps in target/ survive. safe.directory '*' so the root-owned .git is trusted
# (does NOT weaken B6 — dirty is still a real `git status` over the real tree).
# `git clean -fd` (NOT -x) removes any UNTRACKED leftovers cargo-chef cook left in
# the tree (dummy targets it couldn't auto-clean) so the in-container `git status`
# is FAITHFUL — a clean checkout reads clean. It keeps the gitignored cooked
# target/ (no -x), so the dep cache survives. The staged dirty-leg marker is
# TRACKED, so clean never removes it (the dirty signal is preserved).
RUN git config --system --add safe.directory '*' \
 && git clean -fd \
 && cargo clean -p gateway-sidecar --release \
 && cargo build --release --locked -p gateway-sidecar --bin gateway-sidecar \
 && cp target/release/gateway-sidecar /build/gateway-sidecar \
 && echo "=== build.rs emitted provenance (this leg) ===" \
 && (grep -h 'GW_BUILD' target/release/build/gateway-sidecar-*/output 2>/dev/null \
       || echo '(no build-script output captured)')

# --- runtime: bare Alpine, no Rust toolchain ------------------------------------
FROM alpine:3.22@sha256:310c62b5e7ca5b08167e4384c68db0fd2905dd9c7493756d356e893909057601

# sqlite is included for dev/smoke harness DB inspection (test/smoke_phase3.sh).
# triad_mgmt gid 9999 matches Dockerfile.gateway + the compose socket-gid lockdown.
RUN apk add --no-cache ca-certificates sqlite && \
    addgroup -g 9999 triad_mgmt

COPY --from=builder /build/gateway-sidecar /usr/local/bin/gateway-sidecar

EXPOSE 8000
CMD ["gateway-sidecar"]
