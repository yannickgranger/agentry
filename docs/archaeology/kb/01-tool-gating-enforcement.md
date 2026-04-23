# Tool Gating: Physical Enforcement at MCP Level

**Sources:**
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-07-execution.md` (coder-mcp-server section)
- `/var/mnt/workspaces/agency-orchestrator/site/src/reference/agent-protocols.md`

**1-line summary:** Methodology gates are physically enforced via MCP tool availability, not policy—the `write_impl` tool returns HARDSTOP until spec gate passes.

---

## The Pattern

**Problem:** Without structural enforcement, gate compliance is policy-only. Coders could skip spec/contract phases and jump to implementation. 

**Solution:** Make the tools themselves conditional. From RFC-07:

> "Write tools (gated): `write_trait` (requires spec), `write_impl` (requires contract), `write_adapter` (requires domain), `checkpoint` (requires integration)"

The `coder-mcp-server` (B-002) implements `ToolGatekeeper.can_use_tool()` which checks `ToolGatingState` **before every operation**. If a gate hasn't passed, the tool returns a HARDSTOP error with structured reason:

```
ToolGatingState: Allowed | Blocked | AllowedWithWarning
→ block_reason.to_hardstop_message()
```

**Key insight:** This is not a "check at the end." It's a **state machine in the tool layer**. Coders physically cannot call `write_impl` until the gate state transitions to allow it. The orchestrator owns the state (Redis-backed `ToolGateSCRPort`).

---

## Current Implementation Gaps

- **Execution:** InMemoryToolGatekeeper works (logic proven). ALL tool execution returns stubs ("stubbed, tool allowed by gatekeeper").
- **TODO #137:** Wire Redis-backed `ToolGateSCRPort` for real state persistence.
- **Status:** STUB. Mechanism is proven. Integration pending.

---

## Why Interesting for v2

Tool gating sidesteps the "coders won't follow process" problem without permission systems—you don't forbid bad behavior, you make it invisible/unavailable. This is cheaper than IAM and more reliable than policy.

