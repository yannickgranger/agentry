//! Public-surface tests for the orchestrator daemon.
//!
//! `daemon.rs`'s prior inline tests exercised private helpers and
//! state-machine internals: `inbound_satisfied`, `downstream_subdag`,
//! `resolve_rework_target`, `mint_permit`, `next_brief_paths`,
//! `load_next_brief`, `collect_chain_paths`, `compose_verdict_parts`,
//! `compose_meta_verdict`, and `handle_brief`. The migration recipe forbids
//! promoting their visibility, so those tests are dropped — the behaviours
//! they covered are exercised end-to-end by the existing integration suite:
//! `dispatch_concurrency_cap`, `integration_role_loader_malformed`,
//! `integration_transcript_capture`, `integration_workspace_lifecycle`, and
//! `transcript_parsing`.
//!
//! The only public surface on `daemon` is `run(cfg) -> Result<()>`, the top
//! of the orchestrator's main loop. There is no pure-helper smoke test
//! worth pinning here.
