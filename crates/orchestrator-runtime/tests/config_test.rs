//! Integration tests for `Config` defaults and figment overlay.

use figment::providers::Serialized;
use figment::Figment;
use orchestrator_runtime::Config;

#[test]
fn default_targets_local_redis_not_prod() {
    let c = Config::default();
    assert!(
        c.redis.url.contains("127.0.0.1") || c.redis.url.contains("localhost"),
        "default Redis URL must target local: got {}",
        c.redis.url
    );
    assert!(
        !c.redis.url.contains("192.168.1.152"),
        "default Redis URL must never point at prod LXC 401"
    );
    assert!(
        !c.redis.url.contains("192.168.1.189"),
        "default Redis URL must never point at prod LXC 522"
    );
}

#[test]
fn env_overlay_overrides_defaults() {
    let fig = Figment::from(Serialized::defaults(Config::default()))
        .merge(("redis.url", "redis://test.example:1234"))
        .merge(("dashboard.port", 9999u16));
    let c: Config = fig.extract().expect("extract");
    assert_eq!(c.redis.url, "redis://test.example:1234");
    assert_eq!(c.dashboard.port, 9999);
}

#[test]
fn default_dashboard_port_is_7800() {
    assert_eq!(Config::default().dashboard.port, 7800);
}

#[test]
fn default_max_concurrent_briefs_is_4() {
    assert_eq!(Config::default().max_concurrent_briefs, 4);
}
