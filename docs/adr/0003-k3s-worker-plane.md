# ADR 0003: Use k3s as the worker plane

## Status

Accepted

## Context

Stronghold needs to run containerd pods on multiple Vultr boxes. This requires:
- Pod scheduling (which worker has capacity?)
- Container lifecycle management (start, stop, restart)
- Network policy enforcement (pods can't talk to each other by default)
- Persistent volume management (survive pod restarts)
- Health monitoring and rescheduling

## Decision

Use **k3s** as the worker plane. Stronghold's gateway is the control plane that talks to k3s.

## Alternatives Considered

### Build our own scheduler on raw containerd
- **Pros:** Full control, no Kubernetes complexity
- **Cons:** Months of work to reinvent pod scheduling, networking, storage, health checks. We'd own a scheduler — not a good use of time.

### Use full Kubernetes (k8s)
- **Pros:** Battle-tested, huge ecosystem
- **Cons:** Heavy (multiple control plane components, etcd, API server, scheduler, controller manager). Overkill for a fleet of 1-20 Vultr boxes.

### Use Nomad
- **Pros:** Simpler than k8s, good for single-binary deployment
- **Cons:** HashiCorp license change (BSL), less ecosystem than k8s, networking is less mature

### Use Docker Swarm
- **Pros:** Simplest option
- **Cons:** Effectively abandoned by Docker, small ecosystem, limited networking features

## Consequences

### Positive
- k3s is 60MB, single-binary, runs on a Vultr box in 30 seconds
- Handles pod scheduling, networking (via Flannel/Calico/Cilium), storage, health
- Huge ecosystem (Helm charts, operators, etc.) if we need it later
- We don't reinvent container orchestration
- Stronghold's job stays focused: phone approval, WebAuthn, audit, image DSL, agent protocol

### Negative
- k3s brings YAML, Helm, and a whole ecosystem — adds complexity
- Debugging k3s issues requires Kubernetes knowledge
- The "clean, self-contained" feeling is somewhat lost

### Neutral
- k3s is CNCF-certified Kubernetes, just lighter
- Stronghold becomes a layer *on top of* k3s, not a replacement for it

## Implementation

```bash
# Install k3s on the control plane
curl -sfL https://get.k3s.io | sh -

# Install k3s worker
curl -sfL https://get.k3s.io | K3S_URL=https://<server>:6443 K3S_TOKEN=<token> sh -
```

Stronghold's gateway uses the k3s API (via `kube-rs` or direct HTTP) to:
- Schedule pods when agents ORDER machines
- Kill pods when sessions end
- Query worker capacity for scheduling decisions
- Open containerd exec sessions for PTY proxying
