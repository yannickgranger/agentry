//! orchestratord — the daemon binary.
//! Reads briefs from Redis, spawns agent containers, records verdicts.

use orchestrator_runtime::lifecycle::{
    EventSource, RedisEventSource, RedisStateProjector, StateProjector,
};
use orchestrator_runtime::redis_io;
use orchestrator_runtime::{daemon, Config};
use orchestrator_types::BriefId;
use std::sync::Arc;

#[tokio::main]
async fn main() -> orchestrator_runtime::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchestrator_runtime=info,info".into()),
        )
        .init();
    let cfg = Config::load()?;
    tracing::info!(
        dashboard_port = cfg.dashboard.port,
        "orchestratord starting"
    );

    // L.3a: build the per-brief lifecycle adapter factories. Each factory
    // captures a shared ConnectionManager and returns a fresh adapter
    // bound to the brief id at dispatch time. The daemon spawns the
    // resulting projector task alongside the legacy role-chain.
    let conn_for_factories = redis_io::connect(&cfg.redis.url).await?;
    let event_conn = conn_for_factories.clone();
    let event_source_factory: Arc<dyn Fn(BriefId) -> Box<dyn EventSource + Send> + Send + Sync> =
        Arc::new(move |brief_id| Box::new(RedisEventSource::new(event_conn.clone(), brief_id)));
    let projector_conn = conn_for_factories;
    let state_projector_factory: Arc<
        dyn Fn(BriefId) -> Box<dyn StateProjector + Send> + Send + Sync,
    > = Arc::new(move |brief_id| {
        Box::new(RedisStateProjector::new(projector_conn.clone(), brief_id))
    });

    daemon::run(&cfg, event_source_factory, state_projector_factory).await
}
