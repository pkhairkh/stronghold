#!/usr/bin/env bash
# Stronghold monitoring setup
#
# Per W10-T9 DoD:
#   - Installs Prometheus node_exporter
#   - Configures a scrape target on the gateway (/metrics)
#   - Drops a Grafana dashboard JSON and alert rules
#
# Idempotent: safe to re-run.
#
# Usage:
#   bash setup/monitoring.sh                          # node_exporter only
#   bash setup/monitoring.sh --with-prometheus        # also install prometheus
#   bash setup/monitoring.sh --with-grafana           # also install grafana
#   bash setup/monitoring.sh --all                    # everything
#   bash setup/monitoring.sh --status                 # show monitoring state

set -euo pipefail

WITH_PROMETHEUS=false
WITH_GRAFANA=false
STATUS_ONLY=false
PROM_VERSION="${PROMETHEUS_VERSION:-2.55.1}"
NODE_EXPORTER_VERSION="${NODE_EXPORTER_VERSION:-1.8.2}"
GRAFANA_RPM_REPO="${GRAFANA_REPO:-https://rpm.grafana.com}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --with-prometheus) WITH_PROMETHEUS=true ;;
        --with-grafana)    WITH_GRAFANA=true ;;
        --all)             WITH_PROMETHEUS=true; WITH_GRAFANA=true ;;
        --status)          STATUS_ONLY=true ;;
        --help|-h)
            cat <<EOF
Usage: monitoring.sh [OPTIONS]

Options:
  --with-prometheus   Also install Prometheus server
  --with-grafana      Also install Grafana
  --all               Install node_exporter + Prometheus + Grafana
  --status            Show monitoring state and exit
  -h, --help          Show this help

Environment:
  PROMETHEUS_VERSION      (default: 2.55.1)
  NODE_EXPORTER_VERSION   (default: 1.8.2)
EOF
            exit 0 ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
    shift
done

if [[ $EUID -ne 0 ]]; then
    echo "ERROR: Run as root" >&2
    exit 2
fi

if [[ -t 1 ]]; then
    C_INFO='\033[1;34m'; C_OK='\033[1;32m'; C_WARN='\033[1;33m'; C_RST='\033[0m'
else
    C_INFO=''; C_OK=''; C_WARN=''; C_RST=''
fi
log()  { echo -e "${C_INFO}[*]${C_RST} $*"; }
ok()   { echo -e "${C_OK}[+]${C_RST} $*"; }
warn() { echo -e "${C_WARN}[!]${C_RST} $*" >&2; }

# --- Status mode ---
if [[ "$STATUS_ONLY" == "true" ]]; then
    echo "Monitoring status:"
    for svc in node_exporter prometheus grafana-server; do
        state="$(systemctl is-active "$svc" 2>/dev/null || echo not-installed)"
        echo "  $svc: $state"
    done
    exit 0
fi

echo "=========================================="
echo "  Stronghold Monitoring Setup"
echo "=========================================="
echo "  node_exporter: yes"
echo "  prometheus:    $WITH_PROMETHEUS"
echo "  grafana:       $WITH_GRAFANA"
echo ""

# --- Dependencies ---
log "Installing dependencies..."
dnf install -y -q tar wget curl firewalld 2>/dev/null || true
echo ""

# ----------------------------------------------------------------------------
# 1. Prometheus node_exporter
# ----------------------------------------------------------------------------
if ! command -v node_exporter &>/dev/null && [[ ! -x /usr/local/bin/node_exporter ]]; then
    log "Installing node_exporter v${NODE_EXPORTER_VERSION}..."
    NE_URL="https://github.com/prometheus/node_exporter/releases/download/v${NODE_EXPORTER_VERSION}/node_exporter-${NODE_EXPORTER_VERSION}.linux-amd64.tar.gz"
    curl -fsSL "$NE_URL" -o /tmp/node_exporter.tar.gz
    tar -xzf /tmp/node_exporter.tar.gz -C /tmp/
    install -m 0755 "/tmp/node_exporter-${NODE_EXPORTER_VERSION}.linux-amd64/node_exporter" /usr/local/bin/node_exporter
    rm -rf /tmp/node_exporter*

    # Create system user
    if ! id -u node_exporter &>/dev/null; then
        useradd --system --no-create-home --shell /sbin/nologin node_exporter
    fi
    ok "node_exporter binary installed"
else
    ok "node_exporter already installed"
fi

# systemd unit for node_exporter
cat > /etc/systemd/system/node_exporter.service <<EOF
[Unit]
Description=Prometheus node_exporter
Documentation=https://github.com/prometheus/node_exporter
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=node_exporter
Group=node_exporter
ExecStart=/usr/local/bin/node_exporter \\
    --web.listen-address=:9100 \\
    --collector.filesystem.mount-points-exclude=^/(dev|proc|sys|var/lib/docker/.+)($|/) \\
    --collector.textfile.directory=/var/lib/node_exporter/textfile
Restart=always
RestartSec=5
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=true
LockPersonality=true
MemoryDenyWriteExecute=true
RestrictRealtime=true
RestrictSUIDSGID=true
ReadWritePaths=/var/lib/node_exporter

[Install]
WantedBy=multi-user.target
EOF

install -d -m 0755 -o node_exporter -g node_exporter /var/lib/node_exporter/textfile

systemctl daemon-reload
systemctl enable --now node_exporter
sleep 1
if systemctl is-active --quiet node_exporter; then
    ok "node_exporter: active on :9100"
else
    warn "node_exporter not active yet; check: journalctl -u node_exporter"
fi
echo ""

# ----------------------------------------------------------------------------
# 2. Prometheus server (optional)
# ----------------------------------------------------------------------------
if [[ "$WITH_PROMETHEUS" == "true" ]]; then
    log "Installing Prometheus v${PROM_VERSION}..."
    if ! id -u prometheus &>/dev/null; then
        useradd --system --no-create-home --shell /sbin/nologin prometheus
    fi
    install -d -m 0755 -o prometheus -g prometheus /etc/prometheus
    install -d -m 0755 -o prometheus -g prometheus /var/lib/prometheus

    if [[ ! -x /usr/local/bin/prometheus ]]; then
        PROM_URL="https://github.com/prometheus/prometheus/releases/download/v${PROM_VERSION}/prometheus-${PROM_VERSION}.linux-amd64.tar.gz"
        curl -fsSL "$PROM_URL" -o /tmp/prometheus.tar.gz
        tar -xzf /tmp/prometheus.tar.gz -C /tmp/
        install -m 0755 "/tmp/prometheus-${PROM_VERSION}.linux-amd64/prometheus"      /usr/local/bin/prometheus
        install -m 0755 "/tmp/prometheus-${PROM_VERSION}.linux-amd64/promtool"        /usr/local/bin/promtool
        install -m 0644 "/tmp/prometheus-${PROM_VERSION}.linux-amd64/consoles/"*      /etc/prometheus/ 2>/dev/null || true
        install -m 0644 "/tmp/prometheus-${PROM_VERSION}.linux-amd64/console_libraries/"* /etc/prometheus/ 2>/dev/null || true
        rm -rf /tmp/prometheus*
    fi
    ok "prometheus binary installed"

    # Generate scrape config
    cat > /etc/prometheus/prometheus.yml <<EOF
# Prometheus configuration — Stronghold
# Scrapes node_exporter on all boxes + the Stronghold gateway /metrics endpoint.

global:
  scrape_interval:     15s
  evaluation_interval: 15s
  external_labels:
    monitor: 'stronghold'

rule_files:
  - /etc/prometheus/rules.d/*.yml

scrape_configs:
  - job_name: 'node_exporter'
    static_configs:
      - targets:
          - 'localhost:9100'
        labels:
          host: 'control-plane'

  - job_name: 'stronghold-gateway'
    scheme: https
    tls_config:
      insecure_skip_verify: true   # self-signed dev cert; replace with CA-signed for prod
    metrics_path: /metrics
    static_configs:
      - targets:
          - 'localhost:8443'
        labels:
          service: 'stronghold-gateway'

  # Add worker nodes here as they are provisioned:
  # - job_name: 'workers'
  #   static_configs:
  #     - targets: ['worker1.ts.net:9100', 'worker2.ts.net:9100']
EOF

    # Alert rules
    install -d -m 0755 -o prometheus -g prometheus /etc/prometheus/rules.d
    cat > /etc/prometheus/rules.d/stronghold-alerts.yml <<EOF
groups:
  - name: stronghold
    rules:
      - alert: StrongholdGatewayDown
        expr: up{job="stronghold-gateway"} == 0
        for: 1m
        labels:
          severity: critical
        annotations:
          summary: "Stronghold gateway is down"
          description: "{{ \$labels.instance }} has been down for more than 1 minute."

      - alert: NodeExporterDown
        expr: up{job="node_exporter"} == 0
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "node_exporter down on {{ \$labels.host }}"

      - alert: HighCPU
        expr: 100 - (avg by (instance) (rate(node_cpu_seconds_total{mode="idle"}[5m])) * 100) > 80
        for: 10m
        labels:
          severity: warning
        annotations:
          summary: "High CPU on {{ \$labels.instance }}"

      - alert: DiskSpaceLow
        expr: 100 - (node_filesystem_avail_bytes / node_filesystem_size_bytes * 100) > 85
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Low disk space on {{ \$labels.instance }} {{ \$labels.mountpoint }}"

      - alert: StrongholdDBSize
        expr: stronghold_sqlite_db_size_bytes > 1073741824
        for: 30m
        labels:
          severity: info
        annotations:
          summary: "Stronghold SQLite DB > 1GB — consider pruning audit logs"
EOF

    # systemd unit
    cat > /etc/systemd/system/prometheus.service <<EOF
[Unit]
Description=Prometheus monitoring
Documentation=https://prometheus.io/docs/
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=prometheus
Group=prometheus
ExecStart=/usr/local/bin/prometheus \\
    --config.file=/etc/prometheus/prometheus.yml \\
    --storage.tsdb.path=/var/lib/prometheus \\
    --storage.tsdb.retention.time=30d \\
    --web.console.templates=/etc/prometheus/consoles \\
    --web.console.libraries=/etc/prometheus/console_libraries \\
    --web.listen-address=127.0.0.1:9090
Restart=always
RestartSec=5
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=true
LockPersonality=true
MemoryDenyWriteExecute=true
RestrictRealtime=true
RestrictSUIDSGID=true
ReadWritePaths=/var/lib/prometheus

[Install]
WantedBy=multi-user.target
EOF

    systemctl daemon-reload
    systemctl enable --now prometheus
    sleep 1
    if systemctl is-active --quiet prometheus; then
        ok "prometheus: active on http://127.0.0.1:9090"
    else
        warn "prometheus not active; check: journalctl -u prometheus"
    fi
    echo ""
fi

# ----------------------------------------------------------------------------
# 3. Grafana (optional)
# ----------------------------------------------------------------------------
if [[ "$WITH_GRAFANA" == "true" ]]; then
    log "Installing Grafana..."
    if ! rpm -q grafana &>/dev/null; then
        cat > /etc/yum.repos.d/grafana.repo <<EOF
[grafana]
name=grafana
baseurl=${GRAFANA_RPM_REPO}
repo_gpgcheck=1
enabled=1
gpgcheck=1
gpgkey=https://rpm.grafana.com/gpg.key
sslverify=1
sslcacert=/etc/pki/tls/certs/ca-bundle.crt
EOF
        dnf install -y -q grafana 2>/dev/null || warn "Grafana install failed"
    fi
    if rpm -q grafana &>/dev/null; then
        systemctl enable --now grafana-server 2>/dev/null || true
        ok "grafana: $(systemctl is-active grafana-server 2>/dev/null || echo unknown)"
    fi
    echo ""
fi

# ----------------------------------------------------------------------------
# 4. Dashboard JSON
# ----------------------------------------------------------------------------
log "Installing Grafana dashboard JSON..."
install -d -m 0755 /usr/share/stronghold/monitoring
cat > /usr/share/stronghold/monitoring/stronghold-dashboard.json <<'EOF'
{
  "annotations": { "list": [] },
  "title": "Stronghold Control Plane",
  "uid": "stronghold-cp",
  "version": 1,
  "schemaVersion": 39,
  "tags": ["stronghold"],
  "time": { "from": "now-6h", "to": "now" },
  "panels": [
    {
      "type": "stat", "title": "Gateway up",
      "gridPos": { "h": 4, "w": 6, "x": 0, "y": 0 },
      "targets": [{ "expr": "up{job=\"stronghold-gateway\"}", "refId": "A" }],
      "fieldConfig": { "defaults": { "mappings": [
        { "options": { "0": { "text": "DOWN", "color": "red" }, "1": { "text": "UP", "color": "green" } }, "type": "value" }
      ]}}
    },
    {
      "type": "stat", "title": "Active sessions",
      "gridPos": { "h": 4, "w": 6, "x": 6, "y": 0 },
      "targets": [{ "expr": "stronghold_sessions_active", "refId": "A" }]
    },
    {
      "type": "stat", "title": "Pending approvals",
      "gridPos": { "h": 4, "w": 6, "x": 12, "y": 0 },
      "targets": [{ "expr": "stronghold_approvals_pending", "refId": "A" }]
    },
    {
      "type": "stat", "title": "Audit log entries (total)",
      "gridPos": { "h": 4, "w": 6, "x": 18, "y": 0 },
      "targets": [{ "expr": "stronghold_audit_entries_total", "refId": "A" }]
    },
    {
      "type": "graph", "title": "Gateway CPU",
      "gridPos": { "h": 8, "w": 12, "x": 0, "y": 4 },
      "targets": [{ "expr": "rate(process_cpu_seconds_total{job=\"stronghold-gateway\"}[5m])", "refId": "A" }]
    },
    {
      "type": "graph", "title": "Gateway memory (RSS)",
      "gridPos": { "h": 8, "w": 12, "x": 12, "y": 4 },
      "targets": [{ "expr": "process_resident_memory_bytes{job=\"stronghold-gateway\"}", "refId": "A" }]
    },
    {
      "type": "graph", "title": "System CPU (all cores)",
      "gridPos": { "h": 8, "w": 12, "x": 0, "y": 12 },
      "targets": [{ "expr": "100 - (avg by (instance) (rate(node_cpu_seconds_total{mode=\"idle\"}[5m])) * 100)", "refId": "A" }]
    },
    {
      "type": "graph", "title": "Disk usage",
      "gridPos": { "h": 8, "w": 12, "x": 12, "y": 12 },
      "targets": [{ "expr": "100 - (node_filesystem_avail_bytes / node_filesystem_size_bytes * 100)", "refId": "A" }]
    }
  ]
}
EOF
ok "Dashboard: /usr/share/stronghold/monitoring/stronghold-dashboard.json"

# Copy alert rules out for reference
cp -f /etc/prometheus/rules.d/stronghold-alerts.yml /usr/share/stronghold/monitoring/ 2>/dev/null || true
ok "Alert rules: /usr/share/stronghold/monitoring/stronghold-alerts.yml"
echo ""

# ----------------------------------------------------------------------------
# 5. Firewall
# ----------------------------------------------------------------------------
if command -v firewall-cmd &>/dev/null && systemctl is-active --quiet firewalld; then
    # 9100 node_exporter: scrape from Prometheus. If Prometheus is local,
    # bind to 127.0.0.1. If remote (Tailscale), open on trusted zone.
    TS_IFACE="$(ip -o link show 2>/dev/null | awk -F': ' '{print $2}' | grep -E '^tailscale[0-9]*$' | head -1 || true)"
    if [[ -n "$TS_IFACE" ]]; then
        firewall-cmd --permanent --zone=trusted --add-port=9100/tcp 2>/dev/null || true
        firewall-cmd --permanent --zone=trusted --add-port=9090/tcp 2>/dev/null || true
        ok "Monitoring ports open on Tailscale (9100, 9090)"
    else
        # Local-only: don't open publicly
        warn "node_exporter bound to :9100 — restrict via firewall if needed"
    fi
    firewall-cmd --reload 2>/dev/null || true
fi
echo ""

echo "=========================================="
echo "  Monitoring Setup Complete"
echo "=========================================="
echo ""
echo "  node_exporter: http://$(hostname -I 2>/dev/null | awk '{print $1}'):9100/metrics"
if [[ "$WITH_PROMETHEUS" == "true" ]]; then
echo "  prometheus:    http://localhost:9090"
fi
if [[ "$WITH_GRAFANA" == "true" ]]; then
echo "  grafana:       http://localhost:3000  (admin/admin on first login)"
fi
echo ""
echo "  Dashboard JSON: /usr/share/stronghold/monitoring/stronghold-dashboard.json"
echo "  Alert rules:    /usr/share/stronghold/monitoring/stronghold-alerts.yml"
echo ""
echo "Import the dashboard into Grafana:"
echo "  1. Open Grafana → Dashboards → Import"
echo "  2. Upload /usr/share/stronghold/monitoring/stronghold-dashboard.json"
echo "  3. Select the Prometheus data source"
echo ""
echo "The gateway exposes metrics at https://<host>:8443/metrics (Prometheus format)."
echo "Custom Stronghold metrics: stronghold_sessions_active, stronghold_approvals_pending,"
echo "stronghold_audit_entries_total, stronghold_sqlite_db_size_bytes."
