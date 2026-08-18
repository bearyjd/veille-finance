//! ntfy-compatible push transport: POST the message body to the topic URL.

use std::time::Duration;

use async_trait::async_trait;

use super::PushSender;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub struct NtfyPush {
    client: reqwest::Client,
    url: String,
    token: Option<reqwest::header::HeaderValue>,
}

impl NtfyPush {
    pub fn new(url: String, token: Option<String>) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("push client init: {e}"))?;
        let token = token
            .map(|t| {
                let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}"))
                    .map_err(|_| "push token contains invalid header bytes".to_string())?;
                value.set_sensitive(true);
                Ok::<_, String>(value)
            })
            .transpose()?;
        Ok(Self { client, url, token })
    }
}

#[async_trait]
impl PushSender for NtfyPush {
    async fn send(&self, title: &str, body: &str) -> Result<(), String> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Title", title)
            .header("Priority", "high")
            .body(body.to_string());
        if let Some(token) = &self.token {
            request = request.header(reqwest::header::AUTHORIZATION, token.clone());
        }
        // without_url: ntfy topic paths are capability secrets and must not
        // reach logs through transport error text.
        let response = request
            .send()
            .await
            .map_err(|e| format!("push request: {}", e.without_url()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("push endpoint returned HTTP {status}"));
        }
        Ok(())
    }

    fn destination(&self) -> String {
        self.url.clone()
    }
}
