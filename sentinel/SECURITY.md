# Sentinel Security Boundary

Sentinel owns the shared message contract and protocol crate. Gateway owns the
runtime registry, watch execution, authentication, budget enforcement, signing,
and outbox persistence. Council owns deliberation.

The stock Sentinel decision paths are deterministic and do not invoke an LLM.
Runtime configuration, evidence, and signed artifacts can contain private
operator content and must be protected as local state.

Report vulnerabilities using the repository root [`SECURITY.md`](../SECURITY.md).
