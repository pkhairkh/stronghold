//! ntfy client — send push notifications to the phone.
//!
//! ntfy is a self-hosted pub/sub notification service. The phone app
//! (open source) talks to OUR ntfy server. No external provider for
//! content. APNs/FCM are wake-up triggers only (iOS), and even that's
//! optional (instant-delivery polling mode available).

use anyhow::Result;
use reqwest::Client;

/// Push an approval request to the tenant's phone.
pub async fn push_approval_request(
    tenant_id: &str,
    session_id: &str,
    req: &crate::routes::agent::OrderRequest,
) -> Result<()> {
    let topic = format!("{}-session-requested", tenant_id);
    let title = "Stronghold: Session Request";
    let message = format!(
        "Image: {}\nTTL: {}s\nReason: {}",
        req.image, req.ttl_secs, req.reason
    );

    let actions = format!(
        r#"view, Approve, https://gateway/setup?approve={}, " Clear=true; view, Deny, https://gateway/setup?deny={}"#,
        session_id, session_id
    );

    send_notification(&topic, title, &message, Some(&actions), 5).await
}

/// Push an extend request.
pub async fn push_extend_request(
    tenant_id: &str,
    session_id: &str,
    req: &crate::routes::agent::ExtendRequest,
) -> Result<()> {
    let topic = format!("{}-session-requested", tenant_id);
    let title = "Stronghold: Session Extension";
    let message = format!(
        "Machine: {}\nAdditional: {}s",
        req.machine_id, req.additional_secs
    );

    send_notification(&topic, title, &message, None, 4).await
}

/// Push an anomaly alert.
pub async fn push_anomaly(
    tenant_id: &str,
    machine_id: &str,
    message: &str,
) -> Result<()> {
    let topic = format!("{}-session-anomaly", tenant_id);
    let title = "Stronghold: Anomaly Detected";

    send_notification(&topic, title, message, None, 4).await
}

/// Push a session-revoked confirmation.
pub async fn push_revoked(
    tenant_id: &str,
    machine_id: &str,
) -> Result<()> {
    let topic = format!("{}-session-active", tenant_id);
    let title = "Stronghold: Session Revoked";
    let message = format!("Machine {} has been revoked", machine_id);

    send_notification(&topic, title, &message, None, 4).await
}

/// Send a notification to the local ntfy server.
async fn send_notification(
    topic: &str,
    title: &str,
    message: &str,
    actions: Option<&str>,
    priority: u8,
) -> Result<()> {
    let ntfy_url = std::env::var("STRONGHOLD_NTFY_URL")
        .unwrap_or_else(|_| "http://localhost:8090".to_string());

    let url = format!("{}/{}", ntfy_url, topic);
    let client = Client::new();

    let mut req = client
        .post(&url)
        .header("Title", title)
        .header("Priority", priority.to_string())
        .body(message.to_string());

    if let Some(actions) = actions {
        req = req.header("Actions", actions);
    }

    // TODO: add E2E encryption header (X25519 + ML-KEM-768 hybrid)
    // The message body will be encrypted before sending.

    let resp = req.send().await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("ntfy push failed: {}", resp.status()));
    }

    tracing::debug!(topic = topic, title = title, "Notification sent");
    Ok(())
}
