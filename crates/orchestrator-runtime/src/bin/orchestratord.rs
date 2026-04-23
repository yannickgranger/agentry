//! orchestratord — the daemon binary.
//! Reads briefs from Redis, spawns agent containers, records verdicts.

use orchestrator_runtime::{daemon, Config};

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
    daemon::run(&cfg).await
}
