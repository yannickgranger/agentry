//! captain — captain-side authoring CLI.
//!
//! `captain new-brief` emits a typed Brief JSON template to stdout, with
//! `kind` + `contract` placed at the correct top-level fields. Authoring a
//! brief from this template (instead of from memory) prevents the silent
//! "field nested under payload" failure mode and the stale-binary "field
//! dropped on re-serialize" failure mode: the template is produced by a
//! binary that imports `orchestrator_types::Brief` and round-trips through
//! serde_json, so the field names and locations always match the schema the
//! daemon parses.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use orchestrator_runtime::captain_ground::{render_grounding_sheet, CfdbItem, SpecMatch};
use orchestrator_types::{
    now, Assertion, AssertionAnchor, AssertionId, Brief, BriefId, Budget, Contract, EscalationMode,
    TaskShape, VersionedRef,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;

#[derive(Parser, Debug)]
#[command(name = "captain", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Emit a typed Brief JSON template to stdout.
    NewBrief {
        /// Task shape (kebab-case wire form, e.g. `trivial-doc`, `bug-fix`).
        #[arg(long)]
        kind: String,
        /// Forge shorthand for the target repo, e.g. `yg/agentry`.
        #[arg(long)]
        target_repo: String,
        /// Topology reference, formatted `name:version` (e.g.
        /// `agentry-bugfix-v0:1`).
        #[arg(long)]
        topology: String,
        /// PR title used in the emitted payload.
        #[arg(long)]
        pr_title: String,
        /// Issue title used in the emitted payload.
        #[arg(long)]
        issue_title: String,
        /// Optional brief id. Defaults to `BriefId::fresh()`.
        #[arg(long)]
        id: Option<String>,
        /// Optional issue body. Default: a one-line placeholder.
        #[arg(long)]
        issue_body: Option<String>,
        /// Optional PR body. Default: a one-line placeholder.
        #[arg(long)]
        pr_body: Option<String>,
        /// Optional acceptance command. Default: the agentry self-host
        /// acceptance.
        #[arg(long)]
        acceptance: Option<String>,
        /// Optional base branch. Default: `develop`.
        #[arg(long)]
        base_branch: Option<String>,
        /// Optional submitter id. Default: `captain-cli`.
        #[arg(long)]
        submitted_by: Option<String>,
    },
    /// Emit a grounding sheet for a directive by combining cfdb and specs.
    Ground {
        /// Target repo workspace root.
        #[arg(long)]
        workspace: PathBuf,
        /// Free-form directive describing the intended brief.
        #[arg(long)]
        directive: String,
        /// openCypher regex applied to cfdb item names.
        #[arg(long)]
        name_pattern: String,
        /// Optional comma-separated cfdb item kinds (passed through to cfdb).
        #[arg(long)]
        kinds: Option<String>,
        /// Optional specs directory; defaults to `<workspace>/specs/concepts`.
        #[arg(long)]
        specs_dir: Option<PathBuf>,
        /// Optional pre-extracted cfdb db dir. When omitted, captain runs
        /// `cfdb extract` into a tempdir. When given, captain assumes the
        /// keyspace name is `ground` inside that db directory.
        #[arg(long)]
        keyspace_db: Option<PathBuf>,
    },
}

fn parse_kind(s: &str) -> Result<TaskShape> {
    serde_json::from_value::<TaskShape>(Value::String(s.to_string()))
        .with_context(|| format!("unknown --kind value: {s}"))
}

fn parse_topology(s: &str) -> Result<VersionedRef> {
    let (name, version) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("--topology must be of the form name:version (got `{s}`)"))?;
    if name.is_empty() {
        return Err(anyhow!("--topology name component is empty in `{s}`"));
    }
    let version: u32 = version
        .parse()
        .with_context(|| format!("--topology version must be a u32 (got `{version}`)"))?;
    Ok(VersionedRef::new(name, version))
}

const DEFAULT_PLACEHOLDER_BODY: &str =
    "TODO: replace with verb-structured CREATE / UPDATE / REPLACE / DELETE / MOVE lines.";
const DEFAULT_ACCEPTANCE: &str =
    "cargo run -p quality-fast --bin quality-mech --release --quiet && bash scripts/arch-check.sh";
const DEFAULT_BASE_BRANCH: &str = "develop";
const DEFAULT_SUBMITTED_BY: &str = "captain-cli";
const TODO_QNAME: &str = "TODO::replace_with_real_qname";

fn stub_contract(brief_id: &BriefId) -> Contract {
    Contract {
        brief_id: brief_id.clone(),
        assertions: vec![Assertion {
            id: AssertionId("A1".into()),
            prose: "TODO: fill in assertion prose; the anchor below points at a TODO marker that you MUST replace with a real cfdb qname or spec section before dispatching.".into(),
            anchor: AssertionAnchor::Cfdb {
                qname: TODO_QNAME.into(),
            },
        }],
        precursor_artifacts: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_brief(
    kind_str: &str,
    target_repo: &str,
    topology_str: &str,
    pr_title: &str,
    issue_title: &str,
    id: Option<String>,
    issue_body: Option<String>,
    pr_body: Option<String>,
    acceptance: Option<String>,
    base_branch: Option<String>,
    submitted_by: Option<String>,
) -> Result<Brief> {
    let kind = parse_kind(kind_str)?;
    let topology = parse_topology(topology_str)?;
    let id = id.map_or_else(BriefId::fresh, BriefId);

    let payload = json!({
        "issue_number": Value::Null,
        "issue_title": issue_title,
        "issue_body": issue_body.unwrap_or_else(|| DEFAULT_PLACEHOLDER_BODY.to_string()),
        "acceptance": acceptance.unwrap_or_else(|| DEFAULT_ACCEPTANCE.to_string()),
        "target_repo": target_repo,
        "base_branch": base_branch.unwrap_or_else(|| DEFAULT_BASE_BRANCH.to_string()),
        "pr_title": pr_title,
        "pr_body": pr_body.unwrap_or_else(|| DEFAULT_PLACEHOLDER_BODY.to_string()),
    });

    let contract = if kind.requires_contract() {
        Some(stub_contract(&id))
    } else {
        None
    };

    Ok(Brief {
        id,
        project: None,
        topology,
        payload,
        kind: Some(kind),
        contract,
        budget: Budget::default(),
        escalation: EscalationMode::Autonomous,
        parent_brief: None,
        cohort_labels: Vec::new(),
        submitted_by: submitted_by.unwrap_or_else(|| DEFAULT_SUBMITTED_BY.to_string()),
        submitted_at: now(),
    })
}

#[derive(Debug, Deserialize)]
struct CfdbListResponse {
    rows: Vec<CfdbItem>,
}

fn cmd_ground(
    workspace: PathBuf,
    directive: String,
    name_pattern: String,
    kinds: Option<String>,
    specs_dir: Option<PathBuf>,
    keyspace_db: Option<PathBuf>,
) -> Result<()> {
    let workspace_str = workspace.display().to_string();

    let _tempdir_guard;
    let db_path: PathBuf = match keyspace_db {
        Some(p) => p,
        None => {
            let tmp =
                tempfile::TempDir::new().context("failed to allocate tempdir for cfdb extract")?;
            let tmp_str = tmp.path().display().to_string();
            let extract_out = Command::new("cfdb")
                .args([
                    "extract",
                    "--workspace",
                    &workspace_str,
                    "--db",
                    &tmp_str,
                    "--keyspace",
                    "ground",
                ])
                .output()
                .context("failed to spawn cfdb extract")?;
            if !extract_out.status.success() {
                let code = extract_out
                    .status
                    .code()
                    .map_or_else(|| "?".to_string(), |c| c.to_string());
                let stderr = String::from_utf8_lossy(&extract_out.stderr);
                return Err(anyhow!("cfdb extract failed (status {code}): {stderr}"));
            }
            let path = tmp.path().to_path_buf();
            _tempdir_guard = Some(tmp);
            path
        }
    };

    let db_str = db_path.display().to_string();
    let mut list_args: Vec<String> = vec![
        "list-items-matching".to_string(),
        "--db".to_string(),
        db_str,
        "--keyspace".to_string(),
        "ground".to_string(),
        "--name-pattern".to_string(),
        name_pattern,
    ];
    if let Some(k) = kinds {
        list_args.push("--kinds".to_string());
        list_args.push(k);
    }
    let list_out = Command::new("cfdb")
        .args(&list_args)
        .output()
        .context("failed to spawn cfdb list-items-matching")?;
    if !list_out.status.success() {
        let code = list_out
            .status
            .code()
            .map_or_else(|| "?".to_string(), |c| c.to_string());
        let stderr = String::from_utf8_lossy(&list_out.stderr);
        return Err(anyhow!(
            "cfdb list-items-matching failed (status {code}): {stderr}"
        ));
    }
    let parsed: CfdbListResponse = serde_json::from_slice(&list_out.stdout)
        .context("failed to parse cfdb list-items-matching output as JSON")?;
    let items = parsed.rows;

    let resolved_specs_dir = specs_dir.unwrap_or_else(|| workspace.join("specs/concepts"));
    let mut spec_matches: Vec<SpecMatch> = Vec::new();
    if !resolved_specs_dir.is_dir() {
        eprintln!(
            "// WARN: specs dir not found at {}",
            resolved_specs_dir.display()
        );
    } else {
        let directive_tokens: Vec<String> = directive
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let entries = std::fs::read_dir(&resolved_specs_dir).with_context(|| {
            format!("failed to read specs dir {}", resolved_specs_dir.display())
        })?;
        let mut paths: Vec<PathBuf> = entries
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
            .collect();
        paths.sort();
        for path in paths {
            let body = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let mut matched_headings: Vec<String> = Vec::new();
            for line in body.lines() {
                let trimmed = line.trim_start();
                let hash_count = trimmed.chars().take_while(|c| *c == '#').count();
                if hash_count == 0 {
                    continue;
                }
                let after_hashes = &trimmed[hash_count..];
                if !after_hashes.starts_with(' ') {
                    continue;
                }
                let heading = after_hashes[1..].trim_end().to_string();
                let lower = heading.to_lowercase();
                if directive_tokens.iter().any(|tok| lower.contains(tok)) {
                    matched_headings.push(heading);
                }
            }
            if !matched_headings.is_empty() {
                let display_path = match path.strip_prefix(&workspace) {
                    Ok(rel) => rel.display().to_string(),
                    Err(_) => path.display().to_string(),
                };
                spec_matches.push(SpecMatch {
                    file: display_path,
                    headings: matched_headings,
                });
            }
        }
    }

    let sheet = render_grounding_sheet(&directive, &items, &spec_matches);
    println!("{sheet}");
    if items.is_empty() && spec_matches.is_empty() {
        eprintln!(
            "// WARN: no cfdb matches and no spec matches; consider broadening --name-pattern or --directive"
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::NewBrief {
            kind,
            target_repo,
            topology,
            pr_title,
            issue_title,
            id,
            issue_body,
            pr_body,
            acceptance,
            base_branch,
            submitted_by,
        } => {
            let brief = build_brief(
                &kind,
                &target_repo,
                &topology,
                &pr_title,
                &issue_title,
                id,
                issue_body,
                pr_body,
                acceptance,
                base_branch,
                submitted_by,
            )?;
            println!("{}", serde_json::to_string_pretty(&brief)?);
            if brief.contract.is_some() {
                eprintln!(
                    "// REMINDER: replace `{TODO_QNAME}` and the placeholder assertion prose with a real anchor before dispatching."
                );
            }
        }
        Cmd::Ground {
            workspace,
            directive,
            name_pattern,
            kinds,
            specs_dir,
            keyspace_db,
        } => {
            cmd_ground(
                workspace,
                directive,
                name_pattern,
                kinds,
                specs_dir,
                keyspace_db,
            )?;
        }
    }
    Ok(())
}
