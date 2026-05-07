//! Integration tests for `redis_io::fetch_profile` (slice I/2b).
//!
//! Spins up `wiremock` to stand in for the forge contents API. The
//! lower-level `fetch_profile_url` entry point is used so the tests can
//! point at the mock's `http://` base; the public `fetch_profile` builds
//! `https://` URLs and is exercised separately for the input-parse path
//! (`profile_fetch_malformed_target_repo`).

use base64::Engine;
use orchestrator_runtime::redis_io::{fetch_profile, fetch_profile_url, ProfileFetchError};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROFILE_PATH: &str = "/api/v1/repos/owner/repo/contents/.agentry/profile.toml";

fn forge_contents_body(toml_text: &str) -> serde_json::Value {
    let encoded = base64::engine::general_purpose::STANDARD.encode(toml_text.as_bytes());
    serde_json::json!({
        "name": "profile.toml",
        "path": ".agentry/profile.toml",
        "type": "file",
        "encoding": "base64",
        "content": encoded,
    })
}

#[tokio::test]
async fn profile_fetch_404_returns_none() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .and(header("Authorization", "token tok"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let url = format!("{}{PROFILE_PATH}?ref=develop", server.uri());
    let got = fetch_profile_url(&url, "tok").await.expect("fetch ok");
    assert!(got.is_none(), "404 should return Ok(None), got {got:?}");
}

#[tokio::test]
async fn profile_fetch_valid_returns_profile() {
    let server = MockServer::start().await;
    let toml = r#"
[coder]
tool_packs = ["rust-base", "rust-lints"]

[reviewer]
tool_packs = ["reviewer-base"]

[acceptance]
default = "cargo test --workspace"

[methodology]
gates = ["discover", "prescribe"]
"#;
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(forge_contents_body(toml)))
        .mount(&server)
        .await;

    let url = format!("{}{PROFILE_PATH}?ref=develop", server.uri());
    let profile = fetch_profile_url(&url, "tok")
        .await
        .expect("fetch ok")
        .expect("profile present");
    assert_eq!(
        profile.coder.tool_packs,
        vec!["rust-base".to_string(), "rust-lints".to_string()]
    );
    assert_eq!(
        profile.reviewer.tool_packs,
        vec!["reviewer-base".to_string()]
    );
    assert_eq!(
        profile.acceptance.default.as_deref(),
        Some("cargo test --workspace")
    );
    assert_eq!(
        profile.methodology.gates,
        vec!["discover".to_string(), "prescribe".to_string()]
    );
}

#[tokio::test]
async fn profile_fetch_malformed_target_repo() {
    let err = fetch_profile("no-slash-here", "develop", "forge.example", "tok")
        .await
        .expect_err("malformed target_repo must error");
    match err {
        ProfileFetchError::MalformedTargetRepo(s) => {
            assert_eq!(s, "no-slash-here");
        }
        other => panic!("expected MalformedTargetRepo, got {other:?}"),
    }
}

#[tokio::test]
async fn profile_fetch_500_returns_http_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let url = format!("{}{PROFILE_PATH}?ref=develop", server.uri());
    let err = fetch_profile_url(&url, "tok")
        .await
        .expect_err("500 must surface as Http variant");
    match err {
        ProfileFetchError::Http { status, body } => {
            assert_eq!(status, 500);
            assert_eq!(body, "boom");
        }
        other => panic!("expected Http, got {other:?}"),
    }
}

#[tokio::test]
async fn profile_fetch_invalid_toml() {
    let server = MockServer::start().await;
    let bad_toml = "this is = = not valid toml [[[";
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(forge_contents_body(bad_toml)))
        .mount(&server)
        .await;

    let url = format!("{}{PROFILE_PATH}?ref=develop", server.uri());
    let err = fetch_profile_url(&url, "tok")
        .await
        .expect_err("invalid toml must surface as Parse variant");
    assert!(
        matches!(err, ProfileFetchError::Parse(_)),
        "expected Parse, got {err:?}"
    );
}
