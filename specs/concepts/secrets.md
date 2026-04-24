# Secrets

A hygiene ledger — not a bounded context — enumerating every secret the
orchestrator reads, where it lives, who touches it, and the single known
limitation each one carries today. POC-scale; no port, no provider, no
runtime abstraction is implied by this file.

| Secret | Where it lives | Reader | Rotation policy | Known limitation |
| --- | --- | --- | --- | --- |
| Claude OAuth | `~/.claude/.credentials.json` on the host | container (via bind-mount) | none — manual | host-coupled via bind-mount; container is tied to the host UID that owns the credential file |
| GITEA_TOKEN | env var on orchestratord + forwarded into container as `passthru_env` | both | none — manual | same string reused across every forge operation; no per-brief ephemeral token |
| Redis password | embedded in `config.redis.url` (read from `~/.config/agentry/agentry.toml` or `AGENTRY_REDIS__URL`) | daemon | none — manual | travels in the URL string, so it is visible to any reader of the config file or the `AGENTRY_REDIS__URL` env var |
| signing.key | `~/.config/agentry/signing.key` (hex-encoded ed25519 private key, 0600) | daemon | none — manual | loss of this file invalidates every outstanding permit — no key-rotation ceremony |
| webhook secret | `config.webhook.secret` (from TOML / env), or persisted at `~/.config/agentry/webhook.secret` when unset | daemon | none — manual | shared secret in a header; no per-request signing, no replay protection |

#### Trust boundaries

Secrets cross three surfaces, and each surface enforces a different
guarantee. The **host filesystem** relies on POSIX mode bits plus the
ambient assumption that `$HOME` is owned by the orchestrator's UID; any
file under `~/.config/agentry/` must be 0600, and `load_signing_key`
refuses to proceed if `signing.key` is any other mode.

The **process env** (`/proc/<pid>/environ`) is readable to every process
running under the same UID on the host. `GITEA_TOKEN` and any LLM API key
named in a role's `passthru_env` live here; this is acceptable only
because agentry is single-user on a dedicated machine. A shared host
would need a different story.

The **podman bind-mount** surface is where host-side secrets cross into
the container. Every mount is explicit in the `AgentRole.mounts` list and
defaults to read-only. The rule established by PR #21 is that a file
crossing this surface must be repo-owned under `containers/**/`, not a
host user-config file — otherwise a silent edit on the host can change
container behaviour without any audit trail (see *Forbidden patterns*).

#### Intentional non-goals

- No Vault, KeePass-as-a-service, Infisical, or any external secret
  manager — POC scale; the cost/value ratio does not justify an extra
  daemon in the critical path.
- No rotation mechanism — rotations happen manually and infrequently;
  automation is deferred until a concrete incident demands it.
- No multi-user story — agentry assumes a single operator UID on a
  dedicated machine; per-user key scoping is out of scope.
- No audit log of secret reads — the trace stream records tool calls,
  not syscalls; building a secret-access audit trail would duplicate OS
  logging for no POC-visible benefit.
- No per-brief ephemeral credentials — tokens are long-lived; minting a
  short-lived credential per brief would require a token-issuing
  partner, which is explicitly out of scope.

#### Forbidden patterns

- Never bind-mount a host user-config file (e.g. `~/.claude/settings.json`)
  into a container. Container-bound config must be repo-owned under
  `containers/**/` and materialized at seed time (see PR #21 for the
  incident that established this rule).
- Never log the plaintext of any secret in this ledger. Tracing emits
  role name, brief id, and tool name — never the `passthru_env` values,
  never the contents of `signing.key`, never the webhook header.
- Never commit a secret into the repo, even a test-fixture one. Tests
  that need a signing key generate a fresh ephemeral one in a
  `tempfile::tempdir()` and discard it.
- Never widen `signing.key` mode past 0600 as a debugging shortcut —
  the load-time mode check exists to make that mistake loud.
