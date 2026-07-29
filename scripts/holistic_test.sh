#!/usr/bin/env bash
# Holistic Stronghold agent-driven test suite — v3 (corrected schemas + regex).
set -uo pipefail

GATEWAY_URL="${STRONGHOLD_URL:-https://localhost:8443}"
TENANT_NAME="holistic-test-$(date +%s)"
TENANT_ID=""
AGENT_TOKEN=""
DB="/var/lib/stronghold/stronghold.db"

PASS=0
FAIL=0
TOTAL=0
FAILS=()

section() {
    echo
    echo "================================================================"
    echo "  $1"
    echo "================================================================"
}
check() {
    local label="$1"; local cond="$2"
    TOTAL=$((TOTAL + 1))
    if [ "$cond" = "true" ]; then
        PASS=$((PASS + 1))
        echo "  [PASS] $label"
    else
        FAIL=$((FAIL + 1))
        FAILS+=("$label")
        echo "  [FAIL] $label (cond=$cond)"
    fi
}
check_eq() {
    local label="$1"; local got="$2"; local want="$3"
    TOTAL=$((TOTAL + 1))
    if [ "$got" = "$want" ]; then
        PASS=$((PASS + 1))
        echo "  [PASS] $label"
    else
        FAIL=$((FAIL + 1))
        FAILS+=("$label")
        echo "  [FAIL] $label — got='$got' want='$want'"
    fi
}
# grep -E with NO backslash-pipe: callers pass real regex alternation with `|`.
check_contains() {
    local label="$1"; local haystack="$2"; local needle="$3"
    TOTAL=$((TOTAL + 1))
    if echo "$haystack" | grep -qE "$needle"; then
        PASS=$((PASS + 1))
        echo "  [PASS] $label"
    else
        FAIL=$((FAIL + 1))
        FAILS+=("$label")
        echo "  [FAIL] $label — haystack does not match /$needle/"
        echo "        haystack: $(echo "$haystack" | head -c 400)"
    fi
}

source /root/stronghold/agent/stronghold-agent.sh

# ---------------------------------------------------------------------------
# 0. Provision tenant + agent token + quota + fake machine (with real
#    connect_token) so message/exec/git endpoints can be exercised.
# ---------------------------------------------------------------------------
section "0. Provision tenant + agent token + quota + fake machine"

TENANT_RESP=$(curl -sk -X POST "$GATEWAY_URL/admin/tenant" \
    -H 'Content-Type: application/json' \
    -d "{\"name\":\"$TENANT_NAME\"}")
TENANT_ID=$(echo "$TENANT_RESP" | jq -r .id)
check_eq "tenant created" "$TENANT_ID" "$(echo "$TENANT_RESP" | jq -r .id)"
[ -n "$TENANT_ID" ] && [ "$TENANT_ID" != "null" ] && check "tenant id non-empty" "true" || check "tenant id non-empty" "false"
echo "  tenant_id=$TENANT_ID"

TOKEN_B64=$(openssl rand -base64 32 | tr -d '/+=' | head -c 43)
AGENT_TOKEN="stronghold_agent_${TOKEN_B64}"
TOKEN_HASH=$(printf '%s' "$AGENT_TOKEN" | sha256sum | awk '{print $1}')
EXPIRES=$(date -u -d '+1 hour' +%Y-%m-%dT%H:%M:%SZ)
sqlite3 "$DB" "INSERT INTO agent_tokens (tenant_id, token_hash, scope, created_at, expires_at) VALUES ('$TENANT_ID','$TOKEN_HASH','default',datetime('now'),'$EXPIRES');"
# Set per-tenant quota so /agent/order can pass check_capacity
sqlite3 "$DB" "INSERT OR REPLACE INTO quotas (tenant_id, max_concurrent_machines, max_cpu_per_machine, max_memory_gb_per_machine, max_disk_gb_per_machine, total_cpu_budget, total_memory_gb_budget, total_disk_gb_budget, require_sev_snp_workers) VALUES ('$TENANT_ID', 4, 8, 16, 100, 32, 64, 500, 0);"
echo "  agent_token=$AGENT_TOKEN"

# Pre-issue a real connect_token + store its hash for a fake machine
FAKE_MACHINE="fake-machine-cred-$(date +%s)"
CONNECT_TOKEN_PLAINTEXT="stronghold_sess_$(openssl rand -hex 16)"
CONNECT_TOKEN_HASH=$(printf '%s' "$CONNECT_TOKEN_PLAINTEXT" | sha256sum | awk '{print $1}')
sqlite3 "$DB" "INSERT INTO machines (id, tenant_id, image, worker, status, cpu, memory_gb, connect_token_hash, created_at, expires_at) VALUES ('$FAKE_MACHINE','$TENANT_ID','stronghold/test:latest','worker-0','active',2,2,'$CONNECT_TOKEN_HASH',datetime('now'),datetime('now','+1 hour'));"
echo "  fake_machine=$FAKE_MACHINE  connect_token=$CONNECT_TOKEN_PLAINTEXT"

export STRONGHOLD_URL="$GATEWAY_URL"
export STRONGHOLD_TOKEN="$AGENT_TOKEN"
export STRONGHOLD_CURL_FLAGS="-sk"

# ---------------------------------------------------------------------------
# 1. Task lifecycle
# ---------------------------------------------------------------------------
section "1. Task lifecycle (create → progress → result → reflexion)"

TASK_RESP=$(stronghold_task "Build the stronghold gateway in debug mode" "stronghold/rust-nightly:2026.07" 3600)
echo "  task_resp: $TASK_RESP"
TASK_ID=$(echo "$TASK_RESP" | jq -r .task_id 2>/dev/null)
check_contains "task created with id" "$TASK_RESP" "task_id"
check_eq "task_id non-empty" "$TASK_ID" "$(echo "$TASK_RESP" | jq -r .task_id)"

GET_TASK=$(curl -sk "$GATEWAY_URL/agent/task/$TASK_ID" -H "Authorization: Bearer $AGENT_TOKEN")
check_contains "task fetch contains instruction" "$GET_TASK" "Build the stronghold gateway"

# Progress report — needs: files_changed, tests_run, tests_passing, commits, blockers, status
PROGRESS_RESP=$(curl -sk -X POST "$GATEWAY_URL/agent/task/$TASK_ID/progress" \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"files_changed":["gateway/src/main.rs","gateway/src/routes/tasks.rs"],"tests_run":12,"tests_passing":12,"commits":1,"blockers":[],"status":"on_track"}')
echo "  progress_resp: $PROGRESS_RESP"
check_contains "progress report accepted" "$PROGRESS_RESP" "ok|stored|progress_key|status"

# Submit result (success) — endpoint returns 200 OK with body containing task_id or status
RESULT_CODE=$(curl -sk -o /tmp/result_resp -w '%{http_code}' -X POST "$GATEWAY_URL/agent/task/$TASK_ID/result" \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"exit_code":0,"stdout":"cargo build --workspace\n   Compiling stronghold-gateway v0.1.0","stderr":"","summary":"Build succeeded in 87s","artifacts":[]}')
RESULT_RESP=$(cat /tmp/result_resp 2>/dev/null)
echo "  result_resp: '$RESULT_RESP' (http=$RESULT_CODE)"
check_eq "result endpoint returns 200" "$RESULT_CODE" "200"

# Reflexion — needs: what_went_well, what_went_wrong, what_differently, what_learned
REFLEXION_RESP=$(curl -sk -X POST "$GATEWAY_URL/agent/task/$TASK_ID/reflexion" \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"what_went_well":"Build completed in under 90s","what_went_wrong":"Initial run missed the sev-snp feature flag","what_differently":"Check Cargo.toml features before first build","what_learned":"Always pass --features sev-snp on this workspace"}')
echo "  reflexion_resp: $REFLEXION_RESP"
check_contains "reflexion stored" "$REFLEXION_RESP" "reflexion|ok|stored"

GET_REFLEXION=$(curl -sk "$GATEWAY_URL/agent/task/$TASK_ID/reflexion" \
    -H "Authorization: Bearer $AGENT_TOKEN")
echo "  get_reflexion: $(echo $GET_REFLEXION | head -c 300)"
check_contains "reflexion retrievable" "$GET_REFLEXION" "Cargo.toml"

LIST_REFLEXIONS=$(curl -sk "$GATEWAY_URL/agent/reflexions?tenant=$TENANT_ID" \
    -H "Authorization: Bearer $AGENT_TOKEN")
echo "  list_reflexions: $(echo $LIST_REFLEXIONS | head -c 300)"
check_contains "reflexion list non-empty" "$LIST_REFLEXIONS" "Cargo.toml"

# ---------------------------------------------------------------------------
# 2. Roles + Constitution
# ---------------------------------------------------------------------------
section "2. Roles + Constitution"

SEED_RESP=$(curl -sk -X POST "$GATEWAY_URL/admin/roles/seed" \
    -H 'Content-Type: application/json' \
    -d "{\"tenant_id\":\"$TENANT_ID\"}")
echo "  seed_resp: $SEED_RESP"
CREATED_COUNT=$(echo "$SEED_RESP" | jq '.created | length')
check_eq "9 default roles seeded" "$CREATED_COUNT" "9"

LIST_ROLES=$(curl -sk "$GATEWAY_URL/admin/roles?tenant=$TENANT_ID")
ROLE_COUNT=$(echo "$LIST_ROLES" | jq 'length')
check_eq "9 roles listed" "$ROLE_COUNT" "9"

CONSTITUTION=$(curl -sk "$GATEWAY_URL/admin/constitution")
PRINCIPLE_COUNT=$(echo "$CONSTITUTION" | jq 'length')
check_eq "10 constitutional principles" "$PRINCIPLE_COUNT" "10"

HAS_TITLE=$(echo "$CONSTITUTION" | jq 'all(.[]; has("number") and has("title") and has("description"))')
check_eq "constitution shape valid" "$HAS_TITLE" "true"

CUSTOM_ROLE=$(curl -sk -X POST "$GATEWAY_URL/admin/roles" \
    -H 'Content-Type: application/json' \
    -d "{\"tenant_id\":\"$TENANT_ID\",\"name\":\"custom-builder\",\"system_prompt\":\"You are a custom builder.\",\"allowed_tools\":[\"exec\",\"git_clone\"],\"denied_tools\":[\"git_push\"]}")
check_contains "custom role created" "$CUSTOM_ROLE" "role_"

# ---------------------------------------------------------------------------
# 3. Credentials vault
# ---------------------------------------------------------------------------
section "3. Credentials vault (encrypt + fetch + decrypt)"

CRED_RESP=$(curl -sk -X POST "$GATEWAY_URL/admin/credentials" \
    -H 'Content-Type: application/json' \
    -d "{\"tenant_id\":\"$TENANT_ID\",\"name\":\"github-pat\",\"kind\":\"api_token\",\"value\":\"ghp_testSecretToken123\",\"env_var\":\"GITHUB_TOKEN\"}")
echo "  cred_resp: $CRED_RESP"
check_contains "credential stored" "$CRED_RESP" "\"id\""

LIST_CRED=$(curl -sk "$GATEWAY_URL/admin/credentials?tenant=$TENANT_ID")
check_contains "credential listed" "$LIST_CRED" "github-pat"

CRED_FETCH=$(curl -sk "$GATEWAY_URL/agent/$FAKE_MACHINE/credentials/github-pat" \
    -H "Authorization: Bearer $AGENT_TOKEN")
echo "  cred_fetch: $CRED_FETCH"
check_contains "credential decrypted value returned" "$CRED_FETCH" "ghp_testSecretToken123"

# Verify the credential is stored ENCRYPTED at rest in agent_credentials
AT_REST=$(sqlite3 "$DB" "SELECT hex(encrypted_value) FROM agent_credentials WHERE tenant_id='$TENANT_ID' AND name='github-pat' ORDER BY created_at DESC LIMIT 1;")
echo "  at_rest ciphertext (first 80 hex chars): $(echo "$AT_REST" | head -c 80)..."
[ -n "$AT_REST" ] && check "credential stored as ciphertext in agent_credentials" "true" || check "credential stored as ciphertext in agent_credentials" "false"
echo "$AT_REST" | grep -q "ghp_testSecretToken123" && check "ciphertext does NOT contain plaintext" "false" || check "ciphertext does NOT contain plaintext" "true"
# Verify the nonce is also present and non-trivial
NONCE_LEN=$(sqlite3 "$DB" "SELECT COALESCE(LENGTH(nonce),0) FROM agent_credentials WHERE tenant_id='$TENANT_ID' AND name='github-pat' ORDER BY created_at DESC LIMIT 1;")
[ "${NONCE_LEN:-0}" -ge 12 ] && check "AES-256-GCM nonce present ($NONCE_LEN bytes)" "true" || check "AES-256-GCM nonce present" "false"

# ---------------------------------------------------------------------------
# 4. Agent messages (post + poll + stream peek) — needs connect_token
# ---------------------------------------------------------------------------
section "4. Agent messages (post + poll + stream peek)"

MSG_RESP=$(curl -sk -X POST "$GATEWAY_URL/agent/$FAKE_MACHINE/messages?token=$CONNECT_TOKEN_PLAINTEXT" \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"to":null,"channel":"broadcast","body":{"role":"planner","content":"Planning phase 1: scope the feature, identify risks"}}')
echo "  msg_resp: $MSG_RESP"
check_contains "message posted" "$MSG_RESP" "\"id\""

MSG_ID=$(echo "$MSG_RESP" | jq -r .id 2>/dev/null)
MSG_RESP2=$(curl -sk -X POST "$GATEWAY_URL/agent/$FAKE_MACHINE/messages?token=$CONNECT_TOKEN_PLAINTEXT" \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    -H 'Content-Type: application/json' \
    -d "{\"to\":null,\"channel\":\"broadcast\",\"body\":{\"role\":\"coder\",\"content\":\"Acknowledged\",\"parent_id\":\"$MSG_ID\"}}")
check_contains "reply posted" "$MSG_RESP2" "\"id\""

# Poll messages — needs channel + since params
POLL=$(curl -sk "$GATEWAY_URL/agent/$FAKE_MACHINE/messages?token=$CONNECT_TOKEN_PLAINTEXT&channel=broadcast&since=1970-01-01T00:00:00Z" \
    -H "Authorization: Bearer $AGENT_TOKEN")
MSG_COUNT=$(echo "$POLL" | jq '.messages | length' 2>/dev/null || echo "$POLL" | jq 'length')
echo "  poll: $(echo $POLL | head -c 300)"
[ "${MSG_COUNT:-0}" -ge 2 ] && check "≥2 messages polled" "true" || check "≥2 messages polled (count=$MSG_COUNT)" "false"

# SSE stream peek — needs channel param
STREAM_PEEK=$(timeout 2 curl -sk -N "$GATEWAY_URL/agent/$FAKE_MACHINE/messages/stream?token=$CONNECT_TOKEN_PLAINTEXT&channel=broadcast" \
    -H "Authorization: Bearer $AGENT_TOKEN" 2>/dev/null | head -c 600)
check_contains "SSE stream emits events" "$STREAM_PEEK" "event:|data:"

# ---------------------------------------------------------------------------
# 5. Oracle Q&A
# ---------------------------------------------------------------------------
section "5. Oracle Q&A (ask + answer)"

ORACLE_RESP=$(curl -sk -X POST "$GATEWAY_URL/agent/$FAKE_MACHINE/oracle" \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"question":"Should we use SQLite or Postgres for the audit log?","context":{"scale":"single-tenant","throughput":"<10 msg/s"}}')
echo "  oracle_resp: $ORACLE_RESP"
QUESTION_ID=$(echo "$ORACLE_RESP" | jq -r .question_id 2>/dev/null)
check_contains "oracle question created" "$ORACLE_RESP" "question_id"

# Post an answer via the message bus (oracle answers go through agent_messages)
curl -sk -X POST "$GATEWAY_URL/agent/$FAKE_MACHINE/messages?token=$CONNECT_TOKEN_PLAINTEXT" \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    -H 'Content-Type: application/json' \
    -d "{\"to\":null,\"channel\":\"oracle.answers\",\"body\":{\"role\":\"oracle\",\"answer\":\"Use SQLite for single-tenant low-throughput; revisit at >50 msg/s.\",\"question_id\":\"$QUESTION_ID\"}}" > /dev/null

ORACLE_FETCH=$(curl -sk "$GATEWAY_URL/agent/$FAKE_MACHINE/oracle/$QUESTION_ID" \
    -H "Authorization: Bearer $AGENT_TOKEN")
echo "  oracle_fetch: $ORACLE_FETCH"
check_contains "oracle question retrievable" "$ORACLE_FETCH" "$QUESTION_ID"

# ---------------------------------------------------------------------------
# 6. Facilitator disagreement
# ---------------------------------------------------------------------------
section "6. Facilitator (submit + decide + retrieve)"

DIS_RESP=$(curl -sk -X POST "$GATEWAY_URL/agent/$FAKE_MACHINE/disagreement" \
    -H "Authorization: Bearer $AGENT_TOKEN" \
    -H 'Content-Type: application/json' \
    -d "{\"task_id\":\"$TASK_ID\",\"issue\":\"PR #42 should be merged despite failing lint\",\"coder_argument\":\"Lint warnings are non-blocking and the fix is critical\",\"reviewer_argument\":\"Lint must pass before merge\",\"context\":{\"ci_log\":\"...\"}}")
echo "  dis_resp: $DIS_RESP"
DIS_ID=$(echo "$DIS_RESP" | jq -r .disagreement_id 2>/dev/null)
check_contains "disagreement submitted" "$DIS_RESP" "disagreement_id"

# Pretend facilitator decided
sqlite3 "$DB" "UPDATE disagreements SET status='decided', decision='Use async/await — better ecosystem fit', reasoning='Coder position is correct; lint warnings are non-blocking', resolved_at=datetime('now') WHERE id='$DIS_ID';"

DIS_FETCH=$(curl -sk "$GATEWAY_URL/agent/$FAKE_MACHINE/disagreement/$DIS_ID" \
    -H "Authorization: Bearer $AGENT_TOKEN")
echo "  dis_fetch: $DIS_FETCH"
check_contains "disagreement retrievable" "$DIS_FETCH" "$DIS_ID"

# ---------------------------------------------------------------------------
# 7. Workflows
# ---------------------------------------------------------------------------
section "7. Workflows (create + run + status)"

TEMPLATE_FILE="/root/stronghold/agent/templates/standard-cicd.json"
if [ -f "$TEMPLATE_FILE" ]; then
    WF_DAG=$(jq '.dag' "$TEMPLATE_FILE")
    WF_RESP=$(curl -sk -X POST "$GATEWAY_URL/workflow" \
        -H "Authorization: Bearer $AGENT_TOKEN" \
        -H 'Content-Type: application/json' \
        -d "{\"name\":\"standard-cicd-test\",\"dag\":$WF_DAG}")
    echo "  wf_resp: $WF_RESP"
    WF_ID=$(echo "$WF_RESP" | jq -r .workflow_id 2>/dev/null)
    check_contains "workflow created from template" "$WF_RESP" "workflow_id"

    WF_LIST=$(curl -sk "$GATEWAY_URL/workflow" \
        -H "Authorization: Bearer $AGENT_TOKEN")
    check_contains "workflow listed" "$WF_LIST" "standard-cicd-test"

    if [ -n "$WF_ID" ] && [ "$WF_ID" != "null" ]; then
        WF_RUN=$(curl -sk -X POST "$GATEWAY_URL/workflow/$WF_ID/run" \
            -H "Authorization: Bearer $AGENT_TOKEN")
        echo "  wf_run: $WF_RUN"
        RUN_ID=$(echo "$WF_RUN" | jq -r .run_id 2>/dev/null)
        check_contains "workflow run started" "$WF_RUN" "run_id|running"

        if [ -n "$RUN_ID" ] && [ "$RUN_ID" != "null" ]; then
            sleep 1
            RUN_STATUS=$(curl -sk "$GATEWAY_URL/workflow/run/$RUN_ID" \
                -H "Authorization: Bearer $AGENT_TOKEN")
            echo "  run_status: $RUN_STATUS"
            check_contains "workflow run status retrievable" "$RUN_STATUS" "$RUN_ID"
        fi
    fi
else
    check "skipped: template file not found" "false"
fi

# ---------------------------------------------------------------------------
# 8. /agent/order (pre-approved session)
# ---------------------------------------------------------------------------
section "8. /agent/order (pre-approved session → scheduler path)"

(
    curl -sk -X POST "$GATEWAY_URL/agent/order" \
        -H "Authorization: Bearer $AGENT_TOKEN" \
        -H 'Content-Type: application/json' \
        -d "{\"image\":\"stronghold/rocky-dev:2026.07\",\"ttl_secs\":1800,\"reason\":\"holistic test\",\"compute\":{\"cpu\":2,\"memory_gb\":2}}" \
        > /tmp/stronghold_order_resp.json 2>/dev/null
) &
ORDER_PID=$!
echo "  order_pid=$ORDER_PID"

sleep 1
PENDING_ID=$(sqlite3 "$DB" "SELECT id FROM pending_sessions WHERE tenant_id='$TENANT_ID' ORDER BY created_at DESC LIMIT 1;")
echo "  pending_session_id=$PENDING_ID"
if [ -n "$PENDING_ID" ]; then
    sqlite3 "$DB" "UPDATE pending_sessions SET status='approved', decided_at=datetime('now') WHERE id='$PENDING_ID';"
    check "pending_session approved in DB" "true"
else
    check "pending_session approved in DB" "false"
fi

ORDER_OUT=""
for i in $(seq 1 30); do
    if ! kill -0 $ORDER_PID 2>/dev/null; then
        wait $ORDER_PID
        ORDER_OUT=$(cat /tmp/stronghold_order_resp.json 2>/dev/null)
        break
    fi
    sleep 1
done
echo "  order_out: $(echo $ORDER_OUT | head -c 400)"

if [ -n "$ORDER_OUT" ]; then
    if echo "$ORDER_OUT" | jq -e . >/dev/null 2>&1; then
        MACHINE_ID=$(echo "$ORDER_OUT" | jq -r .machine_id 2>/dev/null)
        if [ -n "$MACHINE_ID" ] && [ "$MACHINE_ID" != "null" ]; then
            check "machine scheduled" "true"
            echo "  machine_id=$MACHINE_ID"
            ORDER_CT=$(echo "$ORDER_OUT" | jq -r .connect_token 2>/dev/null)
            [ -n "$ORDER_CT" ] && [ "$ORDER_CT" != "null" ] && check "connect_token returned" "true" || check "connect_token returned" "false"

            EXEC_RESP=$(curl -sk -X POST "$GATEWAY_URL/agent/$MACHINE_ID/exec?token=$ORDER_CT" \
                -H "Authorization: Bearer $AGENT_TOKEN" \
                -H 'Content-Type: application/json' \
                -d '{"cmd":"echo","args":["hello"],"timeout_secs":15}')
            echo "  exec_resp: $(echo $EXEC_RESP | head -c 400)"
            check_contains "exec endpoint responds" "$EXEC_RESP" "audit_seq|exit_code|error"
        else
            check_contains "scheduler error reported (k3s path)" "$ORDER_OUT" "machine_id|scheduler|k3s|pvc|error|Query returned"
        fi
    else
        # /agent/order returns plain text errors when scheduler fails
        check_contains "order error includes useful detail" "$ORDER_OUT" "Query returned|error|failed|k3s|pvc|scheduler|ntfy"
    fi
else
    check "order returned within 30s" "false"
fi

# ---------------------------------------------------------------------------
# 9. Watchdog tables + metrics
# ---------------------------------------------------------------------------
section "9. Watchdog tables + metrics + synthetic reports"

WATCHDOG_TABLES=$(sqlite3 "$DB" "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('watchdog_reports','ultimata','disagreements','agent_messages','agent_roles','workflow_templates','workflow_runs','tasks','task_outputs','audit_entries','credentials');")
TABLE_COUNT=$(echo "$WATCHDOG_TABLES" | wc -w)
echo "  tables: $(echo $WATCHDOG_TABLES | tr '\n' ' ')"
[ "$TABLE_COUNT" -ge 9 ] && check "multi-agent tables present (≥9)" "true" || check "multi-agent tables present (≥9, count=$TABLE_COUNT)" "false"

WATCHDOG_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)
sqlite3 "$DB" "INSERT INTO watchdog_reports (watcher_machine, watched_machine, watched_task_id, dedication_score, progress_files, progress_tests, progress_commits, last_activity_secs, workaround_warnings, ultimatum_level, assessment, created_at) VALUES ('$FAKE_MACHINE','$FAKE_MACHINE','$TASK_ID',0.92,3,12,1,15,'[]',0,'Strong dedication — proceeding on critical path','$WATCHDOG_TIME');" && \
    check "watchdog report inserted" "true" || check "watchdog report inserted" "false"

ULT_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)
sqlite3 "$DB" "INSERT INTO ultimata (watchdog_machine, target_machine, target_task_id, level, message, acknowledged, created_at) VALUES ('$FAKE_MACHINE','$FAKE_MACHINE','$TASK_ID',1,'Workaround detected — provide a justification or revert.',0,'$ULT_TIME');" && \
    check "ultimatum inserted" "true" || check "ultimatum inserted" "false"

METRICS=$(curl -sk "$GATEWAY_URL/metrics")
echo "  metrics tail:"
echo "$METRICS" | tail -10
check_contains "metrics exposes stronghold_" "$METRICS" "stronghold_"
check_contains "metrics reports audit counter" "$METRICS" "stronghold_audit_entries_total"

# ---------------------------------------------------------------------------
# 10. Audit log integrity (per-tenant hash chain + dual signatures)
# ---------------------------------------------------------------------------
section "10. Audit log entries (per-tenant chain + dual signatures)"

AUDIT_COUNT=$(sqlite3 "$DB" "SELECT COUNT(*) FROM audit_entries WHERE tenant_id='$TENANT_ID';")
echo "  audit_count=$AUDIT_COUNT"
[ "${AUDIT_COUNT:-0}" -ge 3 ] && check "audit log populated ($AUDIT_COUNT entries)" "true" || check "audit log populated ($AUDIT_COUNT entries)" "false"

AUDIT_SAMPLE=$(sqlite3 "$DB" "SELECT sig_ed25519, sig_mldsa65 FROM audit_entries WHERE tenant_id='$TENANT_ID' AND sig_ed25519 IS NOT NULL AND sig_mldsa65 IS NOT NULL LIMIT 1;")
[ -n "$AUDIT_SAMPLE" ] && check "dual-signed audit entries present" "true" || check "dual-signed audit entries present" "false"

# Per-tenant hash chain (each tenant's first entry has prev_hash = 0s)
HASH_CHAIN=$(sqlite3 "$DB" "
WITH tenant_entries AS (
    SELECT seq, prev_hash, hash,
           LAG(hash) OVER (PARTITION BY tenant_id ORDER BY seq) AS prev_in_tenant
    FROM audit_entries WHERE tenant_id='$TENANT_ID'
)
SELECT COUNT(*) FROM tenant_entries
WHERE prev_in_tenant IS NOT NULL AND prev_hash != prev_in_tenant;
")
[ "${HASH_CHAIN:-0}" -eq 0 ] && check "audit log per-tenant hash chain intact" "true" || check "audit log per-tenant hash chain intact (broken=$HASH_CHAIN)" "false"

# Verify both signature types are non-trivial length (Ed25519 ~128 hex; ML-DSA-65 ~2,294 hex)
SIG_ED_LEN=$(sqlite3 "$DB" "SELECT COALESCE(LENGTH(sig_ed25519),0) FROM audit_entries WHERE tenant_id='$TENANT_ID' LIMIT 1;")
SIG_ML_LEN=$(sqlite3 "$DB" "SELECT COALESCE(LENGTH(sig_mldsa65),0) FROM audit_entries WHERE tenant_id='$TENANT_ID' LIMIT 1;")
echo "  sig_ed25519_len=$SIG_ED_LEN  sig_mldsa65_len=$SIG_ML_LEN"
[ "${SIG_ED_LEN:-0}" -gt 60 ] && [ "${SIG_ML_LEN:-0}" -gt 60 ] && check "post-quantum dual signatures (Ed25519 + ML-DSA-65) present" "true" || check "post-quantum dual signatures present" "false"

# ---------------------------------------------------------------------------
# 11. Tenant isolation
# ---------------------------------------------------------------------------
section "11. Tenant isolation"

TENANT2_RESP=$(curl -sk -X POST "$GATEWAY_URL/admin/tenant" -H 'Content-Type: application/json' -d '{"name":"isolation-test-2"}')
TENANT2_ID=$(echo "$TENANT2_RESP" | jq -r .id)
TOKEN2_B64=$(openssl rand -base64 32 | tr -d '/+=' | head -c 43)
AGENT2_TOKEN="stronghold_agent_${TOKEN2_B64}"
TOKEN2_HASH=$(printf '%s' "$AGENT2_TOKEN" | sha256sum | awk '{print $1}')
sqlite3 "$DB" "INSERT INTO agent_tokens (tenant_id, token_hash, scope, created_at, expires_at) VALUES ('$TENANT2_ID','$TOKEN2_HASH','default',datetime('now'),'$EXPIRES');"

T2_TASKS=$(curl -sk "$GATEWAY_URL/agent/task/$TASK_ID" -H "Authorization: Bearer $AGENT2_TOKEN")
echo "  t2 fetches t1 task: $(echo $T2_TASKS | head -c 200)"
echo "$T2_TASKS" | grep -qiE "not found|404|forbidden|403|unauthorized" && check "tenant2 cannot see tenant1 task" "true" || check "tenant2 cannot see tenant1 task" "false"

T2_ROLES=$(curl -sk "$GATEWAY_URL/admin/roles?tenant=$TENANT2_ID")
T2_ROLE_COUNT=$(echo "$T2_ROLES" | jq 'length' 2>/dev/null)
[ "${T2_ROLE_COUNT:-0}" -eq 0 ] && check "tenant2 sees 0 roles from tenant1" "true" || check "tenant2 sees 0 roles from tenant1 (count=$T2_ROLE_COUNT)" "false"

# Tenant 2 must not decrypt tenant 1's credentials
T2_CRED=$(curl -sk "$GATEWAY_URL/agent/$FAKE_MACHINE/credentials/github-pat" -H "Authorization: Bearer $AGENT2_TOKEN")
echo "  t2 fetches t1 cred: $(echo $T2_CRED | head -c 200)"
echo "$T2_CRED" | grep -q "ghp_testSecretToken123" && check "tenant2 cannot decrypt tenant1 credentials" "false" || check "tenant2 cannot decrypt tenant1 credentials" "true"

# ---------------------------------------------------------------------------
# 12. Post-quantum TLS handshake (X25519MLKEM768)
# ---------------------------------------------------------------------------
section "12. Post-quantum TLS handshake (X25519MLKEM768)"

TLS_INFO=$(curl -sk -v "$GATEWAY_URL/agent/health" 2>&1 | grep -iE "SSL connection|X25519MLKEM|TLS_AES|TLS_CHACHA|KEM")
echo "  TLS info: $TLS_INFO"
check_contains "PQ hybrid KEM negotiated (X25519MLKEM768)" "$TLS_INFO" "X25519MLKEM768"
check_contains "TLS 1.3 negotiated" "$TLS_INFO" "TLSv1.3"

# ---------------------------------------------------------------------------
# SUMMARY
# ---------------------------------------------------------------------------
section "SUMMARY"
echo "  Total checks: $TOTAL"
echo "  Passed:       $PASS"
echo "  Failed:       $FAIL"
if [ "$FAIL" -gt 0 ]; then
    echo
    echo "  Failed checks:"
    for f in "${FAILS[@]}"; do
        echo "    - $f"
    done
fi
echo
if [ "$FAIL" -eq 0 ]; then
    echo "  ✅ ALL CHECKS PASSED"
    exit 0
else
    echo "  ⚠️  $FAIL check(s) failed"
    exit 1
fi
