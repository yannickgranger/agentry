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

/// Return EVERY failing context — same predicate as
/// [`first_failing_context`] but collected into a `Vec` in source order.
/// Entries without a `context` string are skipped. The rework coder needs
/// the full set because non-trivial CI configs run multiple parallel
/// checks and any one of them can be the actionable failure.
pub fn all_failing_contexts(statuses: &[Value]) -> Vec<String> {
    statuses
        .iter()
        .filter(|s| {
            matches!(
                s.get("state").and_then(Value::as_str),
                Some("failure") | Some("error")
            )
        })
        .filter_map(|s| s.get("context").and_then(Value::as_str).map(str::to_string))
        .collect()
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

/// Result of one merge-POST attempt as classified by the retry loop —
/// the variants the binary's `merge_with_retry` reacts to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptResult {
    /// `http_post` returned `(http_code, body)`.
    Ok(String, String),
    /// `http_post` failed before producing an HTTP code (curl spawn /
    /// non-zero exit). The string is the short error detail.
    Err(String),
}

/// Terminal verdict the retry loop reaches after exhausting attempts or
/// hitting a definitive code. The binary maps each variant to the
/// corresponding emit_* + emit_done call sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeRetryOutcome {
    /// Merge POST returned 200/204 on `attempt` (1-indexed) — emit Shipped.
    Merged { attempt: u32 },
    /// Merge POST returned a code outside `{200, 204, 405, 409}` on
    /// `attempt` — these are real errors pr-rebaser cannot fix, emit
    /// Failed.
    NonTransientFail {
        code: String,
        detail: String,
        attempt: u32,
    },
    /// Retry budget exhausted on persistent transient 405/409 (or curl
    /// spawn errors): the merge-POST race against develop advancing —
    /// chain-trigger pr-rebaser instead of emitting Failed.
    ExhaustedTransient { code: String, detail: String },
}

/// Drive the merge-POST retry loop using caller-supplied `do_post` and
/// `on_transient` closures, returning the terminal [`MergeRetryOutcome`].
/// Pure with respect to network and emit_* I/O so the test crate can
/// exercise it without mocking curl or stdout.
///
/// `do_post(attempt)` performs one HTTP attempt; `on_transient(attempt,
/// code, detail)` runs after a transient 405/409/spawn-error when more
/// attempts remain (the binary uses it for the per-iteration sleep +
/// emit_event). Neither closure is invoked after the loop reaches a
/// terminal classification.
pub fn run_merge_retry_loop<F, S>(
    max_retries: u32,
    mut do_post: F,
    mut on_transient: S,
) -> MergeRetryOutcome
where
    F: FnMut(u32) -> AttemptResult,
    S: FnMut(u32, &str, &str),
{
    let mut last_code = String::new();
    let mut last_detail = String::new();
    for attempt in 1..=max_retries {
        match do_post(attempt) {
            AttemptResult::Ok(code, detail) => {
                last_code = code.clone();
                last_detail = detail.clone();
                if code == "200" || code == "204" {
                    return MergeRetryOutcome::Merged { attempt };
                }
                if code == "405" || code == "409" {
                    if attempt < max_retries {
                        on_transient(attempt, &code, &detail);
                        continue;
                    }
                    break;
                }
                return MergeRetryOutcome::NonTransientFail {
                    code,
                    detail,
                    attempt,
                };
            }
            AttemptResult::Err(e) => {
                last_code = "(spawn-error)".into();
                last_detail = e;
                if attempt < max_retries {
                    on_transient(attempt, &last_code, &last_detail);
                    continue;
                }
                break;
            }
        }
    }
    MergeRetryOutcome::ExhaustedTransient {
        code: last_code,
        detail: last_detail,
    }
}
