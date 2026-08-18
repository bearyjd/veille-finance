//! SMTP digest transport via lettre. Credentials come from the environment
//! variables named in config; the message is one multipart (text + HTML)
//! email addressed to every recipient at once.

use async_trait::async_trait;
use lettre::message::{MultiPart, SinglePart, header};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::EmailSender;
use crate::config::SmtpSection;

pub struct LettreSmtp {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: String,
}

impl LettreSmtp {
    /// Build from config + environment. Returns an error string suitable for
    /// per-tenant failure collection; never panics on bad config.
    pub fn new(section: &SmtpSection) -> Result<Self, String> {
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&section.host)
            .map_err(|e| format!("smtp relay setup: {e}"))?
            .port(section.port);

        let username = section
            .username_env
            .as_deref()
            .map(resolve_env)
            .transpose()?;
        let password = section
            .password_env
            .as_deref()
            .map(resolve_env)
            .transpose()?;
        match (username, password) {
            (Some(user), Some(pass)) => {
                builder = builder.credentials(Credentials::new(user, pass));
            }
            (None, None) => {}
            _ => {
                return Err("smtp username_env and password_env must be set together".into());
            }
        }

        Ok(Self {
            transport: builder.build(),
            from: section.from.clone(),
        })
    }
}

fn resolve_env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("environment variable {name} is not set"))
}

#[async_trait]
impl EmailSender for LettreSmtp {
    async fn send(
        &self,
        to: &[String],
        subject: &str,
        text: &str,
        html: &str,
    ) -> Result<(), String> {
        let mut builder = Message::builder()
            .from(
                self.from
                    .parse()
                    .map_err(|e| format!("from address: {e}"))?,
            )
            .subject(subject);
        for address in to {
            builder = builder.to(address
                .parse()
                .map_err(|e| format!("recipient {address}: {e}"))?);
        }
        let message = builder
            .multipart(
                MultiPart::alternative()
                    .singlepart(
                        SinglePart::builder()
                            .header(header::ContentType::TEXT_PLAIN)
                            .body(text.to_string()),
                    )
                    .singlepart(
                        SinglePart::builder()
                            .header(header::ContentType::TEXT_HTML)
                            .body(html.to_string()),
                    ),
            )
            .map_err(|e| format!("message build: {e}"))?;

        self.transport
            .send(message)
            .await
            .map_err(|e| format!("smtp send: {e}"))?;
        Ok(())
    }
}
