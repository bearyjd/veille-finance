//! OpenAI-compatible `/v1/chat/completions` client for narration.

use std::time::Duration;

use serde_json::json;

use crate::digest::DigestInput;

/// Total per-request budget. The digest is interactive output; a stalled
/// endpoint must not hold it hostage.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TOKENS: u32 = 600;
/// A narration response larger than this is a broken or hostile endpoint.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Accepted narration length cap (characters) — a summary, not an essay.
const MAX_NARRATION_CHARS: usize = 4_000;

const SYSTEM_PROMPT: &str = "You write the narration section of a family's \
financial watch digest. You receive the deterministic findings and account \
balance changes for one period. Summarize them in a few short, plain, calm \
sentences a person reads on a phone. Repeat amounts and names exactly as \
given. Never invent, omit, downplay, or re-rank a finding — the digest \
prints the full findings list above your text and labels your text as an \
automated summary. If there are no findings, say the period looks quiet in \
one sentence.";

/// Resolved narration configuration. `None` anywhere → narration is off.
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl LlmConfig {
    /// Base URL and key come from `VEILLE_LLM_BASE_URL` / `VEILLE_LLM_API_KEY`;
    /// the model from config, overridable via `VEILLE_LLM_MODEL`. Any missing
    /// piece disables narration.
    pub fn resolve(model_from_config: Option<&str>) -> Option<Self> {
        Self::resolve_from(|key| std::env::var(key).ok(), model_from_config)
    }

    /// Same as [`Self::resolve`], with the environment injected for tests.
    pub fn resolve_from(
        lookup: impl Fn(&str) -> Option<String>,
        model_from_config: Option<&str>,
    ) -> Option<Self> {
        let base_url = lookup("VEILLE_LLM_BASE_URL")?;
        let api_key = lookup("VEILLE_LLM_API_KEY")?;
        let model = lookup("VEILLE_LLM_MODEL").or_else(|| model_from_config.map(String::from))?;
        if base_url.trim().is_empty() || api_key.trim().is_empty() || model.trim().is_empty() {
            return None;
        }
        // Userinfo in the URL would surface in transport-error logs
        // (reqwest's Display includes the URL). Refuse rather than leak.
        if let Ok(parsed) = reqwest::Url::parse(&base_url)
            && (!parsed.username().is_empty() || parsed.password().is_some())
        {
            tracing::warn!("VEILLE_LLM_BASE_URL must not embed credentials; narration off");
            return None;
        }
        Some(Self {
            base_url,
            api_key,
            model,
        })
    }

    /// One client for all narration calls in a run.
    pub fn client() -> Option<reqwest::Client> {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()
    }
}

/// Ask the model for a short prose summary of this digest. Returns `None` on
/// any failure — timeout, transport error, non-2xx, oversized or truncated
/// or malformed response — after logging a warning. The digest must never be
/// blocked by this.
pub async fn narrate(
    client: &reqwest::Client,
    config: &LlmConfig,
    input: &DigestInput,
) -> Option<String> {
    match try_narrate(client, config, input).await {
        Ok(prose) => Some(prose),
        Err(reason) => {
            tracing::warn!(%reason, "narration unavailable; digest ships with plain rendering");
            None
        }
    }
}

async fn try_narrate(
    client: &reqwest::Client,
    config: &LlmConfig,
    input: &DigestInput,
) -> Result<String, String> {
    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));

    let findings: Vec<serde_json::Value> = input
        .findings
        .iter()
        .map(|f| {
            json!({
                "rule": f.rule_id,
                "severity": f.severity.as_str(),
                "summary": f.summary,
                "evidence": f.evidence,
            })
        })
        .collect();
    let deltas: Vec<serde_json::Value> = input
        .deltas
        .iter()
        .map(|d| {
            json!({
                "account": d.account,
                "start_minor": d.start_minor,
                "end_minor": d.end_minor,
                "currency": d.currency,
            })
        })
        .collect();
    let user_message = json!({
        "period_start": input.period_start.to_string(),
        "period_end": input.period_end.to_string(),
        "findings": findings,
        "account_changes": deltas,
    })
    .to_string();

    let response = client
        .post(&url)
        .bearer_auth(&config.api_key)
        .json(&json!({
            "model": config.model,
            "max_tokens": MAX_TOKENS,
            "messages": [
                { "role": "system", "content": SYSTEM_PROMPT },
                { "role": "user", "content": user_message },
            ],
        }))
        .send()
        .await
        .map_err(|e| format!("request: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("llm endpoint returned HTTP {status}"));
    }

    // Bounded read: never buffer an unbounded body.
    let mut body_bytes: Vec<u8> = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("read: {e}"))? {
        body_bytes.extend_from_slice(&chunk);
        if body_bytes.len() > MAX_RESPONSE_BYTES {
            return Err(format!("response exceeded {MAX_RESPONSE_BYTES} bytes"));
        }
    }
    let body: serde_json::Value =
        serde_json::from_slice(&body_bytes).map_err(|e| format!("response parse: {e}"))?;

    if body["choices"][0]["finish_reason"] == "length" {
        return Err("narration was truncated by the token limit".into());
    }
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("response missing choices[0].message.content")?
        .trim();
    if content.is_empty() {
        return Err("empty narration".into());
    }
    if content.chars().count() > MAX_NARRATION_CHARS {
        return Err(format!(
            "narration exceeded {MAX_NARRATION_CHARS} characters"
        ));
    }
    Ok(content.to_string())
}
