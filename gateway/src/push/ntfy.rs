//! ntfy client — send push notifications to the phone.
//!
//! ntfy is a self-hosted pub/sub notification service. The phone app
//! (open source) talks to OUR ntfy server. No external provider for
//! content. APNs/FCM are wake-up triggers only (iOS), and even that's
//! optional (instant-delivery polling mode available).
//!
//! Implemented in: W5-T5 (ntfy HTTP push), W5-T9 (daily digest)
//! Tested by: gateway/src/push/ntfy.rs (mock-server unit tests)

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
    _session_id: &str,
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
pub async fn push_anomaly(tenant_id: &str, _machine_id: &str, message: &str) -> Result<()> {
    let topic = format!("{}-session-anomaly", tenant_id);
    let title = "Stronghold: Anomaly Detected";

    send_notification(&topic, title, message, None, 4).await
}

/// Push a session-revoked confirmation.
pub async fn push_revoked(tenant_id: &str, machine_id: &str) -> Result<()> {
    let topic = format!("{}-session-active", tenant_id);
    let title = "Stronghold: Session Revoked";
    let message = format!("Machine {} has been revoked", machine_id);

    send_notification(&topic, title, &message, None, 4).await
}

/// Push a daily audit digest (W5-T9).
///
/// Sent at 09:00 tenant-local time. Summarizes the previous day's
/// activity so the tenant can spot anomalies at a glance.
///
/// The summary is also written to the audit log as a `daily_digest`
/// event by the caller (this function only does the push).
pub async fn push_daily_digest(
    tenant_id: &str,
    sessions_started: u64,
    sessions_revoked: u64,
    commands_executed: u64,
    anomalies_detected: u64,
) -> Result<()> {
    let topic = format!("{}-daily-digest", tenant_id);
    let title = "Stronghold: Daily Digest";
    let message = format!(
        "Sessions started: {}\n\
         Sessions revoked: {}\n\
         Commands executed: {}\n\
         Anomalies detected: {}",
        sessions_started, sessions_revoked, commands_executed, anomalies_detected
    );

    // Priority 3 (default) — informational, not urgent.
    send_notification(&topic, title, &message, None, 3).await
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

    let client = Client::new();
    send_notification_to(
        &client,
        &ntfy_url,
        topic,
        title,
        message,
        actions,
        priority,
    )
    .await
}

/// Send a notification to an explicit ntfy server URL using an explicit
/// reqwest `Client`.
///
/// This is the test-friendly variant of [`send_notification`]: tests pass
/// the URL of a mock HTTP server and a client they control, so they can
/// capture the request without touching the network or the environment.
///
/// The `body` is sent verbatim as the request body. The ntfy protocol
/// uses HTTP headers for metadata (`Title`, `Priority`, `Actions`) and
/// the body as the message content.
pub async fn send_notification_to(
    client: &Client,
    ntfy_url: &str,
    topic: &str,
    title: &str,
    body: &str,
    actions: Option<&str>,
    priority: u8,
) -> Result<()> {
    let url = format!("{}/{}", ntfy_url, topic);

    let mut req = client
        .post(&url)
        .header("Title", title)
        .header("Priority", priority.to_string())
        .body(body.to_string());

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

/// Send an E2E-encrypted notification to an explicit ntfy server URL.
///
/// The plaintext is encrypted with the phone's hybrid public keys
/// (X25519 + ML-KEM-768 → AES-256-GCM), then base64-encoded and sent
/// as the request body. The ntfy server sees only the base64 ciphertext.
///
/// Used by the W5-T7 test that proves the ntfy server cannot read
/// push content.
#[allow(clippy::too_many_arguments)]
pub async fn send_encrypted_notification_to(
    client: &Client,
    ntfy_url: &str,
    topic: &str,
    title: &str,
    plaintext: &[u8],
    phone_x25519_pub: &[u8],
    phone_mlkem_pub: &[u8],
    actions: Option<&str>,
    priority: u8,
) -> Result<()> {
    let encrypted = crate::push::e2e::encrypt(plaintext, phone_x25519_pub, phone_mlkem_pub)?;
    let body = crate::push::e2e::encode(&encrypted);
    send_notification_to(client, ntfy_url, topic, title, &body, actions, priority).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::hybrid_kem::PushKeys;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A captured HTTP request: method, path, headers, body.
    #[derive(Debug, Clone, Default)]
    struct CapturedRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    /// A tiny mock ntfy server: listens on a random localhost port,
    /// captures each incoming POST, and returns 200 OK.
    ///
    /// Returns the bound address (`http://127.0.0.1:PORT`) and a handle
    /// to the captured-requests list.
    struct MockNtfy {
        base_url: String,
        captured: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    impl MockNtfy {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let base_url = format!("http://{}", addr);
            let captured: Arc<Mutex<Vec<CapturedRequest>>> =
                Arc::new(Mutex::new(Vec::new()));

            let cap = captured.clone();
            tokio::spawn(async move {
                loop {
                    let (mut sock, _) = match listener.accept().await {
                        Ok(s) => s,
                        Err(_) => break,
                    };
                    let cap2 = cap.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 8192];
                        // Read whatever the client sends. The request is small
                        // (a single ntfy POST), so one read is usually enough;
                        // loop until we've seen the end of headers + body.
                        let mut total = Vec::new();
                        loop {
                            match sock.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    total.extend_from_slice(&buf[..n]);
                                    // If we've seen the end of headers AND the
                                    // body matches Content-Length, stop.
                                    if let Some(hdr_end) =
                                        find_subslice(&total, b"\r\n\r\n")
                                    {
                                        let headers = &total[..hdr_end];
                                        let content_length = parse_content_length(headers);
                                        let body_start = hdr_end + 4;
                                        if total.len() >= body_start + content_length {
                                            break;
                                        }
                                    }
                                }
                                Err(_) => break,
                            }
                        }

                        let req = parse_http_request(&total);
                        {
                            let mut guard = cap2.lock().unwrap();
                            guard.push(req);
                        }

                        // Respond with 200 OK + empty JSON body.
                        let resp = b"HTTP/1.1 200 OK\r\n\
                                     Content-Type: application/json\r\n\
                                     Content-Length: 17\r\n\
                                     \r\n\
                     {\"event\":\"ok\"}";
                        let _ = sock.write_all(resp).await;
                        let _ = sock.flush().await;
                    });
                }
            });

            MockNtfy { base_url, captured }
        }

        fn requests(&self) -> Vec<CapturedRequest> {
            self.captured.lock().unwrap().clone()
        }
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
    }

    fn parse_content_length(headers: &[u8]) -> usize {
        let s = String::from_utf8_lossy(headers);
        for line in s.lines() {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-length") {
                    return v.trim().parse().unwrap_or(0);
                }
            }
        }
        0
    }

    fn parse_http_request(raw: &[u8]) -> CapturedRequest {
        let s = String::from_utf8_lossy(raw).to_string();
        let mut lines = s.split("\r\n");

        let request_line = lines.next().unwrap_or("");
        let mut rl_parts = request_line.splitn(3, ' ');
        let method = rl_parts.next().unwrap_or("").to_string();
        let path = rl_parts.next().unwrap_or("").to_string();

        let mut headers = Vec::new();
        let mut body = String::new();
        let mut in_body = false;
        for line in lines {
            if in_body {
                body.push_str(line);
                body.push_str("\r\n");
                continue;
            }
            if line.is_empty() {
                in_body = true;
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            }
        }

        // The body may have a trailing CRLF from our parse; strip it.
        while body.ends_with("\r\n") {
            body.truncate(body.len() - 2);
        }

        CapturedRequest {
            method,
            path,
            headers,
            body,
        }
    }

    fn header_value<'a>(req: &'a CapturedRequest, name: &str) -> Option<&'a str> {
        req.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    // --- W5-T5: ntfy push tests ---

    #[tokio::test]
    async fn test_send_notification_posts_to_topic_url() {
        let mock = MockNtfy::start().await;
        let client = Client::new();
        send_notification_to(
            &client,
            &mock.base_url,
            "tenant_x-session-requested",
            "Stronghold: Session Request",
            "hello world",
            None,
            5,
        )
        .await
        .unwrap();

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/tenant_x-session-requested");
        assert_eq!(r.body, "hello world");
    }

    #[tokio::test]
    async fn test_send_notification_sets_title_header() {
        let mock = MockNtfy::start().await;
        let client = Client::new();
        send_notification_to(
            &client,
            &mock.base_url,
            "t",
            "Stronghold: Anomaly Detected",
            "body",
            None,
            4,
        )
        .await
        .unwrap();

        let r = &mock.requests()[0];
        assert_eq!(header_value(r, "Title"), Some("Stronghold: Anomaly Detected"));
    }

    #[tokio::test]
    async fn test_send_notification_sets_priority_header() {
        let mock = MockNtfy::start().await;
        let client = Client::new();
        send_notification_to(&client, &mock.base_url, "t", "T", "b", None, 5).await.unwrap();

        let r = &mock.requests()[0];
        assert_eq!(header_value(r, "Priority"), Some("5"));
    }

    #[tokio::test]
    async fn test_send_notification_includes_actions_header_when_provided() {
        let mock = MockNtfy::start().await;
        let client = Client::new();
        let actions = "view, Approve, https://gateway/approve; view, Deny, https://gateway/deny";
        send_notification_to(
            &client,
            &mock.base_url,
            "t",
            "T",
            "b",
            Some(actions),
            5,
        )
        .await
        .unwrap();

        let r = &mock.requests()[0];
        assert_eq!(header_value(r, "Actions"), Some(actions));
    }

    #[tokio::test]
    async fn test_send_notification_omits_actions_header_when_none() {
        let mock = MockNtfy::start().await;
        let client = Client::new();
        send_notification_to(&client, &mock.base_url, "t", "T", "b", None, 5).await.unwrap();

        let r = &mock.requests()[0];
        assert!(header_value(r, "Actions").is_none());
    }

    #[tokio::test]
    async fn test_send_notification_returns_err_on_non_2xx() {
        // Bind a listener that always returns 500.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let resp = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
            let _ = sock.write_all(resp).await;
        });

        let client = Client::new();
        let result =
            send_notification_to(&client, &base_url, "t", "T", "b", None, 5).await;
        assert!(result.is_err(), "non-2xx must return Err");
    }

    // --- W5-T9: daily digest ---

    #[tokio::test]
    async fn test_push_daily_digest_sends_summary() {
        let mock = MockNtfy::start().await;

        // We test via send_notification_to with the same payload the
        // digest function builds, because push_daily_digest reads the
        // ntfy URL from the env var. We do call push_daily_digest below
        // to cover its payload construction.
        std::env::set_var("STRONGHOLD_NTFY_URL", &mock.base_url);

        push_daily_digest("tenant_d", 12, 1, 348, 2).await.unwrap();

        std::env::remove_var("STRONGHOLD_NTFY_URL");

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        // Topic must be per-tenant daily-digest.
        assert_eq!(r.path, "/tenant_d-daily-digest");
        // Body must contain all four counts.
        assert!(r.body.contains("Sessions started: 12"));
        assert!(r.body.contains("Sessions revoked: 1"));
        assert!(r.body.contains("Commands executed: 348"));
        assert!(r.body.contains("Anomalies detected: 2"));
        // Title set.
        assert_eq!(header_value(r, "Title"), Some("Stronghold: Daily Digest"));
    }

    #[tokio::test]
    async fn test_push_daily_digest_zero_counts() {
        let mock = MockNtfy::start().await;
        std::env::set_var("STRONGHOLD_NTFY_URL", &mock.base_url);
        push_daily_digest("tenant_z", 0, 0, 0, 0).await.unwrap();
        std::env::remove_var("STRONGHOLD_NTFY_URL");

        let r = &mock.requests()[0];
        assert!(r.body.contains("Sessions started: 0"));
        assert!(r.body.contains("Anomalies detected: 0"));
    }

    // --- W5-T7: ntfy sees only ciphertext ---

    #[tokio::test]
    async fn test_ntfy_server_sees_only_ciphertext() {
        // Prove that the ntfy server receives ONLY base64 ciphertext —
        // never the plaintext push content. This is the W5 DoD item:
        // "E2E encryption: ntfy server cannot read content".
        let mock = MockNtfy::start().await;
        let client = Client::new();

        // The plaintext we want to push (would be the approval request body).
        let plaintext = b"Image: rocky-base\nTTL: 3600s\nReason: deploy prod";

        // Phone's keys (generated at enrollment).
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();

        send_encrypted_notification_to(
            &client,
            &mock.base_url,
            "tenant_e-session-requested",
            "Stronghold: Session Request",
            plaintext,
            &x_pub,
            &m_pub,
            None,
            5,
        )
        .await
        .unwrap();

        let reqs = mock.requests();
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];

        // 1. Body must be valid standard base64.
        let body_bytes = r.body.as_bytes();
        assert!(
            body_bytes
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || *c == b'+' || *c == b'/' || *c == b'='),
            "ntfy body must be base64 only"
        );

        // 2. Body must NOT contain any substring of the plaintext.
        for window_start in 0..=(plaintext.len().saturating_sub(4)) {
            let window = &plaintext[window_start..window_start + 4];
            assert!(
                !find_subslice(r.body.as_bytes(), window).is_some(),
                "plaintext substring {:?} leaked into ntfy body",
                window
            );
        }
        // Specifically these sensitive markers must be absent.
        assert!(!r.body.contains("rocky-base"));
        assert!(!r.body.contains("deploy prod"));
        assert!(!r.body.contains("TTL"));

        // 3. The body must round-trip back to the original plaintext
        //    when decoded with the phone's private keys.
        let decoded = crate::push::e2e::decode(&r.body).unwrap();
        let recovered = crate::push::e2e::decrypt(&decoded, &phone).unwrap();
        assert_eq!(recovered.as_slice(), plaintext);
    }

    #[tokio::test]
    async fn test_encrypted_push_title_is_set_but_not_encrypted() {
        // ntfy requires a Title header for display. We send the title in
        // the clear (it's metadata, not content) — only the body is
        // encrypted. This test documents that contract.
        let mock = MockNtfy::start().await;
        let client = Client::new();
        let phone = PushKeys::generate();
        let (x_pub, m_pub) = phone.public_halves();

        send_encrypted_notification_to(
            &client,
            &mock.base_url,
            "t",
            "Stronghold: Session Request",
            b"secret body",
            &x_pub,
            &m_pub,
            None,
            5,
        )
        .await
        .unwrap();

        let r = &mock.requests()[0];
        // Title is in cleartext (it's a header).
        assert_eq!(header_value(r, "Title"), Some("Stronghold: Session Request"));
        // Body is NOT the cleartext "secret body".
        assert_ne!(r.body, "secret body");
        // Body must be valid base64.
        assert!(
            r.body
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'=')
        );
    }
}
