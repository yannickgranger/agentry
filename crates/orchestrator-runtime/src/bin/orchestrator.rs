//! orchestrator — the CLI.
//! `orchestrator submit <brief-file>` — submit a brief.
//! `orchestrator seed` — seed the registry (M0: echo role + team).
//! `orchestrator verdicts` — list last N verdicts.
//! `orchestrator abort --all` — abort all running briefs.

use clap::{Parser, Subcommand};
use orchestrator_runtime::{Result, permit, redis_io, seed};
use orchestrator_types::Brief;
use std::path::PathBuf;

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
    /// Seed the registry with M0 defaults (echo-agent role + echo-team).
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
    match cli.cmd {
        Cmd::Submit { file } => {
            let text = tokio::fs::read_to_string(&file).await?;
            let brief: Brief = serde_json::from_str(&text)?;
            let mut conn = redis_io::connect().await?;
            let id = redis_io::submit_brief(&mut conn, &brief).await?;
            println!("{{\"submitted\":true,\"brief_id\":\"{}\",\"stream_id\":\"{}\"}}", brief.id, id);
        }
        Cmd::Seed => {
            seed::seed_m0().await?;
            println!("{{\"seeded\":true}}");
        }
        Cmd::Verdicts { count } => {
            let mut conn = redis_io::connect().await?;
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
            let path = permit::key_path();
            if path.exists() && !force {
                eprintln!(
                    "refusing to overwrite existing key at {} (use --force)",
                    path.display()
                );
                std::process::exit(2);
            }
            permit::generate_and_save(&path)?;
            println!(
                "{{\"generated\":true,\"path\":\"{}\"}}",
                path.display()
            );
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
    }
    Ok(())
}
