#!/usr/bin/env bash
# Configure k3s containerd to treat localhost:30500 as an insecure registry
# (HTTP + self-signed accepted) so pods can pull from the Stronghold registry.
set -euo pipefail

REGISTRY="localhost:30500"

# k3s containerd config template — appended to /etc/rancher/k3s/registries.yaml
cat > /etc/rancher/k3s/registries.yaml <<EOF
mirrors:
  ${REGISTRY}:
    endpoint:
      - "http://${REGISTRY}"
  "stronghold-registry.stronghold-system.svc.cluster.local:5000":
    endpoint:
      - "http://stronghold-registry.stronghold-system.svc.cluster.local:5000"
  "stronghold/rocky-base":
    endpoint:
      - "http://${REGISTRY}"
  "stronghold/rust-nightly":
    endpoint:
      - "http://${REGISTRY}"
  "stronghold/python-ml":
    endpoint:
      - "http://${REGISTRY}"
  "stronghold/fullstack":
    endpoint:
      - "http://${REGISTRY}"
  "stronghold/node-20":
    endpoint:
      - "http://${REGISTRY}"
  "stronghold/lean-research":
    endpoint:
      - "http://${REGISTRY}"
  "stronghold/go-cli":
    endpoint:
      - "http://${REGISTRY}"
  "stronghold/rust-stable":
    endpoint:
      - "http://${REGISTRY}"

configs:
  "${REGISTRY}":
    tls:
      insecure_skip_verify: true
  "stronghold-registry.stronghold-system.svc.cluster.local:5000":
    tls:
      insecure_skip_verify: true
EOF

echo "Wrote /etc/rancher/k3s/registries.yaml"
cat /etc/rancher/k3s/registries.yaml | head -20

# Restart k3s to pick up the new config
echo "---restarting k3s---"
systemctl restart k3s
sleep 8

# Verify k3s came back
kubectl get nodes 2>&1
echo "---"
kubectl get pods -n stronghold-system 2>&1
