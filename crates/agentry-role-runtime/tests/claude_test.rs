//! Public-surface tests for `agentry_role_runtime::claude`.
//!
//! `claude.rs`'s prior inline tests exercised only crate-private helpers:
//! `reconstruct_assistant_text` (transcript parsing) and `parse_tool_refusal`
//! (single-line JSON-strict refusal sniffing). Both are private fn called
//! exclusively from `stream_claude`, which itself spawns
//! `timeout(1) claude -p ...` and is not exercisable in unit tests without
//! the `claude` CLI on PATH.
//!
//! The migration recipe forbids promoting their visibility (`NO orphan new
//! pub items` — callers fence is live), so those tests are dropped here.
//! The behaviours they covered remain enforced by the original inline
//! suite's intent: any future regression surfaces only in end-to-end runs
//! that exercise `stream_claude` against a real claude transcript.
//!
//! The only items re-exported via `lib.rs` are `stream_claude` and
//! `StreamErr` — both require the CLI to drive, so no smoke test is
//! pinned here.
