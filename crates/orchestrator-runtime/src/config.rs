//! Typed configuration via figment.
//!
//! Load order (later wins):
//! 1. Code defaults (`Config::default()`).
//! 2. `~/.config/agentry/agentry.toml` (optional, 0600 recommended).
//! 3. Env vars prefixed `AGENTRY_`, nested by `__`
//!    (e.g. `AGENTRY_REDIS__URL`, `AGENTRY_DASHBOARD__PORT`,
//!    `AGENTRY_SIGNING__KEY_PATH`).
//!
//! **NOT managed here:** per-role LLM API keys (`XAI_API_KEY`, `GEMINI_API_KEY`).
//! Those are per-role `passthru_env` — the central config has no business
//! knowing about them.

use crate::{Error, Result};
use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_max_concurrent_briefs() -> u32 {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub redis: RedisConfig,
    pub dashboard: DashboardConfig,
    pub signing: SigningConfig,
    #[serde(default)]
    pub webhook: WebhookConfig,
    #[serde(default)]
    pub forge: ForgeConfig,
    #[serde(default)]
    pub sccache: SccacheConfig,
    #[serde(default = "default_max_concurrent_briefs")]
    pub max_concurrent_briefs: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisConfig {
    /// Full Redis URL (may embed password).
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SigningConfig {
    /// Path to the ed25519 signing key (hex-encoded 32 bytes).
    pub key_path: PathBuf,
}

/// Dashboard webhook trigger config. If `secret` is `None`, `POST /submit`
/// returns 401 — webhook submission is disabled.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Shared secret required in the `X-Agentry-Token` header.
    #[serde(default)]
    pub secret: Option<String>,
}

/// Forge defaults applied when a brief's payload does not override them.
/// `default_host` is the `host:port` (no scheme) used to construct the
/// token-bearing clone URL. Unset means every brief must carry its own
/// `forge_host` in the payload.
///
/// `allowed_owners` lists bare forge owner names (e.g. `"yg"`); seed.rs
/// expands each to a `forge:write:<owner>/*` permit on roles that push
/// branches or open PRs. Empty list rejects all writes — required for
/// brief dispatch on the agentry-self-host topology.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForgeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_host: Option<String>,
    #[serde(default)]
    pub allowed_owners: Vec<String>,
    /// Disable TLS certificate validation on the forge HTTP client. Defaults
    /// to false (production-safe). Set true only for lab-internal forges with
    /// self-signed certs; mirrors the substrate's existing `curl -k` pattern
    /// used by shipper-agentry / ci-watcher-agentry.
    #[serde(default)]
    pub tls_insecure: bool,
}

/// Shared sccache backend used by roles that compile Rust. `endpoint`
/// is the network alias or DNS name (with optional `:port`) of the
/// sccache-redis container; seed.rs strips any port and expands it to
/// a `net:allow:<host>` permit. Unset means roles run without sccache.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SccacheConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        Self {
            redis: RedisConfig {
                // Default targets the LOCAL podman dev Redis on 6380.
                // NEVER hard-code a prod Redis here.
                url: "redis://127.0.0.1:6380".into(),
            },
            dashboard: DashboardConfig { port: 7800 },
            signing: SigningConfig {
                key_path: PathBuf::from(format!("{home}/.config/agentry/signing.key")),
            },
            webhook: WebhookConfig::default(),
            forge: ForgeConfig::default(),
            sccache: SccacheConfig::default(),
            max_concurrent_briefs: 4,
        }
    }
}

impl Config {
    /// Load config: defaults → `~/.config/agentry/agentry.toml` → env overlay.
    pub fn load() -> Result<Self> {
        let home = std::env::var("HOME").unwrap_or_default();
        let toml_path = PathBuf::from(format!("{home}/.config/agentry/agentry.toml"));

        let mut fig = Figment::from(Serialized::defaults(Config::default()));
        if toml_path.exists() {
            fig = fig.merge(Toml::file(&toml_path));
        }
        fig = fig.merge(Env::prefixed("AGENTRY_").split("__"));

        fig.extract()
            .map_err(|e| Error::Config(format!("figment extract: {e}")))
    }
}
