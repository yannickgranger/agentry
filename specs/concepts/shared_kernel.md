# Shared kernel

Primitive types used across every bounded context in agentry. This is the
Shared Kernel of the DDD context map: changes here affect every downstream
context, so they require wider review. Kept intentionally small.

## Ts

UTC timestamp alias used on every event, verdict, and permit. Monotonic only
to the resolution of the system clock — never used for cryptographic nonces.

## VersionedRef

A name + monotonic version tuple pointing at a `Registry` record (role or
team). Briefs name teams by `VersionedRef`; the daemon resolves them to the
concrete `AgentRole` / `TeamTopology` at dispatch time. Version is required
so a change to a role does not retroactively alter in-flight briefs.

## TypeError

The shape-validation error raised by parsers and constructors inside
`orchestrator-types`. Distinct from `execution::Error` — this one is about
data not making sense structurally, not about failed side effects.
