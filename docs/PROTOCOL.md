# Stronghold Agent Protocol

> ⚠️ **Alpha release.** Several steps described below (PTY `connect_token`
> verification, audit streaming, anomaly scanning, the `audit` WebSocket
> endpoint) are **NOT yet implemented** in the running gateway. Each affected
> section is marked with `❌ TODO` inline. The gateway also serves plain
> HTTP (TLS not wired in — see gap #1), so use `http://`, not `https://`, in
> the URLs below.

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
  "pty_endpoint": "ws://gateway/agent/mach_01HXYZ/pty",
  "audit_stream": "ws://gateway/agent/mach_01HXYZ/audit"
}
```

> ⚠️ `worker_sev_snp_attested` is **always `false`** in the alpha release.
> The scheduler does not consult SEV-SNP attestation status when placing
> pods (and SEV-SNP has never been tested on real hardware — gap #18).
>
> ⚠️ `audit_stream` URLs use `ws://` (not `wss://`) because TLS is not
> enabled on the gateway (gap #1). The `audit_stream` endpoint itself is
> also not yet implemented — see [GET /agent/:machine_id/audit](#get-agentmachine_idaudit-websocket).

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
  "pty_endpoint": "ws://gateway/agent/mach_01HXYZ/pty",
  "audit_stream": "ws://gateway/agent/mach_01HXYZ/audit"
}
```

> ⚠️ Same caveats as ORDER: `worker_sev_snp_attested` is always `false`,
> `pty_endpoint` / `audit_stream` use `ws://` (TLS not wired in — gap #1),
> and the audit-stream endpoint itself is not yet implemented.

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

The gateway is intended to:

1. ⚠️ **TODO** — Verify the connect token. *Currently NOT implemented: the
     PTY WebSocket does not check `connect_token` (gap #3). Anyone with the
     WS URL can attach to any session.*
2. ✅ Open a containerd exec session on the worker (real `kube exec` via
   kube-rs WebSocket).
3. ✅ Proxy bytes bidirectionally (stdin/stdout/stderr/tty).
4. ❌ **TODO** — Stream all bytes to the audit log in parallel. *Currently
   NOT wired in: the audit log writer exists, but the PTY proxy does not
   feed PTY bytes into it.*
5. ❌ **TODO** — Scan for anomaly patterns and push the phone if matched.
   *The anomaly scanner exists in `anomaly/mod.rs` but is not instantiated
   by the PTY proxy (gap #6).*

Binary WebSocket frames are PTY input/output. Text frames are control messages.

> ⚠️ Use `ws://`, not `wss://` — TLS is not wired into server startup
> (gap #1).

---

### GET /agent/:machine_id/audit (WebSocket)

> ❌ **Not yet implemented.** `routes/pty.rs::audit_stream()` immediately
> sends a "not yet implemented" message and returns. The read-only audit
> stream for tenant-side live session watching is a TODO.

*Target behavior (planned):* read-only audit stream that lets the tenant's
phone (via browser) watch a live session in real-time.

---

## Workflow Example

```bash
# 1. Agent requests a machine
#    NOTE: use http://, not https:// — TLS is not wired into the gateway (gap #1)
RESPONSE=$(curl -s -X POST http://gateway:8443/agent/order \
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

# 4. Agent opens PTY WebSocket (use ws://, not wss:// — gap #1)
#    WARNING: the PTY WS does not verify connect_token in alpha (gap #3).
#    Anyone with the WS URL can attach to the session.
# (use websocat or a WebSocket client)
websocat "$PTY_ENDPOINT" -H "Authorization: Bearer $CONNECT_TOKEN"

# 5. Agent works... chat session ends... connection drops
# 6. Machine is still running (TTL hasn't expired)

# 7. New chat session, agent resumes
RESUME=$(curl -s -X POST http://gateway:8443/agent/resume \
  -H "Authorization: Bearer $AGENT_TOKEN" \
  -H "Content-Type: application/json" \
  -d "{\"machine_id\": \"$MACHINE_ID\"}")

# 8. Agent reconnects to the same machine
# 9. After 4 hours, TTL expires, machine is killed
#    Or agent calls /agent/release early
#    Or tenant taps REVOKE on phone
```

---

## Error Codes

| Code | Meaning | Status (alpha) |
|------|---------|----------------|
| 200 | Success | ✅ |
| 401 | Invalid or missing agent token | ✅ |
| 403 | Phone denied the request | ✅ |
| 404 | Machine not found | ✅ |
| 408 | Phone approval timed out (60s) | ✅ |
| 410 | Machine expired or revoked | ✅ |
| 429 | Tenant quota exceeded | ✅ |
| 500 | Internal server error | ✅ |
| 502 | ntfy push failed | ✅ |
| 503 | No workers available with sufficient capacity | ⚠️ **Not yet returned** — the scheduler currently returns 500 or 429 for capacity issues; the dedicated 503 path is a TODO. The VPS-escalation fallback that would emit 503 is also a stub (gap #9). |
