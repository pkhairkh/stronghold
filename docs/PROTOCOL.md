# Stronghold Agent Protocol

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
  "worker_sev_snp_attested": true,
  "pty_endpoint": "wss://gateway/agent/mach_01HXYZ/pty",
  "audit_stream": "wss://gateway/agent/mach_01HXYZ/audit"
}
```

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
  "worker_sev_snp_attested": true,
  "pty_endpoint": "wss://gateway/agent/mach_01HXYZ/pty",
  "audit_stream": "wss://gateway/agent/mach_01HXYZ/audit"
}
```

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
1. Verifies the connect token
2. Opens a containerd exec session on the worker
3. Proxies bytes bidirectionally
4. Streams all bytes to the audit log (in parallel)
5. Scans for anomaly patterns (pushes phone if matched)

Binary WebSocket frames are PTY input/output. Text frames are control messages.

---

### GET /agent/:machine_id/audit (WebSocket)

Read-only audit stream. Lets the tenant's phone (via browser) watch a live session in real-time.

---

## Workflow Example

```bash
# 1. Agent requests a machine
RESPONSE=$(curl -s -X POST https://gateway:8443/agent/order \
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

# 4. Agent opens PTY WebSocket
# (use websocat or a WebSocket client)
websocat "$PTY_ENDPOINT" -H "Authorization: Bearer $CONNECT_TOKEN"

# 5. Agent works... chat session ends... connection drops
# 6. Machine is still running (TTL hasn't expired)

# 7. New chat session, agent resumes
RESUME=$(curl -s -X POST https://gateway:8443/agent/resume \
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

| Code | Meaning |
|---|---|
| 200 | Success |
| 401 | Invalid or missing agent token |
| 403 | Phone denied the request |
| 404 | Machine not found |
| 408 | Phone approval timed out (60s) |
| 410 | Machine expired or revoked |
| 429 | Tenant quota exceeded |
| 500 | Internal server error |
| 502 | ntfy push failed |
| 503 | No workers available with sufficient capacity |
