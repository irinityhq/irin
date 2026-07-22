# Council Security Boundary

Council deliberates and stores local proceedings. It does not execute Worker
actions. Gateway owns caller authentication, spend enforcement, directive
signing, and the durable outbox.

Council binds to loopback by default. Release builds require Council
authentication where configured. Provider output streams to the authenticated
local War Room before final stored content passes through secret-shape
redaction; arbitrary private content is not generally redacted.

Report vulnerabilities using the repository root [`SECURITY.md`](../SECURITY.md).
