//! Phase 4 of #330 v2: byte-for-byte equivalence tests for the permit
//! strings derived in `seed::seed_m0` from `Config`. The v1 attempt
//! produced `"net:allow:agency.lab:3000"` from a `default_host =
//! "agency.lab:3000"` value — a port-suffix regression that changed
//! `net:allow` permit semantics on every existing deployment. v2's
//! port-stripping idiom (`h.split(':').next().unwrap_or(h)`) restores
//! the prior literal `"net:allow:agency.lab"`. These tests pin that
//! contract so a future refactor cannot silently re-introduce the port.

use orchestrator_runtime::seed::{derive_forge_net_allow, derive_sccache_net_allow};
use orchestrator_runtime::Config;

fn cfg_with_forge_host(host: &str) -> Config {
    let mut cfg = Config::default();
    cfg.forge.default_host = Some(host.into());
    cfg.forge.allowed_owners = vec!["yg".into()];
    cfg
}

fn cfg_with_sccache_endpoint(endpoint: &str) -> Config {
    let mut cfg = Config::default();
    cfg.sccache.endpoint = Some(endpoint.into());
    cfg
}

#[test]
fn forge_net_allow_strips_port() {
    let cfg = cfg_with_forge_host("agency.lab:3000");
    let permit = derive_forge_net_allow(&cfg).expect("default_host set");
    assert_eq!(permit, "net:allow:agency.lab");
}

#[test]
fn forge_net_allow_no_port_unchanged() {
    let cfg = cfg_with_forge_host("agency.lab");
    let permit = derive_forge_net_allow(&cfg).expect("default_host set");
    assert_eq!(permit, "net:allow:agency.lab");
}

#[test]
fn sccache_net_allow_strips_port() {
    let cfg = cfg_with_sccache_endpoint("agentry-sccache-redis:6379");
    let permit = derive_sccache_net_allow(&cfg).expect("endpoint set");
    assert_eq!(permit, "net:allow:agentry-sccache-redis");
}

#[test]
fn sccache_net_allow_no_port_unchanged() {
    let cfg = cfg_with_sccache_endpoint("agentry-sccache-redis");
    let permit = derive_sccache_net_allow(&cfg).expect("endpoint set");
    assert_eq!(permit, "net:allow:agentry-sccache-redis");
}
