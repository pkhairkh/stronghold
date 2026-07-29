# Stronghold Agent Protocol

> 🟡 **Beta release.** The protocol steps described below (PTY
> `connect_token` verification, audit streaming, anomaly scanning, the
> `audit` WebSocket endpoint) are now wired into the running gateway. The
> gateway now serves real HTTPS via `axum_server::bind_rustls()` (TLS 1.3 +
> X25519MLKEM768 hybrid), so use `https://` and `wss://` in the URLs below.
> Remaining limitations are noted inline with ⚠️ / ❌ markers.

## Overview

The agent protocol decouples **machine lifetime** from **agent connection**. A machine has a TTL (time-to-live); the agent attaches and detaches freely without killing it. This is critical for AI agents, whose chat sessions are short and ephemeral, but whose real builds take hours.

---

## Authentication

All endpoints require a `Bearer` token in the `Authorization` header:

```
Authorization: Bearer stronghold_agent_<token>
```

Agent tokens are minted by the tenant via the `stronghold agent-token mint` CLI command. Tokens are scoped per-tenant and TTL'd (default 24 hours).

---

## Endpoints

### POST /agent/order

Request a new machine. Triggers phone approval.

**Request:**
```json
{
  "image": "stronghold/rust-nightly:2026.07",
  "ttl_secs": 14400,
  "reason": "iterate on a Lean proof",
  "compute": {
    "cpu": 4,
    "memory_gb": 8,
    "dedicated": false,
    "gpu": false
  },
  "ephemeral_volumes": ["~/work", "~/.cache"]
}
```

**Response (200 OK — after phone approval):**
```json
{
  "machine_id": "mach_01HXYZ...",
  "connect_token": "stronghold_sess_...",
  "expires_at": "2026-07-29T18:23:00Z",
  "worker": "vultr-worker-3.fra1",
  "worker_sev_snp_attested": false,
  "pty_endpoint": "wss://gateway/agent/mach_01HXYZ/pty",
  "audit_stream": "wss://gateway/agent/mach_01HXYZ/audit"
}
```

> ⚠️ `worker_sev_snp_attested` is **always `false`** in the beta release.
> The scheduler does not consult SEV-SNP attestation status when placing
> pods (and SEV-SNP has never been tested on real hardware — hardware-blocked).
>
> ✅ `pty_endpoint` and `audit_stream` URLs now use `wss://` because the
> gateway terminates TLS (TLS 1.3 + X25519MLKEM768 hybrid).

**Response (403 Forbidden — phone denied):**
```json
{
  "error": "Session denied by tenant"
}
```

**Response (408 Request Timeout — no phone response in 60s):**
```json
{
  "error": "Approval timed out"
}
```

The HTTP request is held open (long-poll) for up to 60 seconds while waiting for the phone decision.

---

### POST /agent/resume

Reattach to an existing machine. Does NOT trigger phone approval (the original ORDER approval covers the machine's lifetime).

**Request:**
```json
{
  "machine_id": "mach_01HXYZ..."
}
```

**Response (200 OK):**
```json
{
  "machine_id": "mach_01HXYZ...",
  "connect_token": "stronghold_sess_...",
  "expires_at": "2026-07-29T18:23:00Z",
  "worker": "vultr-worker-3.fra1",
  "worker_sev_snp_attested": false,
  "pty_endpoint": "wss://gateway/agent/mach_01HXYZ/pty",
  "audit_stream": "wss://gateway/agent/mach_01HXYZ/audit"
}
```

> ⚠️ `worker_sev_snp_attested` is still always `false` (hardware-blocked).
> ✅ `pty_endpoint` / `audit_stream` use `wss://` (TLS is now wired in).

**Response (404 Not Found):** Machine does not exist or does not belong to this agent.

**Response (410 Gone):** Machine has expired or been revoked.

---

### POST /agent/release

Kill the machine early (agent-initiated).

**Request:**
```json
{
  "machine_id": "mach_01HXYZ..."
}
```

**Response (200 OK):** Machine killed, volumes snapshotted for 7 days.

---

### POST /agent/extend

Request more time. Triggers a new phone approval.

**Request:**
```json
{
  "machine_id": "mach_01HXYZ...",
  "additional_secs": 7200
}
```

**Response (200 OK — after phone approval):** Same as `/agent/resume`.

**Response (403 Forbidden):** Extension denied.

**Response (408 Request Timeout):** No phone response in 60s. Machine expires at original TTL.

---

### GET /agent/health

Health check. Returns 200 OK if the gateway is running.

---

### GET /agent/:machine_id/pty (WebSocket)

Bidirectional PTY connection. The agent opens this after a successful ORDER or RESUME.

**Headers:**
```
Authorization: Bearer <connect_token>
Upgrade: websocket
```

The gateway:

1. ✅ **Verifies the connect token.** Token is verified against its SHA-256
   hash stored in the `machines` table. Missing/wrong token → 401.
2. ✅ Opens a containerd exec session on the worker (real `kube exec` via
   kube-rs WebSocket).
3. ✅ Proxies bytes bidirectionally (stdin/stdout/stderr/tty).
4. ✅ **Streams all bytes to the audit log in parallel.** The audit log
   writer is fed PTY bytes by the proxy.
5. ✅ **Scans for anomaly patterns and writes audit entries if matched.**
   The anomaly scanner is instantiated per session and scans PTY bytes
   (detects `curl`/`wget`/`scp`, `rm -rf`, `sudo`, `ssh`). *Note:
   `push_anomaly()` is defined but not called — anomalies are audit-only;
   the phone is not pushed.*
6. ✅ **Enforces quorum on destructive commands.** Destructive commands are
   blocked, a `pending_sessions` row is created, the proxy polls for
   approval, and executes only on approval. *Note: no ntfy push fires for
   quorum requests — the phone polls the SSE stream instead.*

Binary WebSocket frames are PTY input/output. Text frames are control messages.

> ✅ Use `wss://`, not `ws://` — the gateway terminates TLS with the
> X25519MLKEM768 hybrid PQ key exchange.

---

### GET /agent/:machine_id/audit (WebSocket)

✅ **Implemented.** `routes/pty.rs::audit_stream()` long-polls the DB and
streams JSON audit entries to authorised clients. This is the read-only
audit stream that lets the tenant's phone (via browser) watch a live session
in real-time.

---

## Workflow Example

```bash
# 1. Agent requests a machine (gateway now serves HTTPS — use https://)
RESPONSE=$(curl -sk -X POST https://gateway:8443/agent/order \
  -H "Authorization: Bearer $AGENT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "image": "stronghold/rust-nightly:2026.07",
    "ttl_secs": 14400,
    "reason": "work on VUMA compiler",
    "compute": { "cpu": 4, "memory_gb": 8 }
  }')

# 2. Phone buzzes, tenant taps Approve, Face ID
# 3. Agent gets the response
MACHINE_ID=$(echo $RESPONSE | jq -r .machine_id)
CONNECT_TOKEN=$(echo $RESPONSE | jq -r .connect_token)
PTY_ENDPOINT=$(echo $RESPONSE | jq -r .pty_endpoint)

# 4. Agent opens PTY WebSocket (use wss:// — TLS is now wired in)
#    The PTY WS now verifies connect_token against its SHA-256 hash.
websocat "$PTY_ENDPOINT" -H "Authorization: Bearer $CONNECT_TOKEN"

# 5. Agent works... chat session ends... connection drops
# 6. Machine is still running (TTL hasn't expired)

# 7. New chat session, agent resumes
RESUME=$(curl -sk -X POST https://gateway:8443/agent/resume \
  -H "Authorization: Bearer $AGENT_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"machine_id\": \"$MACHINE_ID\"}")

# 8. Agent reconnects to the same machine
# 9. After 4 hours, TTL expires, machine is killed
#    Or agent calls /agent/release early
#    Or tenant taps REVOKE on phone
```

> `-k` skips TLS verification because the gateway uses a self-signed cert
> auto-generated on first boot. Pin the cert on the phone at enrollment.

---

## Error Codes

| Code | Meaning | Status (beta) |
|------|---------|----------------|
| 200 | Success | ✅ |
| 401 | Invalid or missing agent token (or PTY `connect_token` mismatch) | ✅ |
| 403 | Phone denied the request | ✅ |
| 404 | Machine not found | ✅ |
| 408 | Phone approval timed out (60s) | ✅ |
| 410 | Machine expired or revoked | ✅ |
| 429 | Tenant quota exceeded | ✅ |
| 500 | Internal server error | ✅ |
| 502 | ntfy push failed | ✅ |
| 503 | No workers available with sufficient capacity, OR global concurrency limit hit | ✅ — the global concurrency limiter returns 503 when the 100-session cap is exceeded. The dedicated 503 path for capacity issues is also returned by the scheduler. (VPS-escalation fallback that would emit 503 is still a stub — deferred to the v1.0 RC.) |
