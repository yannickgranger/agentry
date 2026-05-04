//! null-agent — meta-brief no-op shake-down agent.
//!
//! Ports `NULL_AGENT_AGENTRY_SCRIPT` from bash to Rust under EPIC #161 B0.
//! The bash heredoc was four lines: emit one `event`, emit `done shipped`.
//! The Rust port does exactly the same, plus gets the structural
//! always-emit-done guarantee from `DoneGuard` for free.

use agentry_role_runtime::{emit_done, emit_event, DoneGuard};
use orchestrator_types::EventVerdict;
use serde_json::json;

fn main() {
    let _guard = DoneGuard::new();
    emit_event(json!({
        "msg": "null-agent shake-down",
        "status": "ok",
    }));
    emit_done(EventVerdict::Shipped, None);
}
