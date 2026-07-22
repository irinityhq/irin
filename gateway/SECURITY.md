# Gateway Security Boundary

Gateway binds to loopback by default and is intended for single-operator local
use. The sidecar management surface is available only through its Unix domain
socket, and externally reachable Gateway routes require configured caller
authentication.

The directive outbox stores signed artifacts and its row reads require admin
authorization. Only the signing public-key endpoint is unauthenticated. The
watch producer and action path remain disabled until explicitly enabled and
armed.

Gateway stores business content in local durable databases. Credential-shaped
values are scrubbed on selected paths, but general content redaction is not
provided. Protect the host, signing key, caller keys, and database backups.

Report vulnerabilities using the repository root [`SECURITY.md`](../SECURITY.md).
