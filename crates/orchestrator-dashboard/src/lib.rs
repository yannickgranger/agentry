//! orchestrator-dashboard library — exposes the request-routing layer so
//! integration tests can spin the brief routes against a temp transcripts
//! directory without booting the full webhook + Redis-backed dashboard.

#![forbid(unsafe_code)]

pub mod routes;
