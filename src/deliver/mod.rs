//! Delivery (PRP §9): push for Alert-severity findings, SMTP for the
//! periodic digest. §2.2 is structural here: the ONLY recipient accessor
//! returns owners and watchers together — there is no API that could address
//! a subset, and config validation refuses watcher-only tenants.

pub mod push;
pub mod smtp;

use async_trait::async_trait;

use crate::config::RecipientConfig;

/// Immediate notification channel (ntfy or compatible). One destination per
/// tenant — everyone entitled to alerts subscribes to it.
#[async_trait]
pub trait PushSender: Send + Sync {
    async fn send(&self, title: &str, body: &str) -> Result<(), String>;
    /// Where sends go, for the delivery audit trail.
    fn destination(&self) -> String;
}

/// Digest email transport.
#[async_trait]
pub trait EmailSender: Send + Sync {
    async fn send(
        &self,
        to: &[String],
        subject: &str,
        text: &str,
        html: &str,
    ) -> Result<(), String>;
}

/// The one and only recipient accessor: owners and watchers, together,
/// always. Deduplicated, order-stable.
pub fn all_recipient_emails(recipients: &[RecipientConfig]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    recipients
        .iter()
        .filter(|r| seen.insert(r.email.clone()))
        .map(|r| r.email.clone())
        .collect()
}
