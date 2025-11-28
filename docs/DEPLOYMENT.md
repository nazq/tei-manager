# Deployment Guide

Production deployment patterns for TEI Manager.

## Docker Compose

Basic single-host deployment:

```yaml
version: '3.8'
services:
  tei-manager:
    image: ghcr.io/nazq/tei-manager:0.6.0-tei-1.8.3
    ports:
      - "9000:9000"   # REST API
      - "9001:9001"   # gRPC Multiplexer
      - "8080-8089:8080-8089"  # TEI instance ports
    volumes:
      - tei-data:/data
      - ./config:/etc/tei-manager/config:ro
    deploy:
      resources:
        reservations:
          devices:
            - driver: nvidia
              count: all
              capabilities: [gpu]
    restart: unless-stopped

volumes:
  tei-data:
```

## Kubernetes

### Basic Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: tei-manager
spec:
  replicas: 1  # Single instance per node
  selector:
    matchLabels:
      app: tei-manager
  template:
    metadata:
      labels:
        app: tei-manager
    spec:
      containers:
      - name: tei-manager
        image: ghcr.io/nazq/tei-manager:0.6.0-tei-1.8.3
        ports:
        - containerPort: 9000
          name: http
        - containerPort: 9001
          name: grpc
        resources:
          limits:
            nvidia.com/gpu: 2  # Request GPUs
        volumeMounts:
        - name: data
          mountPath: /data
        - name: config
          mountPath: /etc/tei-manager/config
        env:
        - name: TEI_MANAGER_STATE_FILE
          value: /data/state.toml
        livenessProbe:
          httpGet:
            path: /health
            port: 9000
          initialDelaySeconds: 10
          periodSeconds: 30
        readinessProbe:
          httpGet:
            path: /health
            port: 9000
          initialDelaySeconds: 5
          periodSeconds: 10
      volumes:
      - name: data
        persistentVolumeClaim:
          claimName: tei-manager-data
      - name: config
        configMap:
          name: tei-manager-config
---
apiVersion: v1
kind: Service
metadata:
  name: tei-manager
spec:
  selector:
    app: tei-manager
  ports:
  - name: http
    port: 9000
    targetPort: 9000
  - name: grpc
    port: 9001
    targetPort: 9001
```

### State Persistence

TEI Manager persists instance configurations to `state.toml`. In Kubernetes:

**Option 1: PersistentVolumeClaim (Recommended)**
```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: tei-manager-data
spec:
  accessModes:
    - ReadWriteOnce
  resources:
    requests:
      storage: 1Gi
```

**Option 2: EmptyDir (Non-persistent)**

For stateless deployments where instances are created via seed config:
```yaml
volumes:
- name: data
  emptyDir: {}
```

Use seed instances in config to recreate state on startup:
```toml
[[instances]]
name = "bge-small"
model_id = "BAAI/bge-small-en-v1.5"
auto_start = true
```

### GPU Scheduling

TEI Manager runs multiple TEI instances on allocated GPUs. Kubernetes sees the pod as using N GPUs, but TEI Manager handles internal GPU assignment.

**Single GPU per pod:**
```yaml
resources:
  limits:
    nvidia.com/gpu: 1
```

**Multiple GPUs per pod:**
```yaml
resources:
  limits:
    nvidia.com/gpu: 4
```

Then assign instances to specific GPUs via `gpu_id`:
```bash
curl -X POST http://tei-manager:9000/instances \
  -d '{"name": "model-0", "model_id": "...", "gpu_id": 0}'
curl -X POST http://tei-manager:9000/instances \
  -d '{"name": "model-1", "model_id": "...", "gpu_id": 1}'
```

### Resource Sizing

| GPU Memory | Recommended Models | Max Instances |
|------------|-------------------|---------------|
| 16GB | bge-small, all-MiniLM | 4-6 |
| 24GB | bge-base, bge-large | 2-3 |
| 40GB+ | bge-large, multilingual | 3-4 |

Memory per instance varies by model:
- `bge-small-en-v1.5`: ~500MB
- `bge-base-en-v1.5`: ~1GB
- `bge-large-en-v1.5`: ~2GB

### Port Allocation

TEI instances need HTTP ports. Options:

**1. Port range (simple):**
```yaml
ports:
- containerPort: 8080
- containerPort: 8081
# ... up to max_instances
```

**2. Auto-allocation (recommended):**

Set `instance_port_start` and `instance_port_end` in config:
```toml
instance_port_start = 8080
instance_port_end = 8180  # 100 ports available
```

Instances get ports from this range automatically.

### Port Exhaustion Limitation

When an instance is deleted, the OS keeps the port in TIME_WAIT state for approximately 60 seconds.
During rapid create/delete cycles, you may exhaust available ports even though no TEI process is
using them.

**Recommended port range sizing:**
| Use Case | Min Port Range |
|----------|----------------|
| Stable workload (max N instances) | N + 10% buffer |
| Moderate churn | 2x max instances |
| High churn (frequent create/delete) | 3x max instances |

Example for high-churn with max 10 instances:
```toml
instance_port_start = 8080
instance_port_end = 8110  # 30 ports = 3x max instances
max_instances = 10
```

**3. ClusterIP only:**

If clients only use the gRPC multiplexer, instance ports don't need external exposure:
```yaml
ports:
- name: grpc
  port: 9001
  targetPort: 9001
# Instance ports stay internal
```

## Security: Authentication Bypass Prevention

When using mTLS authentication behind a reverse proxy (nginx, envoy), you must configure
the `require_cert_headers` option to prevent authentication bypass:

```toml
[auth]
enabled = true
providers = ["mtls"]
# IMPORTANT: Set to true when behind a reverse proxy
require_cert_headers = true
```

### Security Modes

| `require_cert_headers` | Deployment | Behavior |
|------------------------|------------|----------|
| `false` (default) | Native TLS (no proxy) | Requests without cert headers pass through (rustls verified) |
| `true` | Behind reverse proxy | Requests without cert headers are rejected |

### Why This Matters

When `require_cert_headers = false`, requests without `X-SSL-Client-Cert` headers are
assumed to be native TLS connections where rustls already verified the client certificate.

**Risk**: If an attacker bypasses the reverse proxy and connects directly to the API port,
they can send requests without cert headers and bypass authentication entirely.

**Mitigation**:
1. **Preferred**: Set `require_cert_headers = true` when running behind a reverse proxy
2. Ensure the API port is not directly accessible from untrusted networks
3. Use firewall rules to restrict access to the API port

### Example nginx Configuration

```nginx
server {
    listen 443 ssl;
    ssl_client_certificate /etc/nginx/ca.crt;
    ssl_verify_client on;

    location / {
        proxy_pass http://localhost:9000;
        proxy_set_header X-SSL-Client-Cert $ssl_client_escaped_cert;
        proxy_set_header X-SSL-Protocol $ssl_protocol;
        proxy_set_header X-SSL-Cipher $ssl_cipher;
    }
}
```

## Health Checks

TEI Manager exposes `/health` which returns:
- `200 OK` when the manager is running
- Includes status of managed instances

For deeper health checking, query individual instances:
```bash
curl http://tei-manager:9000/instances/bge-small
# Returns {"status": "running", "health_check_failures": 0, ...}
```

## Monitoring

### Prometheus

Scrape metrics from `/metrics`:
```yaml
# prometheus.yml
scrape_configs:
  - job_name: 'tei-manager'
    static_configs:
      - targets: ['tei-manager:9000']
```

Key metrics:
- `tei_manager_instances_count` - Current instance count
- `tei_manager_instances_created_total` - Instance creation counter
- `tei_manager_health_check_failures_total` - Health check failures by instance
- `tei_manager_instance_restarts_total` - Auto-restart counter

### Grafana Dashboard

Import the dashboard from `docs/grafana-dashboard.json` (if available) or create alerts on:
- `rate(tei_manager_health_check_failures_total[5m]) > 0`
- `tei_manager_instances_count < expected_count`

## Troubleshooting

### Instance won't start

Check logs:
```bash
kubectl logs deployment/tei-manager
# Or
curl http://tei-manager:9000/instances/my-instance/logs
```

Common issues:
- Model not found on HuggingFace
- GPU out of memory
- Port conflict

### High latency

1. Check GPU utilization: `nvidia-smi`
2. Reduce `max_concurrent_requests` per instance
3. Use Arrow batch embeddings for throughput

### State not persisting

Verify PVC is mounted and writable:
```bash
kubectl exec deployment/tei-manager -- ls -la /data/
```
