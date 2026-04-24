//! Spawner — abstract container lifecycle; Podman adapter.
//!
//! The Spawner:
//!   1. Accepts a Brief + AgentRole + WorkPermit.
//!   2. Spawns a container on the appropriate substrate.
//!   3. Injects the startup JSON (brief + permit + role) on the container's stdin.
//!   4. Tails stdout as NDJSON `Event`s, mirroring each to the brief's trace stream.
//!   5. On `Done`, appends a Verdict and tears down the container.
//!
//! Only Podman is implemented today; other substrates (Docker, LXC, SSH, VM)
//! will land as sibling adapters implementing the same `Spawner` trait.

use crate::{permit as permit_mod, redis_io, workspace::BriefWorkspace, Error, Result};
use async_trait::async_trait;
use ed25519_dalek::VerifyingKey;
use orchestrator_types::{
    AgentRole, Brief, BriefId, Event, EventKind, PackageManager, Verdict, VerdictKind, WorkPermit,
};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::Serialize;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// Startup bundle passed to the agent via stdin (one JSON document).
#[derive(Serialize)]
pub struct AgentStartup<'a> {
    pub brief: &'a Brief,
    pub role: &'a AgentRole,
    pub permit: &'a WorkPermit,
    /// Messages routed to this role from upstream roles in the same team.
    /// Populated from the team's message_graph + accumulated Message events.
    pub team_context: &'a TeamContext,
}

/// Per-brief, per-role context delivered on stdin. Accumulates in the daemon
/// as upstream roles emit `Message` events.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct TeamContext {
    pub messages: Vec<RoutedMessage>,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct RoutedMessage {
    pub from: String,
    pub to: String,
    pub payload: serde_json::Value,
    pub at: chrono::DateTime<chrono::Utc>,
}

/// Handle returned by the spawner; used for teardown.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    pub agent_id: String,
    pub container_name: String,
}

/// The spawner abstraction.
/// Outcome of running one role.
pub struct AgentOutcome {
    pub handle: AgentHandle,
    pub verdict: Verdict,
    /// Messages the agent emitted; the daemon routes these to downstream roles.
    pub outbox: Vec<RoutedMessage>,
}

/// Borrowed bundle of inputs to `Spawner::run_agent`, so the trait method
/// keeps a two-arg shape (context + connection). Construct at the daemon,
/// destructure at the spawner.
pub struct RunAgentCtx<'a> {
    pub brief: &'a Brief,
    pub role: &'a AgentRole,
    pub permit: &'a WorkPermit,
    pub verifying_key: &'a VerifyingKey,
    pub team_context: &'a TeamContext,
    /// Per-brief workspace. Bind-mounted into the container when
    /// `role.workspace_mount.is_some()`. `None` is valid for briefs whose
    /// team has no workspace-using roles.
    pub workspace: Option<&'a BriefWorkspace>,
}

#[async_trait]
pub trait Spawner: Send + Sync {
    /// Run the agent fully: spawn, pipe stdin, tail stdout to trace, enforce
    /// permit on tool-call events, route messages, write verdict, tear down.
    async fn run_agent(
        &self,
        ctx: RunAgentCtx<'_>,
        conn: &mut ConnectionManager,
    ) -> Result<AgentOutcome>;
}

/// Podman spawner.
pub struct PodmanSpawner;

impl PodmanSpawner {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    fn container_name(agent_id: &str) -> String {
        format!("agentry-{agent_id}")
    }
}

impl Default for PodmanSpawner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Spawner for PodmanSpawner {
    async fn run_agent(
        &self,
        ctx: RunAgentCtx<'_>,
        conn: &mut ConnectionManager,
    ) -> Result<AgentOutcome> {
        let RunAgentCtx {
            brief,
            role,
            permit,
            verifying_key,
            team_context,
            workspace,
        } = ctx;
        // Defence in depth: verify the permit we're about to hand out.
        permit_mod::verify(permit, verifying_key)?;

        let agent_id = &permit.agent_id;
        let name = Self::container_name(agent_id);

        let startup = AgentStartup {
            brief,
            role,
            permit,
            team_context,
        };
        let startup_json = serde_json::to_string(&startup)?;

        tracing::info!(
            brief = %brief.id,
            role = %role.name,
            agent = %agent_id,
            image = %role.image,
            "spawning container"
        );

        let mut cmd = Command::new("podman");
        cmd.arg("run")
            .arg("--rm")
            .arg("-i")
            // Stock public base images (alpine:3.21, debian:bookworm-slim) are
            // pulled once and cached. `missing` downloads only when absent;
            // subsequent spawns reuse the cached layer.
            .arg("--pull=missing")
            // Every agentry-spawned container joins the shared `agentry-net`
            // network so roles can reach the sccache-redis (and future
            // per-brief services) by container name. The network is created
            // out-of-band via `just agentry-net-up` — the spawn fails loudly
            // if it doesn't exist.
            .arg("--network=agentry-net")
            .arg("--name")
            .arg(&name)
            .arg("--label")
            .arg(format!("agentry.brief={}", brief.id))
            .arg("--label")
            .arg(format!("agentry.role={}", role.name))
            .arg("--label")
            .arg(format!("agentry.agent={agent_id}"));
        // sccache wiring — when declared, route all Rust compilations through
        // the shared sccache-redis. The `sccache` binary is auto-added to the
        // install list below (not the role's responsibility). Endpoint uses
        // the podman-network DNS name, not the host port.
        if role.sccache {
            cmd.arg("--env").arg("RUSTC_WRAPPER=sccache");
            cmd.arg("--env")
                .arg("SCCACHE_REDIS_ENDPOINT=redis://agentry-sccache-redis:6379");
            cmd.arg("--env").arg("SCCACHE_REDIS_KEY_PREFIX=agentry");
        }
        // Pass through declared env vars from orchestratord's own env.
        // Missing vars are logged and skipped — role doesn't get what it wanted.
        for var_name in &role.passthru_env {
            match std::env::var(var_name) {
                Ok(val) => {
                    cmd.arg("--env").arg(format!("{var_name}={val}"));
                }
                Err(_) => {
                    tracing::warn!(
                        role = %role.name,
                        env = %var_name,
                        "passthru env not set in orchestratord; skipped"
                    );
                }
            }
        }
        // Bind mounts: `-v source:target[:ro]`. When mounts are declared,
        // disable SELinux label translation — otherwise rootless podman on
        // Fedora/Silverblue can't read host-owned files (EACCES). `:z`/`:Z`
        // mount flags would relabel the SOURCE on the host, which is worse.
        let role_wants_workspace = role.workspace_mount.is_some();
        if !role.mounts.is_empty() || role_wants_workspace {
            cmd.arg("--security-opt").arg("label=disable");
        }
        for m in &role.mounts {
            let spec = if m.readonly {
                format!("{}:{}:ro", m.source, m.target)
            } else {
                format!("{}:{}", m.source, m.target)
            };
            cmd.arg("-v").arg(spec);
        }
        // Workspace mount: when the role declares a `workspace_mount`, bind
        // the brief's host workspace at the declared container path. If the
        // role wants one but the daemon didn't allocate (configuration bug),
        // fail fast rather than silently running without the workspace.
        if let Some(wm) = &role.workspace_mount {
            let ws = workspace.ok_or_else(|| {
                Error::Config(format!(
                    "role '{}' declares workspace_mount but no workspace was allocated for brief {}",
                    role.name, brief.id
                ))
            })?;
            let spec = if wm.readonly {
                format!("{}:{}:ro", ws.host_path.display(), wm.container_path)
            } else {
                format!("{}:{}", ws.host_path.display(), wm.container_path)
            };
            cmd.arg("-v").arg(spec);
        }

        // Deliver the inline entrypoint script via the AGENTRY_SCRIPT env var
        // and override the image command with a bootstrap that installs
        // `binaries` via the declared package manager and execs the script.
        cmd.arg("--env")
            .arg(format!("AGENTRY_SCRIPT={}", role.entrypoint_script));
        cmd.arg(&role.image);
        let effective_binaries = effective_binaries(role);
        cmd.arg("sh")
            .arg("-c")
            .arg(bootstrap_command(role.package_manager, &effective_binaries));

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child: Child = cmd.spawn().map_err(|e| Error::Spawn(e.to_string()))?;

        // Feed startup bundle to stdin, then close.
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(startup_json.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.shutdown().await.ok();
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Spawn("no stdout".into()))?;
        let mut reader = BufReader::new(stdout).lines();

        // The read loop is wrapped in `tokio::time::timeout(permit.max_wall_seconds)`
        // so a hung agent (network stall, runaway script) cannot stall the daemon.
        // The inner future owns `reader` and mutably borrows `conn`; on elapsed
        // the future is dropped, releasing both, and we `podman stop` the
        // container by name to unblock `child.wait()` below.
        let read_fut = async {
            let mut terminal: Option<Event> = None;
            let mut permit_violation: Option<String> = None;
            let mut outbox: Vec<RoutedMessage> = Vec::new();
            while let Some(line) = reader.next_line().await? {
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Event>(&line) {
                    Ok(ev) => {
                        // Enforce permit on tool calls: any call outside the
                        // allowlist OR outside the narrowed fs scope kills the
                        // container.
                        if let EventKind::ToolCall { call } = &ev.kind {
                            append_audit(conn, &brief.id, agent_id, &call.tool, &call.args)
                                .await
                                .ok();
                            if let Err(reason) =
                                orchestrator_types::check_tool_call(permit, &call.tool, &call.args)
                            {
                                tracing::warn!(
                                    brief = %brief.id,
                                    agent = %agent_id,
                                    tool = %call.tool,
                                    reason = %reason,
                                    "permit violation — killing container"
                                );
                                permit_violation = Some(reason);
                                redis_io::append_trace(conn, &brief.id, agent_id, &ev).await?;
                                let _ = tokio::process::Command::new("podman")
                                    .args(["stop", "-t", "1", &name])
                                    .output()
                                    .await;
                                break;
                            }
                        }
                        // Collect outbox messages for downstream routing.
                        if let EventKind::Message { to, payload } = &ev.kind {
                            outbox.push(RoutedMessage {
                                from: role.name.0.clone(),
                                to: to.clone(),
                                payload: payload.clone(),
                                at: ev.at,
                            });
                        }
                        redis_io::append_trace(conn, &brief.id, agent_id, &ev).await?;
                        if ev.is_terminal() {
                            terminal = Some(ev);
                            break;
                        }
                    }
                    Err(err) => {
                        tracing::warn!(line=%line, error=%err, "skipped malformed event");
                    }
                }
            }
            Ok::<(Option<Event>, Option<String>, Vec<RoutedMessage>), Error>((
                terminal,
                permit_violation,
                outbox,
            ))
        };

        let (timed_out, terminal, permit_violation, outbox) = match permit.max_wall_seconds {
            Some(secs) => match tokio::time::timeout(Duration::from_secs(secs), read_fut).await {
                Ok(Ok((t, pv, ob))) => (false, t, pv, ob),
                Ok(Err(e)) => return Err(e),
                Err(_elapsed) => {
                    tracing::warn!(
                        brief = %brief.id,
                        agent = %agent_id,
                        seconds = secs,
                        "wall-clock budget exceeded — stopping container"
                    );
                    let _ = tokio::process::Command::new("podman")
                        .args(["stop", "-t", "1", &name])
                        .output()
                        .await;
                    (true, None, None, Vec::new())
                }
            },
            None => {
                let (t, pv, ob) = read_fut.await?;
                (false, t, pv, ob)
            }
        };

        // Capture stderr (diagnostic only — not mirrored to trace).
        if let Some(mut stderr) = child.stderr.take() {
            let mut buf = Vec::new();
            use tokio::io::AsyncReadExt;
            stderr.read_to_end(&mut buf).await.ok();
            if !buf.is_empty() {
                tracing::debug!(stderr = %String::from_utf8_lossy(&buf), "agent stderr");
            }
        }

        let status = child.wait().await?;

        let verdict = compute_verdict(
            &brief.id,
            timed_out,
            permit_violation.as_deref(),
            terminal.as_ref().and_then(Event::verdict),
            status.code(),
        );

        Ok(AgentOutcome {
            handle: AgentHandle {
                agent_id: agent_id.clone(),
                container_name: name,
            },
            verdict,
            outbox,
        })
    }
}

/// Build the team-level `Verdict` from the spawner's observed outcomes.
/// Pure function so the verdict-selection logic is unit-testable without
/// spawning a container.
fn compute_verdict(
    brief_id: &BriefId,
    timed_out: bool,
    permit_violation: Option<&str>,
    terminal_event: Option<orchestrator_types::EventVerdict>,
    exit_code: Option<i32>,
) -> Verdict {
    let (kind, reason) = if timed_out {
        (
            VerdictKind::Failed,
            Some("wall-clock budget exceeded".to_string()),
        )
    } else if let Some(r) = permit_violation {
        (VerdictKind::PermitViolation, Some(r.to_string()))
    } else {
        match terminal_event {
            Some(v) => (VerdictKind::from(v), None),
            None => (
                VerdictKind::Failed,
                Some(format!(
                    "agent exited without done event (code={exit_code:?})"
                )),
            ),
        }
    };
    let v = Verdict::new(brief_id.clone(), kind);
    if let Some(r) = reason {
        v.with_reason(r)
    } else {
        v
    }
}

/// Append a tool-call entry to the brief's audit stream (independent from the trace).
/// Audit is tamper-evident (append-only) and contains what was asked for, regardless
/// of whether the broker allowed it. Used by dashboards + post-hoc review.
async fn append_audit(
    conn: &mut ConnectionManager,
    brief: &BriefId,
    agent_id: &str,
    tool: &str,
    args: &serde_json::Value,
) -> Result<()> {
    let stream = format!("agentry:brief:{}:audit", brief.0);
    let args_str = serde_json::to_string(args)?;
    let _: String = conn
        .xadd(
            &stream,
            "*",
            &[
                ("agent", agent_id),
                ("tool", tool),
                ("args", args_str.as_str()),
            ],
        )
        .await?;
    Ok(())
}

/// Build the `sh -c` argument that the container runs as its command.
///
/// Installs a baseline (`bash ca-certificates coreutils jq`) plus role-specific
/// `binaries` via the declared `package_manager`, then execs the script
/// delivered via the `AGENTRY_SCRIPT` env var.
/// Merge the role's declared `binaries` with any implicit extras derived
/// from other role fields. Today: `sccache=true` adds the `sccache` package
/// to the install list so it can run as `RUSTC_WRAPPER`.
fn effective_binaries(role: &AgentRole) -> Vec<String> {
    let mut out = role.binaries.clone();
    if role.sccache && !out.iter().any(|b| b == "sccache") {
        out.push("sccache".into());
    }
    out
}

fn bootstrap_command(pm: PackageManager, extra_binaries: &[String]) -> String {
    const BASELINE: &[&str] = &["bash", "ca-certificates", "coreutils", "jq"];
    let all: Vec<&str> = BASELINE
        .iter()
        .copied()
        .chain(extra_binaries.iter().map(String::as_str))
        .collect();
    let pkgs = all.join(" ");
    let install = match pm {
        PackageManager::Apk => format!("apk add --no-cache {pkgs} >/dev/null"),
        PackageManager::Apt => format!(
            "apt-get update -qq >/dev/null && apt-get install -y --no-install-recommends {pkgs} >/dev/null"
        ),
    };
    // $AGENTRY_SCRIPT is passed as an env var by the spawner. `bash -c` runs
    // it as a script; the script's own `cat` still reads the startup JSON
    // bundle from stdin (not affected by the outer bootstrap).
    format!("set -e\n{install}\nexec bash -c \"$AGENTRY_SCRIPT\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator_types::EventVerdict;

    fn bid() -> BriefId {
        BriefId("brf_test".into())
    }

    #[test]
    fn container_name_format() {
        let n = PodmanSpawner::container_name("agt_abcd");
        assert_eq!(n, "agentry-agt_abcd");
    }

    #[test]
    fn verdict_timeout_beats_everything() {
        // Even if a permit_violation or terminal event were observed, a
        // timeout signal must dominate — timeouts indicate the agent did not
        // complete within budget, which trumps any partial signal.
        let v = compute_verdict(
            &bid(),
            true,
            Some("tried to write denied path"),
            Some(EventVerdict::Shipped),
            Some(137),
        );
        assert!(matches!(v.kind, VerdictKind::Failed));
        assert_eq!(v.reason.as_deref(), Some("wall-clock budget exceeded"));
    }

    #[test]
    fn verdict_permit_violation_when_no_timeout() {
        let v = compute_verdict(
            &bid(),
            false,
            Some("unauthorized tool call: write"),
            None,
            None,
        );
        assert!(matches!(v.kind, VerdictKind::PermitViolation));
        assert_eq!(v.reason.as_deref(), Some("unauthorized tool call: write"));
    }

    #[test]
    fn verdict_from_terminal_event() {
        let v = compute_verdict(&bid(), false, None, Some(EventVerdict::Shipped), Some(0));
        assert!(matches!(v.kind, VerdictKind::Shipped));
        assert!(v.reason.is_none());
    }

    fn sample_role(sccache: bool, binaries: Vec<&str>) -> AgentRole {
        AgentRole {
            name: orchestrator_types::RoleName("probe".into()),
            version: 1,
            model: None,
            system_prompt: None,
            image: "alpine:3.21".into(),
            substrate_class: orchestrator_types::SubstrateClass::Podman,
            package_manager: PackageManager::Apk,
            entrypoint_script: "#!/usr/bin/env bash\nexit 0\n".into(),
            binaries: binaries.into_iter().map(String::from).collect(),
            mcp_servers: vec![],
            tool_allowlist: orchestrator_types::ToolAllowlist::default(),
            permit_scope: orchestrator_types::PermitScope::default(),
            passthru_env: vec![],
            mounts: vec![],
            workspace_mount: None,
            sccache,
        }
    }

    #[test]
    fn effective_binaries_adds_sccache_when_enabled() {
        let r = sample_role(true, vec!["git", "curl"]);
        let eff = effective_binaries(&r);
        assert!(eff.iter().any(|b| b == "sccache"));
        assert_eq!(eff.len(), 3);
    }

    #[test]
    fn effective_binaries_no_sccache_when_disabled() {
        let r = sample_role(false, vec!["git"]);
        let eff = effective_binaries(&r);
        assert!(!eff.iter().any(|b| b == "sccache"));
        assert_eq!(eff.len(), 1);
    }

    #[test]
    fn effective_binaries_no_duplicate_when_role_already_has_sccache() {
        let r = sample_role(true, vec!["sccache"]);
        let eff = effective_binaries(&r);
        assert_eq!(eff.iter().filter(|b| b.as_str() == "sccache").count(), 1);
    }

    #[test]
    fn verdict_failed_without_done_event() {
        // Container exited but never emitted `done` — Failed with exit code in reason.
        let v = compute_verdict(&bid(), false, None, None, Some(1));
        assert!(matches!(v.kind, VerdictKind::Failed));
        let reason = v.reason.expect("reason required");
        assert!(reason.contains("agent exited without done event"));
        assert!(reason.contains("1"), "exit code surfaced: {reason}");
    }

    #[test]
    fn bootstrap_apk_installs_baseline_plus_extras() {
        let s = bootstrap_command(PackageManager::Apk, &["git".into(), "curl".into()]);
        assert!(s.contains("apk add --no-cache"));
        assert!(s.contains("bash"));
        assert!(s.contains("coreutils"));
        assert!(s.contains("jq"));
        assert!(s.contains("git"));
        assert!(s.contains("curl"));
        assert!(s.contains("exec bash -c \"$AGENTRY_SCRIPT\""));
    }

    #[test]
    fn bootstrap_apt_uses_apt_get() {
        let s = bootstrap_command(PackageManager::Apt, &[]);
        assert!(s.contains("apt-get update"));
        assert!(s.contains("apt-get install -y --no-install-recommends"));
        assert!(s.contains("exec bash -c \"$AGENTRY_SCRIPT\""));
    }
}
