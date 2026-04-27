//! agentry-workspace — operator triage + GC for per-brief preserved workspaces.
//!
//! Subcommands:
//!   list                  — table of every workspace under the root
//!   path <brief_id>       — print the absolute path of one workspace
//!   gc [--older-than D]   — remove preserved workspaces older than threshold
//!   remove <brief_id>     — manual single-target removal
//!
//! Path resolution is delegated to `BriefWorkspace::root()` — no duplication
//! of the env-var fallback or the `briefs/<id>/` layout. The scan + GC logic
//! lives in `orchestrator_runtime::workspace` so integration tests can call it
//! without spawning a subprocess.

use clap::{Parser, Subcommand};
use orchestrator_runtime::workspace::{self, BriefWorkspace};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "agentry-workspace", version)]
struct Cli {
    /// Override the workspace root. Defaults to `BriefWorkspace::root()`
    /// (`AGENTRY_WORKSPACE_ROOT` env var or the compile-time default).
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List every per-brief workspace currently on disk.
    List,
    /// Print the absolute host path of a brief's workspace.
    /// Exits 1 if no such workspace is on disk.
    Path { brief_id: String },
    /// Remove preserved workspaces older than `--older-than` (default `7d`).
    Gc {
        /// Threshold age, parsed by `humantime` (e.g. `7d`, `12h`, `30m`).
        #[arg(long, default_value = "7d")]
        older_than: String,
        /// Print targets that would be removed without actually removing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a single brief's workspace by id.
    Remove {
        brief_id: String,
        /// Skip the interactive confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let root = cli.root.unwrap_or_else(BriefWorkspace::root);
    if let Err(e) = dispatch(&cli.cmd, &root) {
        eprintln!("agentry-workspace: {e}");
        std::process::exit(1);
    }
}

fn dispatch(cmd: &Cmd, root: &Path) -> Result<(), String> {
    match cmd {
        Cmd::List => cmd_list(root),
        Cmd::Path { brief_id } => cmd_path(root, brief_id),
        Cmd::Gc {
            older_than,
            dry_run,
        } => cmd_gc(root, older_than, *dry_run),
        Cmd::Remove { brief_id, yes } => cmd_remove(root, brief_id, *yes),
    }
}

fn cmd_list(root: &Path) -> Result<(), String> {
    let entries = workspace::scan_workspaces(root);
    println!(
        "{:<40} {:<32} {:>10} {:>14} {:<16}",
        "brief_id", "branch", "age", "disk_usage_mb", "last_verdict"
    );
    for e in entries {
        println!(
            "{:<40} {:<32} {:>10} {:>14} {:<16}",
            e.brief_id,
            e.branch.unwrap_or_else(|| "-".into()),
            format_age(e.age),
            e.disk_usage_bytes / 1_048_576,
            "-"
        );
    }
    Ok(())
}

fn cmd_path(root: &Path, brief_id: &str) -> Result<(), String> {
    let candidate = root.join("briefs").join(brief_id);
    if !candidate.exists() {
        return Err(format!("no workspace for brief_id {brief_id}"));
    }
    println!("{}", candidate.display());
    Ok(())
}

fn cmd_gc(root: &Path, older_than: &str, dry_run: bool) -> Result<(), String> {
    let threshold = humantime::parse_duration(older_than)
        .map_err(|e| format!("parse --older-than {older_than:?}: {e}"))?;
    let targets = workspace::gc_run(root, threshold, dry_run);

    if targets.is_empty() {
        println!("no workspaces older than {older_than}");
        return Ok(());
    }

    for t in &targets {
        if dry_run {
            println!(
                "would remove {} ({})",
                t.entry.brief_id,
                t.entry.path.display()
            );
        } else if t.removed {
            println!("removed {} ({})", t.entry.brief_id, t.entry.path.display());
        } else {
            eprintln!("failed to remove {}", t.entry.path.display());
        }
    }
    Ok(())
}

fn cmd_remove(root: &Path, brief_id: &str, yes: bool) -> Result<(), String> {
    let target = root.join("briefs").join(brief_id);
    if !target.exists() {
        return Err(format!("no workspace for brief_id {brief_id}"));
    }
    if !yes {
        eprint!("Remove {} ? [y/N] ", target.display());
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|e| format!("read confirmation: {e}"))?;
        let trimmed = answer.trim();
        if !trimmed.eq_ignore_ascii_case("y") && !trimmed.eq_ignore_ascii_case("yes") {
            println!("aborted");
            return Ok(());
        }
    }
    std::fs::remove_dir_all(&target).map_err(|e| format!("remove {}: {e}", target.display()))?;
    println!("removed {}", target.display());
    Ok(())
}

fn format_age(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{}h", secs / 3_600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}
