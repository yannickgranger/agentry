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

use crate::{delivery, permit as permit_mod, redis_io, workspace::BriefWorkspace, Error, Result};
use async_trait::async_trait;
use ed25519_dalek::VerifyingKey;
use orchestrator_types::{
    AgentRole, Brief, BriefId, Event, EventKind, PackageManager, Verdict, VerdictKind, WorkPermit,
};
use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// Process-wide registry of running role containers, keyed by `BriefId`.
///
/// The dashboard's `POST /briefs/{id}/kill` and `GET
/// /briefs/{id}/workspace/path` routes look up the container handle here to
/// signal SIGTERM or surface the workspace path. Guarded by a `RwLock` so
/// reads (the common case from the dashboard) don't block the daemon's
/// concurrent spawns.
///
/// CRITICAL: every insert-on-spawn is paired with a `Drop`-fired removal via
/// `RegistrationGuard`. A manual `unregister_running` call positioned after
/// `child.wait()` would leak the entry on any `?`-bubbled error between
/// spawn and wait.
#[derive(Debug, Clone)]
pub struct ContainerHandle {
    pub container_name: String,
    pub workspace_path: Option<PathBuf>,
}

fn registry() -> &'static RwLock<HashMap<BriefId, ContainerHandle>> {
    static R: OnceLock<RwLock<HashMap<BriefId, ContainerHandle>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(HashMap::new()))
}

fn register_running(brief_id: &BriefId, handle: ContainerHandle) {
    let mut g = registry()
        .write()
        .expect("running-container registry poisoned");
    g.insert(brief_id.clone(), handle);
}

fn unregister_running(brief_id: &BriefId) {
    let mut g = registry()
        .write()
        .expect("running-container registry poisoned");
    g.remove(brief_id);
}

/// RAII guard: registers the container on construction, removes the entry
/// on `Drop`. Holding the guard across the spawn-to-wait window guarantees
/// the registry never leaks an entry, even when an early `?` returns out of
/// the spawner.
struct RegistrationGuard {
    brief_id: BriefId,
}

impl RegistrationGuard {
    fn new(brief_id: BriefId, handle: ContainerHandle) -> Self {
        register_running(&brief_id, handle);
        Self { brief_id }
    }
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        unregister_running(&self.brief_id);
    }
}

/// SIGTERM the running container associated with `brief_id`, returning
/// `Ok(())` on signaled, `Error::NotFound` if no container is registered, or
/// a Podman error if the kill itself fails. The container's exitpoint
/// script (when configured) runs.
pub async fn kill(brief_id: &BriefId) -> Result<()> {
    let name = {
        let g = registry()
            .read()
            .expect("running-container registry poisoned");
        g.get(brief_id).map(|h| h.container_name.clone())
    };
    let name = name.ok_or_else(|| Error::NotFound {
        kind: "running container",
        key: brief_id.0.clone(),
    })?;
    let out = tokio::process::Command::new("podman")
        .args(["kill", "--signal", "SIGTERM", &name])
        .output()
        .await
        .map_err(|e| Error::Podman(format!("kill {name}: {e}")))?;
    if !out.status.success() {
        return Err(Error::Podman(format!(
            "podman kill {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

/// Look up the host workspace path of a brief's running container.
/// Returns `None` if the brief has no live container, or if the container
/// runs without a workspace mount.
#[must_use]
pub fn workspace_path(brief_id: &BriefId) -> Option<PathBuf> {
    let g = registry()
        .read()
        .expect("running-container registry poisoned");
    g.get(brief_id).and_then(|h| h.workspace_path.clone())
}

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
#[derive(Clone)]
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

        // Announce the spawn on the trace stream so the projector can
        // materialize a row in the agent-state store. The projector discovers
        // brief streams by polling `agentry:projector:streams`; sadd on every
        // spawn is idempotent and cheap.
        let spawn_ev = Event::new(EventKind::Event {
            payload: serde_json::json!({
                "agent_event": "spawned",
                "brief_id": brief.id.0,
                "role_name": role.name.0,
                "project": brief.project,
                "cohort_labels": brief.cohort_labels,
                "started_at": chrono::Utc::now().to_rfc3339(),
            }),
        });
        redis_io::append_trace(conn, &brief.id, agent_id, &spawn_ev).await?;
        let _: () = conn
            .sadd("agentry:projector:streams", brief.id.0.as_str())
            .await?;

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
            // ra-query is operator-installed via `just ra-query-binary`. A
            // missing host binary must NOT block the reviewer-claude spawn —
            // the entrypoint script falls back to `command -v ra-query` and
            // skips the pre-pass with a `ra_query_unavailable` event. Other
            // mounts (claude, credentials, settings) keep podman's default
            // fail-fast behaviour: a missing source surfaces as a spawn error.
            if role.name.0 == "reviewer-claude-agentry"
                && m.target == "/usr/local/bin/ra-query"
                && !std::path::Path::new(&m.source).exists()
            {
                tracing::warn!(
                    role = %role.name,
                    path = %m.source,
                    "ra-query host binary missing at {}; reviewer pre-pass will be skipped — run 'just ra-query-binary' on the host",
                    m.source
                );
                continue;
            }
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

            // If the workspace is a git worktree, its `.git` is a FILE
            // containing a `gitdir:` pointer to `<root>/.clones/.../worktrees/<name>/`.
            // That host-absolute path must resolve INSIDE the container too,
            // or every git operation fails with `fatal: not a git repository`
            // and `set -euo pipefail` kills the script at the first git call.
            // Bind-mount the `.clones/` root at its own host path so the
            // pointer resolves. Read-write because git writes worktree admin
            // files (HEAD, logs/HEAD) on every commit. Contents are public
            // forge objects — no secrets live in the bare clone's config.
            let dotgit = ws.host_path.join(".git");
            if tokio::fs::metadata(&dotgit)
                .await
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                let clones_root = crate::workspace::BriefWorkspace::root().join(".clones");
                if clones_root.exists() {
                    let spec = format!("{}:{}", clones_root.display(), clones_root.display());
                    cmd.arg("-v").arg(spec);
                    tracing::debug!(
                        brief = %brief.id,
                        role = %role.name,
                        clones = %clones_root.display(),
                        "bind-mounted bare-clone root for worktree"
                    );
                }
            }
        }

        // Deliver the inline entrypoint script via the AGENTRY_SCRIPT env var
        // and override the image command with a bootstrap that installs
        // `binaries` via the declared package manager and execs the script.
        cmd.arg("--env")
            .arg(format!("AGENTRY_SCRIPT={}", role.entrypoint_script));
        if let Some(ep) = &role.exitpoint_script {
            cmd.arg("--env").arg(format!("AGENTRY_EXITPOINT={ep}"));
        }
        cmd.arg(&role.image);
        let effective_binaries = effective_binaries(role);
        cmd.arg("sh").arg("-c").arg(bootstrap_command(
            role.package_manager,
            &effective_binaries,
            &role.extra_bootstrap,
            role.exitpoint_script.is_some(),
        ));

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child: Child = cmd.spawn().map_err(|e| Error::Spawn(e.to_string()))?;

        // Register the running container so the dashboard can address it
        // (kill, workspace-path lookup). The guard's Drop unregisters on
        // every exit path of this scope — including any `?`-bubbled error
        // before `child.wait()` returns.
        let _registration = RegistrationGuard::new(
            brief.id.clone(),
            ContainerHandle {
                container_name: name.clone(),
                workspace_path: workspace.map(|w| w.host_path.clone()),
            },
        );

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
            let mut findings: Vec<orchestrator_types::ReviewFinding> = Vec::new();
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
                                let _ = delivery::record(conn, &brief.id, agent_id, &ev).await;
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
                        // Accumulate findings for attachment to ReworkNeeded verdict.
                        if let EventKind::Finding { finding } = &ev.kind {
                            findings.push(finding.clone());
                        }
                        redis_io::append_trace(conn, &brief.id, agent_id, &ev).await?;
                        let _ = delivery::record(conn, &brief.id, agent_id, &ev).await;
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
            Ok::<
                (
                    Option<Event>,
                    Option<String>,
                    Vec<RoutedMessage>,
                    Vec<orchestrator_types::ReviewFinding>,
                ),
                Error,
            >((terminal, permit_violation, outbox, findings))
        };

        let (timed_out, terminal, permit_violation, outbox, findings) = match permit
            .max_wall_seconds
        {
            Some(secs) => match tokio::time::timeout(Duration::from_secs(secs), read_fut).await {
                Ok(Ok((t, pv, ob, fi))) => (false, t, pv, ob, fi),
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
                    (true, None, None, Vec::new(), Vec::new())
                }
            },
            None => {
                let (t, pv, ob, fi) = read_fut.await?;
                (false, t, pv, ob, fi)
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
            findings,
        );

        let term_ev = Event::new(EventKind::Event {
            payload: serde_json::json!({
                "agent_event": "terminated",
                "verdict": format!("{:?}", verdict.kind).to_lowercase(),
                "exit_code": status.code(),
            }),
        });
        redis_io::append_trace(conn, &brief.id, agent_id, &term_ev).await?;

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
///
/// `accumulated_findings` are `EventKind::Finding` payloads emitted by the
/// agent during its run. They are only attached to the output when
/// `terminal_event == Some(ReworkNeeded)`; otherwise they're dropped (the
/// agent declared a different terminal outcome and the findings are
/// informational trace data only).
fn compute_verdict(
    brief_id: &BriefId,
    timed_out: bool,
    permit_violation: Option<&str>,
    terminal_event: Option<orchestrator_types::EventVerdict>,
    exit_code: Option<i32>,
    accumulated_findings: Vec<orchestrator_types::ReviewFinding>,
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
            Some(orchestrator_types::EventVerdict::ReworkNeeded) => (
                VerdictKind::ReworkNeeded {
                    findings: accumulated_findings,
                },
                None,
            ),
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

fn bootstrap_command(
    pm: PackageManager,
    extra_binaries: &[String],
    extra_bootstrap: &[String],
    has_exitpoint: bool,
) -> String {
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
    let mut script = String::from("set -e\n");
    script.push_str(&install);
    script.push('\n');
    for cmd in extra_bootstrap {
        script.push_str(cmd);
        script.push('\n');
    }
    // $AGENTRY_SCRIPT is passed as an env var by the spawner. `bash -c` runs
    // it as a script; the script's own `cat` still reads the startup JSON
    // bundle from stdin (not affected by the outer bootstrap).
    if has_exitpoint {
        script.push_str("bash -c \"$AGENTRY_SCRIPT\"; _rc=$?\n");
        script.push_str("if [ \"$_rc\" -eq 0 ] && [ -n \"${AGENTRY_EXITPOINT:-}\" ]; then\n");
        script.push_str("    exec bash -c \"$AGENTRY_EXITPOINT\"\n");
        script.push_str("fi\n");
        script.push_str("exit \"$_rc\"");
    } else {
        script.push_str("exec bash -c \"$AGENTRY_SCRIPT\"");
    }
    script
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
            Vec::new(),
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
            Vec::new(),
        );
        assert!(matches!(v.kind, VerdictKind::PermitViolation));
        assert_eq!(v.reason.as_deref(), Some("unauthorized tool call: write"));
    }

    #[test]
    fn verdict_from_terminal_event() {
        let v = compute_verdict(
            &bid(),
            false,
            None,
            Some(EventVerdict::Shipped),
            Some(0),
            Vec::new(),
        );
        assert!(matches!(v.kind, VerdictKind::Shipped));
        assert!(v.reason.is_none());
    }

    #[test]
    fn verdict_rework_needed_attaches_accumulated_findings() {
        use orchestrator_types::{FindingOrigin, ReviewFinding, Severity};
        let findings = vec![ReviewFinding {
            file: None,
            line: None,
            severity: Severity::Blocker,
            origin: FindingOrigin::Mechanical {
                tool: "clippy".into(),
                rule: None,
            },
            category: "acceptance".into(),
            message: "stderr blob".into(),
            suggested_fix: None,
            prohibitions: Vec::new(),
            requirements: Vec::new(),
        }];
        let v = compute_verdict(
            &bid(),
            false,
            None,
            Some(EventVerdict::ReworkNeeded),
            Some(0),
            findings.clone(),
        );
        match v.kind {
            VerdictKind::ReworkNeeded { findings: got } => assert_eq!(got, findings),
            other => panic!("expected ReworkNeeded, got {other:?}"),
        }
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
            exitpoint_script: None,
            binaries: binaries.into_iter().map(String::from).collect(),
            mcp_servers: vec![],
            tool_allowlist: orchestrator_types::ToolAllowlist::default(),
            permit_scope: orchestrator_types::PermitScope::default(),
            passthru_env: vec![],
            mounts: vec![],
            workspace_mount: None,
            sccache,
            extra_bootstrap: vec![],
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
        let v = compute_verdict(&bid(), false, None, None, Some(1), Vec::new());
        assert!(matches!(v.kind, VerdictKind::Failed));
        let reason = v.reason.expect("reason required");
        assert!(reason.contains("agent exited without done event"));
        assert!(reason.contains("1"), "exit code surfaced: {reason}");
    }

    #[test]
    fn bootstrap_apk_installs_baseline_plus_extras() {
        let s = bootstrap_command(
            PackageManager::Apk,
            &["git".into(), "curl".into()],
            &[],
            false,
        );
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
        let s = bootstrap_command(PackageManager::Apt, &[], &[], false);
        assert!(s.contains("apt-get update"));
        assert!(s.contains("apt-get install -y --no-install-recommends"));
        assert!(s.contains("exec bash -c \"$AGENTRY_SCRIPT\""));
    }

    #[test]
    fn bootstrap_includes_extra_commands() {
        let s = bootstrap_command(
            PackageManager::Apt,
            &[],
            &[
                "rustup component add rustfmt".into(),
                "rustup component add clippy".into(),
            ],
            false,
        );
        assert!(s.contains("rustup component add rustfmt"));
        assert!(s.contains("rustup component add clippy"));
        let install_idx = s.find("apt-get install").expect("install line present");
        let rustfmt_idx = s
            .find("rustup component add rustfmt")
            .expect("rustfmt line present");
        let clippy_idx = s
            .find("rustup component add clippy")
            .expect("clippy line present");
        let exec_idx = s
            .find("exec bash -c \"$AGENTRY_SCRIPT\"")
            .expect("exec line present");
        assert!(install_idx < rustfmt_idx);
        assert!(rustfmt_idx < clippy_idx);
        assert!(clippy_idx < exec_idx);
    }

    #[test]
    fn bootstrap_runs_exitpoint_when_flag_set() {
        let s = bootstrap_command(PackageManager::Apt, &[], &[], true);
        assert!(s.contains("AGENTRY_EXITPOINT"));
        assert!(s.contains("exec bash -c \"$AGENTRY_EXITPOINT\""));
        assert!(!s.contains("exec bash -c \"$AGENTRY_SCRIPT\""));
    }
}
