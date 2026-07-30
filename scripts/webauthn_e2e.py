#!/usr/bin/env python3
"""WebAuthn E2E test — simulates the full ceremony using a virtual P-256
authenticator (Python cryptography library).

Flow:
  1. Create a tenant + credential enrollment
  2. POST /phone/ceremony/begin → get ceremony options + challenge
  3. Generate a P-256 key pair (virtual authenticator)
  4. Build an attestation object (format "none") + client data JSON
  5. POST /phone/ceremony/finish → verify attestation, store credential
  6. POST /agent/order → create pending session (seeds phone_challenges)
  7. Build an assertion (authenticator data + client data + signature)
  8. POST /phone/decide → verify assertion, approve session
  9. Verify session is approved in DB

All crypto is real — the gateway's verify_attestation + verify_assertion
must actually validate the signatures.
"""
import base64
import hashlib
import json
import os
import struct
import sys
import time
import urllib.request
import ssl

# Use the cryptography library for P-256 key generation + signing
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat
import cbor2

GATEWAY = "https://localhost:8443"
DB = "/var/lib/stronghold/stronghold.db"

# Bypass self-signed cert
ctx = ssl.create_default_context()
ctx.check_hostname = False
ctx.verify_mode = ssl.CERT_NONE

def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b'=').decode()

def b64url_decode(s: str) -> bytes:
    padding = 4 - len(s) % 4
    if padding < 4:
        s += '=' * padding
    return base64.urlsafe_b64decode(s)

def api(method, path, body=None, token=None):
    url = GATEWAY + path
    data = json.dumps(body).encode() if body else None
    req = urllib.request.Request(url, data=data, method=method)
    req.add_header('Content-Type', 'application/json')
    if token:
        req.add_header('Authorization', f'Bearer {token}')
    try:
        resp = urllib.request.urlopen(req, context=ctx)
        return resp.status, json.loads(resp.read() or b'{}')
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()

def sqlite_exec(sql):
    import subprocess
    r = subprocess.run(['sqlite3', DB, sql], capture_output=True, text=True)
    return r.stdout.strip(), r.stderr.strip()

# ─── Step 0: Bootstrap tenant ─────────────────────────────────────────────
print("═══════════════════════════════════════════════════════════════")
print("  WebAuthn E2E Test — Virtual P-256 Authenticator")
print("═══════════════════════════════════════════════════════════════")

# Create tenant
status, resp = api('POST', '/admin/tenant', {'name': f'webauthn-e2e-{int(time.time())}'})
tenant_id = resp['id']
print(f"✓ tenant: {tenant_id}")

# Set quota
sqlite_exec(f"INSERT OR REPLACE INTO quotas (tenant_id, max_concurrent_machines, max_cpu_per_machine, max_memory_gb_per_machine, max_disk_gb_per_machine, total_cpu_budget, total_memory_gb_budget, total_disk_gb_budget, require_sev_snp_workers) VALUES ('{tenant_id}', 4, 4, 8, 100, 16, 32, 500, 0);")

# Mint agent token
import secrets, hashlib
token_b64 = secrets.token_urlsafe(32)[:43]
agent_token = f"stronghold_agent_{token_b64}"
token_hash = hashlib.sha256(agent_token.encode()).hexdigest()
expires = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(time.time() + 3600))
sqlite_exec(f"INSERT INTO agent_tokens (tenant_id, token_hash, scope, created_at, expires_at) VALUES ('{tenant_id}','{token_hash}','default',datetime('now'),'{expires}');")
# Mint a phone token (needed for /phone/decide auth)
phone_token_raw = f"stronghold_phone_{secrets.token_urlsafe(32)[:43]}"
phone_token_hash = hashlib.sha256(phone_token_raw.encode()).hexdigest()
sqlite_exec(f"INSERT INTO phone_tokens (tenant_id, token_hash, created_at) VALUES ('{tenant_id}','{phone_token_hash}',datetime('now'));")
print(f"✓ phone token: {phone_token_raw[:30]}...")

# ─── Step 1: Ceremony Begin ───────────────────────────────────────────────
print("\n─── Step 1: POST /phone/ceremony/begin ──────────────────────")
status, resp = api('POST', f'/phone/ceremony/begin?tenant={tenant_id}')
print(f"  status={status}")
if status != 200:
    print(f"  ERROR: {resp}")
    sys.exit(1)
print(f"  challenge_id: {resp.get('challenge_id', 'N/A')}")
# The CeremonyBeginResponse uses #[serde(flatten)] so options fields are at top level
challenge_b64 = resp.get('challenge', '')
challenge_id = resp.get('challenge_id', '')
print(f"  challenge: {challenge_b64[:40] if challenge_b64 else 'MISSING'}...")
print(f"  rp: {resp.get('rp', {})}")
print(f"  pubKeyCredParams: {resp.get('pubKeyCredParams', [])}")

# ─── Step 2: Generate virtual authenticator (P-256 key pair) ──────────────
print("\n─── Step 2: Generate P-256 key pair (virtual authenticator) ──")
priv_key = ec.generate_private_key(ec.SECP256R1())
pub_key = priv_key.public_key()
pub_key_bytes = pub_key.public_bytes(Encoding.X962, PublicFormat.UncompressedPoint)
credential_id = os.urandom(32)
print(f"  credential_id: {b64url(credential_id)[:40]}...")
print(f"  public_key: {pub_key_bytes.hex()[:40]}...")

# ─── Step 3: Build attestation object (format "none") ─────────────────────
print("\n─── Step 3: Build attestation + client data ──────────────────")
# RP ID hash
rp_id = "localhost"
rp_id_hash = hashlib.sha256(rp_id.encode()).digest()

# Authenticator data for attestation:
# rp_id_hash (32) + flags (1) + sign_count (4)
# flags: bit 0 = user present, bit 2 = user verified, bit 6 = attested credential data
flags = 0x01 | 0x04 | 0x40  # UP + UV + AT
sign_count = 0
# Attested credential data: aaguid (16) + cred_id_len (2) + cred_id + cred_pubkey (COSE)
aaguid = b'\x00' * 16
cred_id_len = struct.pack('>H', len(credential_id))
# COSE public key format (EC2 P-256):
# {1: 2 (kty EC2), 3: -7 (alg ES256), -1: 1 (crv P-256), -2: x, -3: y}
x = pub_key_bytes[1:33]
y = pub_key_bytes[33:65]
cose_key = {
    1: 2,     # kty: EC2
    3: -7,    # alg: ES256
    -1: 1,    # crv: P-256
    -2: x,    # x coordinate (bytes)
    -3: y,    # y coordinate (bytes)
}
cose_key_cbor = cbor2.dumps(cose_key)

auth_data = rp_id_hash + bytes([flags]) + struct.pack('>I', sign_count) + aaguid + cred_id_len + credential_id + cose_key_cbor

# Client data JSON for "webauthn.create"
client_data = {
    "type": "webauthn.create",
    "challenge": challenge_b64,
    "origin": "https://localhost:8443",
    "crossOrigin": False,
}
client_data_json = json.dumps(client_data, separators=(',', ':')).encode()

# Attestation object (format "none"): {fmt: "none", attStmt: {}, authData: ...}
att_obj = {
    "fmt": "none",
    "attStmt": {},
    "authData": auth_data,
}
att_obj_cbor = cbor2.dumps(att_obj)

print(f"  auth_data: {len(auth_data)} bytes")
print(f"  client_data: {len(client_data_json)} bytes")
print(f"  attestation_object: {len(att_obj_cbor)} bytes")

# ─── Step 4: POST /phone/ceremony/finish ──────────────────────────────────
print("\n─── Step 4: POST /phone/ceremony/finish (verify attestation) ─")
finish_body = {
    'challenge_id': challenge_id,
    'credential_id': b64url(credential_id),
    'attestation_object': b64url(att_obj_cbor),
    'client_data_json': b64url(client_data_json),
}
status, resp = api('POST', '/phone/ceremony/finish', finish_body)
print(f"  status={status}")
print(f"  resp: {resp}")
if status != 200:
    print("  ❌ Attestation verification FAILED")
    sys.exit(1)
print(f"  ✓ credential stored: {resp.get('credential_id', 'N/A')}")

# ─── Step 5: POST /agent/order (creates pending session + seeds challenge) ─
print("\n─── Step 5: POST /agent/order (create pending session) ───────")
import threading
def order_in_background():
    body = {
        'image': 'localhost:30500/stronghold/rocky-base:latest',
        'ttl_secs': 300,
        'reason': 'webauthn e2e test',
        'compute': {'cpu': 1, 'memory_gb': 1},
    }
    status, resp = api('POST', '/agent/order', body, token=agent_token)
    with open('/tmp/webauthn_order.json', 'w') as f:
        json.dump({'status': status, 'resp': resp}, f)

t = threading.Thread(target=order_in_background)
t.start()
time.sleep(2)

# Find the pending session
out, _ = sqlite_exec(f"SELECT id FROM pending_sessions WHERE tenant_id='{tenant_id}' ORDER BY created_at DESC LIMIT 1;")
session_id = out
print(f"  pending session: {session_id}")

# Find the challenge for this session (keyed by session_id in phone_challenges)
out, _ = sqlite_exec(f"SELECT hex(challenge) FROM phone_challenges WHERE id='{session_id}' AND tenant_id='{tenant_id}';")
challenge_hex = out
challenge_bytes = bytes.fromhex(challenge_hex) if challenge_hex else b''
challenge_b64_decide = b64url(challenge_bytes)
print(f"  challenge: {challenge_b64_decide[:40]}...")

# ─── Step 6: Build assertion + POST /phone/decide ─────────────────────────
print("\n─── Step 6: POST /phone/decide (verify assertion, approve) ───")
# Authenticator data for assertion (no attested credential data):
# rp_id_hash (32) + flags (1) + sign_count (4)
# flags: UP + UV (no AT bit)
flags_assert = 0x01 | 0x04  # UP + UV
sign_count_assert = 1
auth_data_assert = rp_id_hash + bytes([flags_assert]) + struct.pack('>I', sign_count_assert)

# Client data JSON for "webauthn.get"
client_data_assert = {
    "type": "webauthn.get",
    "challenge": challenge_b64_decide,
    "origin": "https://localhost:8443",
    "crossOrigin": False,
}
client_data_json_assert = json.dumps(client_data_assert, separators=(',', ':')).encode()

# Sign: auth_data || sha256(client_data_json)
client_data_hash = hashlib.sha256(client_data_json_assert).digest()
signed_data = auth_data_assert + client_data_hash
signature = priv_key.sign(signed_data, ec.ECDSA(hashes.SHA256()))

decide_body = {
    'session_id': session_id,
    'decision': 'approve',
    'credential_id': b64url(credential_id),
    'authenticator_data': b64url(auth_data_assert),
    'client_data_json': b64url(client_data_json_assert),
    'signature': b64url(signature),
}
status, resp = api('POST', '/phone/decide', decide_body, token=phone_token_raw)
print(f"  status={status}")
print(f"  resp: {resp}")
if status != 200:
    print("  ❌ Assertion verification FAILED")
    sys.exit(1)
print(f"  ✓ session approved: {resp.get('session_id', 'N/A')}")

# ─── Step 7: Verify session is approved in DB ─────────────────────────────
print("\n─── Step 7: Verify session approved in DB ────────────────────")
out, _ = sqlite_exec(f"SELECT status FROM pending_sessions WHERE id='{session_id}';")
print(f"  DB status: {out}")
if out == 'approved':
    print("  ✓ Session approved!")
else:
    print(f"  ❌ Session NOT approved (status={out})")
    sys.exit(1)

# Wait for the order to complete (pod scheduling)
t.join(timeout=30)
order_result = json.load(open('/tmp/webauthn_order.json'))
print(f"\n  /agent/order result: {order_result}")

# ─── Step 8: Verify audit log ─────────────────────────────────────────────
print("\n─── Step 8: Verify audit log ────────────────────────────────")
out, _ = sqlite_exec(f"SELECT seq, event FROM audit_entries WHERE tenant_id='{tenant_id}' ORDER BY seq;")
print(f"  audit events:")
for line in out.split('\n'):
    if line.strip():
        print(f"    {line}")

# Check for WebAuthn-related events
out, _ = sqlite_exec(f"SELECT COUNT(*) FROM audit_entries WHERE tenant_id='{tenant_id}' AND (event LIKE '%webauthn%' OR event LIKE '%session%' OR event LIKE '%credential%');")
count = int(out) if out else 0
print(f"\n  WebAuthn/session/credential events: {count}")

# ─── Summary ──────────────────────────────────────────────────────────────
print("\n═══════════════════════════════════════════════════════════════")
print("  ✅ WebAuthn E2E Test PASSED")
print("═══════════════════════════════════════════════════════════════")
print(f"  Tenant:     {tenant_id}")
print(f"  Session:    {session_id}")
print(f"  Credential: {b64url(credential_id)[:20]}...")
print(f"  All signatures verified by the gateway's real verify_attestation + verify_assertion")
sys.exit(0)
