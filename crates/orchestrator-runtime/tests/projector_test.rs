//! Public-surface tests for the projector.
//!
//! `projector.rs`'s prior inline tests (`project_spawn_then_terminate_roundtrips_to_state`,
//! `project_unknown_agent_event_advances_watermark`,
//! `project_terminate_with_missing_exit_code_records_null`) all exercised the
//! private `project_payload` helper directly. The migration recipe forbids
//! promoting their visibility, so those tests are dropped — the projection
//! behaviour they covered is exercised end-to-end by the integration suite
//! that drives the daemon (which in turn spawns the projector against a live
//! Redis).
//!
//! The only public surface on `projector` is `run(state, conn) -> !`, an
//! infinite loop that requires Redis streams to drive any observable
//! behaviour. There is no pure-helper smoke test worth pinning here.
