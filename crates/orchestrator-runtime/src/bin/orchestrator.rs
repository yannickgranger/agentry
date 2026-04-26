//! orchestrator — the CLI.
//! `orchestrator submit <brief-file>` — submit a brief.
//! `orchestrator seed` — seed the registry (roles + teams).
//! `orchestrator verdicts` — list last N verdicts.
//! `orchestrator abort --all` — abort all running briefs.

use clap::{Parser, Subcommand};
use orchestrator_runtime::{cli_agents, permit, redis_io, seed, state, Config, Result};
use orchestrator_types::Brief;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "orchestrator", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Submit a brief (JSON file) to `agentry:briefs`.
    Submit {
        /// Path to a JSON Brief file.
        file: PathBuf,
    },
    /// Seed the registry with the default roles and team topologies.
    Seed,
    /// List the last N verdicts.
    Verdicts {
        #[arg(short, long, default_value_t = 10)]
        count: usize,
    },
    /// Abort running briefs (M0: kills all labelled containers).
    Abort {
        #[arg(long)]
        all: bool,
    },
    /// Generate an ed25519 signing key for permits (M3).
    KeyGen {
        /// Overwrite if the key already exists.
        #[arg(long)]
        force: bool,
    },
    /// Inspect the running fleet (NDJSON output).
    Agents {
        #[command(subcommand)]
        sub: AgentsCmd,
    },
}

#[derive(Subcommand, Debug)]
enum AgentsCmd {
    /// List agents (default: status='running').
    List {
        #[arg(long)]
        all: bool,
    },
    /// Run a read-only SELECT against the agent index.
    Query { sql: String },
    /// Show recent trace events for an agent.
    Trace {
        agent_id: String,
        #[arg(long, default_value_t = 50)]
        last: usize,
    },
    /// Show recent Status verdicts for an agent.
    RecentStatus {
        agent_id: String,
        #[arg(long, default_value_t = 10)]
        count: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchestrator_runtime=info,warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = Config::load()?;
    match cli.cmd {
        Cmd::Submit { file } => {
            let text = tokio::fs::read_to_string(&file).await?;
            let brief: Brief = serde_json::from_str(&text)?;
            let mut conn = redis_io::connect(&cfg.redis.url).await?;
            let id = redis_io::submit_brief(&mut conn, &brief).await?;
            println!(
                "{{\"submitted\":true,\"brief_id\":\"{}\",\"stream_id\":\"{}\"}}",
                brief.id, id
            );
        }
        Cmd::Seed => {
            seed::seed_m0(&cfg).await?;
            println!("{{\"seeded\":true}}");
        }
        Cmd::Verdicts { count } => {
            let mut conn = redis_io::connect(&cfg.redis.url).await?;
            use redis::AsyncCommands;
            let rev: redis::streams::StreamRangeReply = conn
                .xrevrange_count(redis_io::STREAM_VERDICTS, "+", "-", count)
                .await?;
            for entry in rev.ids.iter().rev() {
                let body: Option<String> = entry.map.get("verdict").and_then(|v| match v {
                    redis::Value::BulkString(b) => std::str::from_utf8(b).ok().map(String::from),
                    redis::Value::SimpleString(s) => Some(s.clone()),
                    _ => None,
                });
                if let Some(b) = body {
                    println!("{}", b);
                }
            }
        }
        Cmd::KeyGen { force } => {
            let path = permit::key_path_from(&cfg);
            if path.exists() && !force {
                eprintln!(
                    "refusing to overwrite existing key at {} (use --force)",
                    path.display()
                );
                std::process::exit(2);
            }
            permit::generate_and_save(path)?;
            println!("{{\"generated\":true,\"path\":\"{}\"}}", path.display());
        }
        Cmd::Abort { all } => {
            if all {
                // Kill all agentry-labelled containers via podman.
                let out = tokio::process::Command::new("podman")
                    .args(["ps", "--filter", "label=agentry.brief", "-q"])
                    .output()
                    .await?;
                let ids = String::from_utf8_lossy(&out.stdout);
                for id in ids.split_whitespace() {
                    let _ = tokio::process::Command::new("podman")
                        .args(["stop", "-t", "1", id])
                        .output()
                        .await;
                }
                println!("{{\"aborted\":true}}");
            } else {
                eprintln!("abort requires --all for M0");
                std::process::exit(2);
            }
        }
        Cmd::Agents { sub } => {
            let state_path = std::env::var("AGENTRY_STATE_PATH").unwrap_or_else(|_| {
                format!(
                    "{}/.config/agentry/state.db",
                    std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
                )
            });
            match sub {
                AgentsCmd::List { all } => {
                    let state = state::open_or_init(Path::new(&state_path))?;
                    let rows = cli_agents::list(&state, all).await?;
                    for v in rows {
                        println!("{}", serde_json::to_string(&v)?);
                    }
                }
                AgentsCmd::Query { sql } => {
                    let state = state::open_or_init(Path::new(&state_path))?;
                    let rows = cli_agents::query(&state, &sql).await?;
                    for v in rows {
                        println!("{}", serde_json::to_string(&v)?);
                    }
                }
                AgentsCmd::Trace { agent_id, last } => {
                    let mut conn = redis_io::connect(&cfg.redis.url).await?;
                    let rows = cli_agents::trace(&mut conn, &agent_id, last).await?;
                    for v in rows {
                        println!("{}", serde_json::to_string(&v)?);
                    }
                }
                AgentsCmd::RecentStatus { agent_id, count } => {
                    let mut conn = redis_io::connect(&cfg.redis.url).await?;
                    let rows = cli_agents::recent_status(&mut conn, &agent_id, count).await?;
                    for v in rows {
                        println!("{}", serde_json::to_string(&v)?);
                    }
                }
            }
        }
    }
    Ok(())
}
