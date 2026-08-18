//! OpenAI-compatible `/v1/chat/completions` client for narration.

use std::time::Duration;

use serde_json::json;

use crate::digest::DigestInput;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_COMPLETION_TOKENS: u32 = 600;

const SYSTEM_PROMPT: &str = "You write the narration section of a family's \
financial watch digest. You receive the deterministic findings and account \
balance changes for one period. Summarize them in a few short, plain, calm \
sentences a person reads on a phone. Repeat amounts and names exactly as \
given. Never invent, omit, downplay, or re-rank a finding — the full list is \
printed below your text regardless. If there are no findings, say the period \
looks quiet in one sentence.";

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
        Some(Self {
            base_url,
            api_key,
            model,
        })
    }
}

/// Ask the model for a short prose summary of this digest. Returns `None` on
/// any failure — timeout, transport error, non-2xx, malformed response —
/// after logging a warning. The digest must never be blocked by this.
pub async fn narrate(config: &LlmConfig, input: &DigestInput) -> Option<String> {
    match try_narrate(config, input).await {
        Ok(prose) => Some(prose),
        Err(reason) => {
            tracing::warn!(%reason, "narration unavailable; digest ships with plain rendering");
            None
        }
    }
}

async fn try_narrate(config: &LlmConfig, input: &DigestInput) -> Result<String, String> {
    let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("client init: {e}"))?;

    let findings: Vec<serde_json::Value> = input
        .findings
        .iter()
        .map(|f| {
            json!({
                "rule": f.rule_id,
                "severity": f.severity.as_str(),
                "subject": f.subject,
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
            "max_tokens": MAX_COMPLETION_TOKENS,
            "temperature": 0.3,
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
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("response parse: {e}"))?;
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .ok_or("response missing choices[0].message.content")?
        .trim();
    if content.is_empty() {
        return Err("empty narration".into());
    }
    Ok(content.to_string())
}
