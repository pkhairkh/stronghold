# ADR 0007: ntfy self-hosted, no custom phone app

## Status

Accepted

## Context

Stronghold needs to push notifications to the tenant's phone when:
- An agent requests a session (ORDER)
- An agent requests an extension (EXTEND)
- An anomaly is detected (exfiltration, destructive command)
- A session is about to expire

The question is: how do we deliver these notifications without building a custom phone app and without relying on external providers?

## Decision

Use **self-hosted ntfy** for push notifications. Use the phone's **native browser** for all UI. **No custom phone app.**

## Alternatives Considered

### Build a custom phone app (iOS + Android)
- **Pros:** Full control over UX, can do background processing, push notifications
- **Cons:** App store review process (Apple is notoriously slow), maintenance burden across iOS/Android versions, need to support multiple OS versions, need a developer account ($99/year Apple, $25 Google), users must install an unknown app

### Use APNs (Apple Push Notification Service) + FCM (Firebase Cloud Messaging)
- **Pros:** Native push, works when app is backgrounded
- **Cons:** External providers (Apple, Google) see all notification content. Requires a custom app to receive them. Requires Apple Developer account and Firebase project.

### Use a hosted notification service (Pushover, Pushbullet, etc.)
- **Pros:** Easy to integrate
- **Cons:** External provider, monthly fees, content visible to the provider, dependency on a third party

### Use email/SMS
- **Pros:** Universal
- **Cons:** Slow (email), expensive (SMS), not real-time

### ntfy self-hosted + browser
- **Pros:** Self-hosted (no external provider), open-source ntfy app exists (F-Droid / Play Store / TestFlight), supports end-to-end encryption, supports action buttons, supports UnifiedPush (Android, no FCM)
- **Cons:** iOS requires APNs as a wake-up trigger (content is still E2E encrypted), or use "instant delivery" polling mode (~2%/day battery)

## Consequences

### Positive
- No custom app to build, maintain, or get through app review
- Open-source ntfy app handles all the platform-specific push delivery
- Content is end-to-end encrypted (X25519 + ML-KEM-768 hybrid) — ntfy server sees ciphertext only
- APNs (if used on iOS) sees only "wake up," not content
- Android can use UnifiedPush — zero Google dependency
- iOS can use "instant delivery" mode — zero Apple dependency (at cost of ~2%/day battery)
- The ntfy app supports action buttons (Approve / Deny) that deep-link to the browser

### Negative
- Two-tap flow: notification → browser → Face ID (slightly more friction than a single-app flow)
- No "dashboard" view of all active sessions (you see them as separate notifications)
- Background delivery on iOS is less reliable than a native app with APNs content push

### Neutral
- The ntfy app is well-maintained, open-source, and widely used
- If we ever need a custom app, we can build one later — the protocol doesn't change

## Implementation

### ntfy server

Runs on the Vultr box (same as the gateway, or on workers):

```bash
# Install ntfy
dnf install -y ntfy

# Configure
cat > /etc/ntfy/server.yml << EOF
base-url: "https://gateway:8090"
listen-http: ":8090"
cache-file: "/var/lib/ntfy/cache.db"
EOF

systemctl enable ntfy
systemctl start ntfy
```

### Phone setup

1. Install the ntfy app (F-Droid / Play Store / TestFlight)
2. Add the Stronghold ntfy server URL
3. Subscribe to `<tenant-id>-session-requested`, `<tenant-id>-session-active`, `<tenant-id>-session-anomaly` topics
4. Authenticate with phone bearer token

### Push notification flow

```
Agent ORDERs → Gateway pushes to ntfy → ntfy pushes to phone
  ↓
Phone notification with [Approve] [Deny] action buttons
  ↓
Tenant taps Approve → ntfy opens browser → WebAuthn Face ID → gateway mints session
```

### End-to-end encryption

```rust
// Gateway encrypts payload with phone's X25519 + ML-KEM-768 public keys
let encrypted = push::e2e::encrypt(payload, phone_x25519_pub, phone_mlkem_pub)?;
let encoded = push::e2e::encode(&encrypted);

// Send to ntfy
ntfy::send_notification(&topic, title, &encoded, ...).await?;
```

The phone decrypts with its private halves (stored in IndexedDB).

## References

- [ntfy.sh](https://ntfy.sh)
- [ntfy self-hosting guide](https://docs.ntfy.sh/install/)
- [UnifiedPush](https://unifiedpush.org/)
