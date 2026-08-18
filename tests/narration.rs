#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Narration contract (PRP §8): well-formed OpenAI-compatible requests,
//! prose out — and `None` on every failure mode, because the digest must
//! never be blocked by an LLM.

use chrono::{TimeZone, Utc};
use serde_json::json;
use veille::digest::DigestInput;
use veille::narrate::{LlmConfig, narrate};
use veille::store::repo::StoredFinding;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn digest_input() -> DigestInput {
    DigestInput {
        tenant_display_name: "Alpha Household".into(),
        period_start: "2026-08-13".parse().expect("date"),
        period_end: "2026-08-20".parse().expect("date"),
        findings: vec![StoredFinding {
            rule_id: "sync-stale".into(),
            severity: veille::domain::Severity::Alert,
            subject: "institution:First National".into(),
            evidence: json!({ "days_stale": 5 }),
            detected_at: Utc
                .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
                .single()
                .expect("ts"),
            dedupe_key: "sync-stale:First National:x".into(),
        }],
        deltas: Vec::new(),
        narration: None,
    }
}

fn config_for(server: &MockServer) -> LlmConfig {
    LlmConfig {
        base_url: server.uri(),
        api_key: "test-llm-key".into(),
        model: "cheap-think".into(),
    }
}

#[tokio::test]
async fn request_is_well_formed_and_prose_comes_back() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer test-llm-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "role": "assistant", "content": "One alert this week." } }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let narration = narrate(&config_for(&server), &digest_input()).await;
    assert_eq!(narration.as_deref(), Some("One alert this week."));

    let requests = server.received_requests().await.expect("requests");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("json body");
    assert_eq!(body["model"], "cheap-think");
    assert!(body["messages"].is_array());
    let user_message = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|m| m["role"] == "user")
        .expect("user message")["content"]
        .as_str()
        .expect("content");
    assert!(
        user_message.contains("sync-stale") && user_message.contains("First National"),
        "the findings must be in the prompt: {user_message}"
    );
}

#[tokio::test]
async fn server_error_yields_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    assert_eq!(narrate(&config_for(&server), &digest_input()).await, None);
}

#[tokio::test]
async fn unreachable_endpoint_yields_none() {
    let config = LlmConfig {
        base_url: "http://127.0.0.1:1".into(),
        api_key: "k".into(),
        model: "m".into(),
    };
    assert_eq!(narrate(&config, &digest_input()).await, None);
}

#[tokio::test]
async fn malformed_response_yields_none() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server)
        .await;
    assert_eq!(narrate(&config_for(&server), &digest_input()).await, None);
}

#[test]
fn config_resolution_requires_every_piece() {
    let full = |key: &str| -> Option<String> {
        match key {
            "VEILLE_LLM_BASE_URL" => Some("https://llm.example/v1".into()),
            "VEILLE_LLM_API_KEY" => Some("k".into()),
            _ => None,
        }
    };
    let resolved = LlmConfig::resolve_from(full, Some("cheap-think")).expect("resolves");
    assert_eq!(resolved.model, "cheap-think");

    // Env model overrides config model.
    let with_model = |key: &str| -> Option<String> {
        match key {
            "VEILLE_LLM_BASE_URL" => Some("https://llm.example/v1".into()),
            "VEILLE_LLM_API_KEY" => Some("k".into()),
            "VEILLE_LLM_MODEL" => Some("writing".into()),
            _ => None,
        }
    };
    let resolved = LlmConfig::resolve_from(with_model, Some("cheap-think")).expect("resolves");
    assert_eq!(resolved.model, "writing");

    // Missing any piece disables narration entirely.
    assert!(
        LlmConfig::resolve_from(full, None).is_none(),
        "no model anywhere"
    );
    let no_key = |key: &str| -> Option<String> {
        (key == "VEILLE_LLM_BASE_URL").then(|| "https://llm.example/v1".into())
    };
    assert!(
        LlmConfig::resolve_from(no_key, Some("m")).is_none(),
        "no api key"
    );
    assert!(
        LlmConfig::resolve_from(|_| None, Some("m")).is_none(),
        "no base url"
    );
}
