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
