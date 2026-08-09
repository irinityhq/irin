# Gateway Security Boundary

Gateway binds to loopback by default and is intended for single-operator local
use. The sidecar listens only on its Unix domain socket; the OpenResty edge
re-exposes a fixed allowlist of admin routes on the loopback HTTP port, and the
sidecar authorizes each one. Force-wake and quarantine routes stay
socket-only; worker lifecycle routes ride the proxied `/watch/outbox/` prefix
and are admin-authorized in the sidecar. The producer arm ceremony is proxied
only when the installed desktop pack mounts its non-secret feature marker. Externally reachable Gateway
routes require configured caller authentication.

The directive outbox stores signed artifacts and its row reads require admin
authorization. Only the signing public-key endpoint is unauthenticated. The
watch producer and action path remain disabled until explicitly enabled and
armed.

Gateway stores business content in local durable databases. Credential-shaped
values are scrubbed on selected paths, but general content redaction is not
provided. Protect the host, signing key, caller keys, and database backups.

Report vulnerabilities using the repository root [`SECURITY.md`](../SECURITY.md).
