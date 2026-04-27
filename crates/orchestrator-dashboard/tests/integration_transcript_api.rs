//! HTTP-level coverage for `/briefs/{id}/transcript*` and `/workspace/path`.
//!
//! Each test builds the brief router with a temp `transcripts/` dir, drives
//! it through `tower::ServiceExt::oneshot`, and asserts on the response.
//! No live podman, no live Redis — the runtime is exercised purely as a
//! library.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use orchestrator_dashboard::routes::briefs::{router, BriefsState};
use std::path::Path;
use tower::ServiceExt;

const FIXTURE_TRANSCRIPT: &str = r#"{"type":"system","subtype":"init","session_id":"abc","model":"claude-sonnet-4-7"}
{"type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"text","text":"hello"},{"type":"tool_use","id":"tu_1","name":"Read","input":{"path":"/etc/hosts"}}],"usage":{"input_tokens":10,"output_tokens":5}}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"127.0.0.1 localhost"}]}}
{"type":"result","subtype":"success","result":"hello","is_error":false,"total_cost_usd":0.001,"duration_ms":420}
"#;

const NO_TOOLS_TRANSCRIPT: &str = r#"{"type":"system","subtype":"init"}
{"type":"assistant","message":{"id":"m","role":"assistant","content":[{"type":"text","text":"plain reply"}]}}
{"type":"result","subtype":"success","result":"plain reply","is_error":false}
"#;

async fn body_string(body: Body) -> String {
    let bytes = to_bytes(body, 1024 * 1024).await.expect("body bytes");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

fn req(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("build request")
}

fn req_post(uri: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .body(Body::empty())
        .expect("build POST request")
}

fn setup_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("transcripts tmpdir")
}

fn write_transcript(dir: &Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).expect("write transcript fixture");
}

#[tokio::test]
async fn get_transcript_200_when_present() {
    let tmp = setup_dir();
    let brief = "brf_present";
    write_transcript(tmp.path(), &format!("{brief}.jsonl"), FIXTURE_TRANSCRIPT);
    let app = router(BriefsState::new(tmp.path().to_path_buf()));

    let res = app
        .oneshot(req(&format!("/briefs/{brief}/transcript")))
        .await
        .expect("router call");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res.into_body()).await;
    assert!(body.contains("\"type\":\"system\""));
}

#[tokio::test]
async fn get_transcript_404_when_absent() {
    let tmp = setup_dir();
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req("/briefs/brf_missing/transcript"))
        .await
        .expect("router call");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tail_respects_n_param_and_caps_at_1000() {
    let tmp = setup_dir();
    let brief = "brf_tail";
    let mut s = String::new();
    for i in 0..1500 {
        s.push_str(&format!("{{\"type\":\"event\",\"i\":{i}}}\n"));
    }
    write_transcript(tmp.path(), &format!("{brief}.jsonl"), &s);
    let app = router(BriefsState::new(tmp.path().to_path_buf()));

    let res = app
        .clone()
        .oneshot(req(&format!("/briefs/{brief}/transcript/tail?n=5")))
        .await
        .expect("router call");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res.into_body()).await;
    assert_eq!(body.lines().count(), 5);

    let res = app
        .oneshot(req(&format!("/briefs/{brief}/transcript/tail?n=5000")))
        .await
        .expect("router call");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res.into_body()).await;
    assert_eq!(body.lines().count(), 1000, "tail must cap at 1000");
}

#[tokio::test]
async fn last_tool_call_404_when_no_tool_use() {
    let tmp = setup_dir();
    let brief = "brf_notools";
    write_transcript(tmp.path(), &format!("{brief}.jsonl"), NO_TOOLS_TRANSCRIPT);
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req(&format!("/briefs/{brief}/transcript/last-tool-call")))
        .await
        .expect("router call");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn last_tool_call_returns_call_when_present() {
    let tmp = setup_dir();
    let brief = "brf_withtool";
    write_transcript(tmp.path(), &format!("{brief}.jsonl"), FIXTURE_TRANSCRIPT);
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req(&format!("/briefs/{brief}/transcript/last-tool-call")))
        .await
        .expect("router call");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res.into_body()).await;
    let v: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(v.get("tool").and_then(|t| t.as_str()), Some("Read"));
    assert_eq!(v.get("completed").and_then(|c| c.as_bool()), Some(true));
}

#[tokio::test]
async fn summary_fields_populated_for_complete_fixture() {
    let tmp = setup_dir();
    let brief = "brf_sum";
    write_transcript(tmp.path(), &format!("{brief}.jsonl"), FIXTURE_TRANSCRIPT);
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req(&format!("/briefs/{brief}/transcript/summary")))
        .await
        .expect("router call");
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_string(res.into_body()).await;
    let v: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(v.get("event_count").and_then(|x| x.as_u64()), Some(4));
    assert_eq!(v.get("total_tokens_in").and_then(|x| x.as_u64()), Some(10));
    assert_eq!(v.get("total_tokens_out").and_then(|x| x.as_u64()), Some(5));
    let hist = v
        .get("tool_histogram")
        .and_then(|h| h.as_object())
        .expect("histogram object");
    assert_eq!(hist.get("Read").and_then(|x| x.as_u64()), Some(1));
}

#[tokio::test]
async fn workspace_path_404_when_brief_not_running() {
    let tmp = setup_dir();
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req("/briefs/brf_no_running/workspace/path"))
        .await
        .expect("router call");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn kill_404_when_brief_not_running() {
    let tmp = setup_dir();
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req_post("/briefs/brf_no_running/kill"))
        .await
        .expect("router call");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

// ---- traversal-defence tests --------------------------------------------

#[tokio::test]
async fn traversal_via_brief_id_path_segment_is_400() {
    let tmp = setup_dir();
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req("/briefs/..%2F..%2Fetc%2Fshadow/transcript"))
        .await
        .expect("router call");
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "encoded traversal in brief id must be rejected by charset gate"
    );
}

#[tokio::test]
async fn traversal_via_role_query_param_is_400() {
    let tmp = setup_dir();
    let brief = "brf_role";
    write_transcript(
        tmp.path(),
        &format!("{brief}.coder.jsonl"),
        FIXTURE_TRANSCRIPT,
    );
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req(&format!("/briefs/{brief}/transcript?role=..%2Fevil")))
        .await
        .expect("router call");
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "encoded traversal in role param must be rejected"
    );
}

#[tokio::test]
async fn symlink_in_transcripts_dir_pointing_outside_is_rejected() {
    let tmp = setup_dir();
    let brief = "brf_symlink";
    let link = tmp.path().join(format!("{brief}.coder.jsonl"));
    let outside = Path::new("/etc/passwd");
    if !outside.exists() {
        eprintln!("[skip] /etc/passwd absent on this runner");
        return;
    }
    std::os::unix::fs::symlink(outside, &link).expect("symlink");
    let app = router(BriefsState::new(tmp.path().to_path_buf()));
    let res = app
        .oneshot(req(&format!("/briefs/{brief}/transcript?role=coder")))
        .await
        .expect("router call");
    assert_eq!(
        res.status(),
        StatusCode::BAD_REQUEST,
        "symlink escape must be caught by canonicalize-prefix check"
    );
}
