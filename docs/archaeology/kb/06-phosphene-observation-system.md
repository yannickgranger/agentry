# PHOSPHENE: 5-Layer Agent Monitoring with Mood FSM

**Sources:**
- `/var/mnt/workspaces/agency-orchestrator/docs/RFC/RFC-14-observation.md`
- External repo: `/var/mnt/workspaces/phosphene/`

**1-line summary:** Real-time agent health monitoring via ## STATUS lines, classified into behaviors (Healthy/Stuck/Looping/Dead), aggregated into system mood (Calm/Vigilant/Anxious/Panic/Shutdown), emits alerts to escalation.

---

## 5-Layer Architecture

| Layer | Name | Function |
|-------|------|----------|
| **SD-1** | Observation | Read ##STATUS lines via `ObservationSourcePort`, parse with `StatusParser` |
| **SD-2** | Classification | Deterministic state machine classifies agent behavior (7 states) |
| **SD-3** | Limbic (Mood FSM) | Aggregate classification signals → system-wide mood (5 states) |
| **SD-4** | Alert Emission | Emit typed alerts to `observation:alerts` Redis stream |
| **SD-5** | Compliance (SIS) | Detect gate skips, force pushes, CI bypasses |

---

## Classification System (SD-2)

From `phosphene-domain/src/classifier.rs`:

| Classification | Trigger |
|----------------|---------|
| `Healthy` | Expected signal within interval |
| `Silent` | Expected signal missing > grace period (absence telemetry) |
| `Stuck` | Same phase repeated N times |
| `Struggling` | Rising error count |
| `Looping` | Phase regression detected |
| `BlockerReported` | Explicit blocker in ##STATUS |
| `Dead` | No response for critical duration |

**Per-agent sliding window of phase history with deterministic transition rules.**

---

## Mood FSM (SD-3)

| State | Check Interval | Alert Channel | Escalation |
|-------|---------------|---------------|------------|
| `Calm` | 1000ms | None | — |
| `Vigilant` | 500ms | Dashboard | — |
| `Anxious` | 200ms | Email | — |
| `Panic` | 100ms | Pager/SMS | Immediate |
| `Shutdown` | 100ms | Manual intervention | System halt |

**Port:** `MoodPublisherPort` — drives transitions based on aggregate classification signals.

---

## Absence Telemetry (Novel)

Expectation-based monitoring: for each agent, define expected signal intervals with grace periods. If no signal arrives within `expected_interval + grace_period`, classify as `Silent`.

```rust
pub struct Expectation {
    id: Uuid,
    name: String,
    expected_interval_secs: u64,
    grace_period_secs: u64,
    source: String,           // "heartbeat", "marker"
    last_satisfied: Option<DateTime<Utc>>,
}
```

This is fundamentally different from "has the agent timed out?"—it's "the agent broke an expectation it set."

---

## Compliance Monitoring (SD-5/SIS)

Violation types:
- `GateSkipped` — CI gate explicitly bypassed
- `CommitWithoutClippy` — pushed without clippy passing
- `ForcePushWithoutGate` — force push without prior gate pass

**Port:** `ComplianceMonitorPort` — consumes `CommitEventSourcePort`, publishes `SystemThreatDetected` → Alert.

---

## Known Coupling Violation (BR-11)

PHOSPHENE writes directly to `agent:lead-dev:inbox` (S-037) — bypasses the escalation domain boundary. Should route through `observation:alerts` → ACL-7 → escalation → lead-dev inbox.

**Status:** Code exists for ACL-7 (Observation → Escalation) adapter in escalation-contracts, but is not wired in production.

---

## Why Interesting for v2

Absence telemetry is a clever inversion: instead of "timeout after X seconds," you track "agent broke its own expectations." Mood FSM is multi-paradigm (NSA SIGINT, industrial control theory, stigmergic consciousness)—expensive in prose but cheap in logic. The system escalates intelligently: Calm → Vigilant (dashboard) → Anxious (email) → Panic (immediate) → Shutdown (manual).

