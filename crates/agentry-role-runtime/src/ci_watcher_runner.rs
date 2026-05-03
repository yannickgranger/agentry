//! Pure helpers for the `ci-watcher-runner` binary (EPIC #161 Wave 2 port
//! of `CI_WATCHER_AGENTRY_SCRIPT`). Extracted to a lib module so the
//! `tests/ci_watcher_runner_test.rs` test crate can exercise them — the
//! workspace's `arch-ban-inline-cfg-test-in-src.cypher` rule (PR #295)
//! forbids inline `#[cfg(test)] mod tests` blocks in `src/`.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Return the LAST message in `messages` whose `from` field equals
/// `"shipper-agentry"`. Mirrors bash:
/// `[.team_context.messages[] | select(.from=="shipper-agentry")] | last`.
///
/// Returns `None` when no shipper-agentry message is present.
pub fn find_shipper_message(messages: &[Value]) -> Option<&Value> {
    messages
        .iter()
        .rfind(|m| m.get("from").and_then(Value::as_str) == Some("shipper-agentry"))
}

/// Return the `context` of the FIRST status entry whose `state` is
/// `"failure"` or `"error"`. Mirrors bash:
/// `[.statuses[]? | select(.state=="failure" or .state=="error") | .context] | .[0]`.
///
/// Returns `None` on empty input or when no failing context is present.
pub fn first_failing_context(statuses: &[Value]) -> Option<String> {
    statuses
        .iter()
        .find(|s| {
            matches!(
                s.get("state").and_then(Value::as_str),
                Some("failure") | Some("error")
            )
        })
        .and_then(|s| s.get("context").and_then(Value::as_str).map(str::to_string))
}

/// Return a non-cryptographic jitter in `0..=9`, replacing bash's
/// `RANDOM % 10`. Used by the merge-retry backoff loop to avoid the
/// thundering-herd pattern when several CI-green children attempt to
/// merge concurrently.
pub fn rand_jitter() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.subsec_nanos() as u64) % 10)
        .unwrap_or(0)
}
