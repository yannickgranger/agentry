//! orchestrator-dashboard library — exposes the request-routing layer so
//! integration tests can spin the brief routes against a temp transcripts
//! directory without booting the full webhook + Redis-backed dashboard.

#![forbid(unsafe_code)]

pub mod metrics;
pub mod routes;
pub mod store;

use axum::extract::FromRef;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use store::DashboardStore;

/// Shared application state for the dashboard router. Defined here (rather
/// than in the binary crate) so route modules in this lib can name it.
#[derive(Clone)]
pub struct AppState {
    pub store: DashboardStore,
    pub webhook_secret: Option<String>,
}

impl FromRef<AppState> for DashboardStore {
    fn from_ref(state: &AppState) -> Self {
        state.store.clone()
    }
}

/// Error wrapper so handlers can use `?`. Defined here so route modules
/// can name it (the binary crate cannot expose its types to the lib).
pub struct AppError(pub anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        Self(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self.0, "handler error");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("error: {}", self.0),
        )
            .into_response()
    }
}

/// Resolve the webhook shared secret used to guard `POST /submit`.
///
/// Explicit config wins: if `cfg_value` is `Some`, it is returned unchanged.
/// Otherwise look for a persisted secret at `~/.config/agentry/webhook.secret`;
/// read + trim if present, else mint 16 random bytes hex-encoded (32 ASCII
/// chars), persist with mode 0o600, and return the new value. This keeps
/// the webhook token stable across daemon restarts when no TOML/env secret
/// is configured — the first start mints one, every later start reads it
/// back.
pub fn resolve_webhook_secret(cfg_value: Option<String>) -> std::io::Result<Option<String>> {
    if cfg_value.is_some() {
        return Ok(cfg_value);
    }
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(None);
    };
    let mut path = std::path::PathBuf::from(home);
    path.push(".config");
    path.push("agentry");
    path.push("webhook.secret");

    if path.exists() {
        let contents = std::fs::read_to_string(&path)?;
        return Ok(Some(contents.trim().to_string()));
    }

    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| std::io::Error::other(format!("getrandom: {e}")))?;
    let value: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &value)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }

    Ok(Some(value))
}
