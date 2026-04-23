//! Spawner — abstract container lifecycle; Podman adapter for M0.
//!
//! The Spawner:
//!   1. Accepts a Brief + AgentRole + WorkPermit.
//!   2. Spawns a container on the appropriate substrate.
//!   3. Injects the startup JSON (brief + permit + role) on the container's stdin.
//!   4. Tails stdout as NDJSON `Event`s, mirroring each to the brief's trace stream.
//!   5. On `Done`, appends a Verdict and tears down the container.
//!
//! For M0: only Podman is implemented. Other substrates come later.

use crate::{Error, Result, redis_io};
use async_trait::async_trait;
use orchestrator_types::{
    AgentRole, Brief, BriefId, Event, EventKind, Verdict, VerdictKind, WorkPermit,
};
use redis::aio::ConnectionManager;
use serde::Serialize;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// Startup bundle passed to the agent via stdin (one JSON document).
#[derive(Serialize)]
pub struct AgentStartup<'a> {
    pub brief: &'a Brief,
    pub role: &'a AgentRole,
    pub permit: &'a WorkPermit,
}

/// Handle returned by the spawner; used for teardown.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    pub agent_id: String,
    pub container_name: String,
}

/// The spawner abstraction.
#[async_trait]
pub trait Spawner: Send + Sync {
    /// Run the agent fully: spawn, pipe stdin, tail stdout to trace, write verdict, tear down.
    async fn run_agent(
        &self,
        brief: &Brief,
        role: &AgentRole,
        permit: &WorkPermit,
        conn: &mut ConnectionManager,
    ) -> Result<(AgentHandle, Verdict)>;
}

/// Podman spawner (M0).
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
        brief: &Brief,
        role: &AgentRole,
        permit: &WorkPermit,
        conn: &mut ConnectionManager,
    ) -> Result<(AgentHandle, Verdict)> {
        let agent_id = &permit.agent_id;
        let name = Self::container_name(agent_id);

        let startup = AgentStartup {
            brief,
            role,
            permit,
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
            .arg("--pull=never")
            .arg("--name")
            .arg(&name)
            .arg("--label")
            .arg(format!("agentry.brief={}", brief.id))
            .arg("--label")
            .arg(format!("agentry.role={}", role.name))
            .arg("--label")
            .arg(format!("agentry.agent={agent_id}"))
            .arg(&role.image);

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

        let mut terminal: Option<Event> = None;
        while let Some(line) = reader.next_line().await? {
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Event>(&line) {
                Ok(ev) => {
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

        // Capture stderr (diagnostic only — not mirrored to trace for M0).
        if let Some(mut stderr) = child.stderr.take() {
            let mut buf = Vec::new();
            use tokio::io::AsyncReadExt;
            stderr.read_to_end(&mut buf).await.ok();
            if !buf.is_empty() {
                tracing::debug!(stderr = %String::from_utf8_lossy(&buf), "agent stderr");
            }
        }

        let status = child.wait().await?;

        let verdict_kind = match terminal.as_ref().and_then(Event::verdict) {
            Some(v) => VerdictKind::from(v),
            None => {
                tracing::warn!(brief=%brief.id, exit=?status.code(), "no done event");
                VerdictKind::Failed
            }
        };

        let reason = match &verdict_kind {
            VerdictKind::Failed if terminal.is_none() => {
                Some(format!("agent exited without done event (code={:?})", status.code()))
            }
            _ => None,
        };

        let verdict = Verdict::new(brief.id.clone(), verdict_kind);
        let verdict = if let Some(r) = reason { verdict.with_reason(r) } else { verdict };

        Ok((
            AgentHandle {
                agent_id: agent_id.clone(),
                container_name: name,
            },
            verdict,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_name_format() {
        let n = PodmanSpawner::container_name("agt_abcd");
        assert_eq!(n, "agentry-agt_abcd");
    }
}

// Silence unused imports in M0 (full use comes in later milestones).
#[allow(dead_code)]
fn _used(_: EventKind, _: BriefId) {}
