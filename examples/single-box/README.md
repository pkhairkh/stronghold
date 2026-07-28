# Single-Box Deployment Example

All Stronghold components on one Vultr box. Suitable for 1-3 concurrent agents.

## Requirements

- Vultr High Frequency plan with AMD SEV-SNP
- 8+ vCPU, 16+ GB RAM, 200+ GB NVMe
- Rocky Linux 9

## Steps

```bash
# 1. Provision a Vultr box with SEV-SNP
#    Plan: HF-8C-32GB
#    Region: Any that supports SEV-SNP
#    OS: Rocky Linux 9

# 2. SSH in and bootstrap
curl -sL https://github.com/pkhairkh/stronghold/releases/latest/download/bootstrap.sh | bash

# 3. Verify SEV-SNP
ls -la /dev/sev

# 4. Enroll your phone
# Open the printed URL in your phone browser
# Verify the SEV-SNP measurement
# Complete Face ID enrollment

# 5. Mint an agent token
stronghold agent-token mint --tenant default --ttl 86400
# → AGENT_TOKEN=stronghold_agent_...

# 6. Test with an agent
curl -X POST https://your-gateway:8443/agent/order \
  -H "Authorization: Bearer $AGENT_TOKEN" \
  -d '{
    "image": "stronghold/rust-nightly:2026.07",
    "ttl_secs": 3600,
    "reason": "test session"
  }'
```

## Architecture

```
┌─────────────────────────────────────┐
│  Vultr Box (SEV-SNP)               │
│                                     │
│  ┌─────────────┐  ┌─────────────┐   │
│  │ stronghold  │  │ ntfy        │   │
│  │ gateway     │  │ (port 8090) │   │
│  │ (port 8443) │  └─────────────┘   │
│  └──────┬──────┘                     │
│         │                            │
│  ┌──────▼──────┐                     │
│  │ k3s (single)│                     │
│  └──────┬──────┘                     │
│         │                            │
│  ┌──────▼──────┐                     │
│  │ containerd  │                     │
│  │ pods        │                     │
│  └─────────────┘                     │
└─────────────────────────────────────┘
```

## Limitations

- Max ~3 concurrent agents (bounded by 16GB RAM, 4GB per agent)
- No redundancy — box failure loses all sessions
- Single point of failure
