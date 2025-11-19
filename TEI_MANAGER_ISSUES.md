# TEI Manager Issues and Feature Requests

**Date:** 2025-11-19
**Reporter:** Testing with chimera local stack
**TEI Manager Version:** `nazq/tei-manager:latest` (Docker Hub)

---

## Critical Bugs

### 1. Duplicate `--prometheus-port` Argument Crashes TEI Instances

**Severity:** Critical - Blocks all instance startup

**Description:**
TEI Manager passes the `--prometheus-port` argument twice to `text-embeddings-router`, causing immediate process crash on startup.

**Error Message:**
```
error: the argument '--prometheus-port <PROMETHEUS_PORT>' cannot be used multiple times

Usage: text-embeddings-router [OPTIONS]

For more information, try '--help'.
```

**Root Cause:**
TEI Manager auto-assigns prometheus ports (9100, 9101, 9102, etc.) and passes them to TEI instances, but appears to be passing the argument multiple times in the command line.

**Impact:**
- All TEI instances crash immediately on startup
- Health checks fail continuously
- Instances restart in a crash loop
- Local stack tests cannot run

**Evidence:**
```json
{"timestamp":"2025-11-19T14:38:49.303726Z","level":"INFO","fields":{"message":"Instance added to registry","instance":"splade","total_instances":1,"prometheus_port":"Some(9102)"},"target":"tei_manager::registry"}
{"timestamp":"2025-11-19T14:38:49.303912Z","level":"INFO","fields":{"message":"TEI instance started","instance":"splade","model":"naver/splade-cocondenser-ensembledistil","port":8082,"pid":32,"gpu_id":"None"},"target":"tei_manager::instance"}
```
Followed immediately by the prometheus-port error.

**Workaround Attempted:**
Tried setting `extra_args = ["--prometheus-port", "0"]` in config to disable prometheus, but this made the problem worse (now passing it 3 times).

**Recommended Fix:**
- Only pass `--prometheus-port` argument once to TEI processes
- If user specifies it in `extra_args`, don't auto-add it
- Consider making prometheus metrics optional via config flag

---

### 2. GPU Configuration Not Respected

**Severity:** High - Performance degradation

**Description:**
The `gpu_id` field in instance configuration is not parsed or used. All instances start in CPU mode even when GPU is available and configured.

**Configuration:**
```toml
[[instances]]
name = "small"
model_id = "BAAI/bge-small-en-v1.5"
port = 8080
gpu_id = 0  # ← Not respected
```

**Actual Behavior:**
```json
{"message":"TEI instance started","instance":"small","model":"BAAI/bge-small-en-v1.5","port":8080,"pid":33,"gpu_id":"None"}
```
Always shows `"gpu_id":"None"` regardless of config.

**Environment Verification:**
- Docker container has GPU access (verified with `nvidia-smi`)
- NVIDIA RTX 4080 available with 16GB VRAM
- Docker compose configured with GPU resources:
  ```yaml
  deploy:
    resources:
      reservations:
        devices:
          - driver: nvidia
            count: all
            capabilities: [gpu]
  ```

**Impact:**
- TEI instances run 10-50x slower in CPU mode
- Cannot utilize available GPU hardware
- Defeats purpose of GPU-enabled container

**Recommended Fix:**
- Parse `gpu_id` field from config
- Pass appropriate GPU flags to `text-embeddings-router` (e.g., `--cuda-device-id`)
- Respect `gpu_id` in both seed config and API requests

---

## Feature Requests

### 3. Per-Instance Health Endpoint

**Endpoint:** `GET /instances/:name/health`

**Description:**
Add dedicated endpoint to check health status of a specific instance without fetching full instance list.

**Rationale:**
- Current API requires listing all instances to check one
- Health checks from external systems need simple status endpoint
- Aligns with standard health check patterns (e.g., Kubernetes probes)

**Proposed Response:**
```json
{
  "instance": "small",
  "healthy": true,
  "last_check": "2025-11-19T14:38:49Z",
  "consecutive_failures": 0,
  "uptime_seconds": 3600
}
```

**Use Cases:**
- Kubernetes readiness/liveness probes
- Load balancer health checks
- Monitoring systems (Prometheus, Datadog, etc.)
- CI/CD health verification

---

### 4. Per-Instance Info Endpoint

**Endpoint:** `GET /instances/:name/info`

**Description:**
Add endpoint to get detailed information about a specific instance including runtime stats, configuration, and current state.

**Proposed Response:**
```json
{
  "name": "small",
  "model_id": "BAAI/bge-small-en-v1.5",
  "port": 8080,
  "prometheus_port": 9100,
  "gpu_id": 0,
  "status": "running",
  "pid": 33,
  "uptime_seconds": 3600,
  "created_at": "2025-11-19T10:38:49Z",
  "health": {
    "healthy": true,
    "last_check": "2025-11-19T14:38:49Z",
    "consecutive_failures": 0
  },
  "config": {
    "max_batch_tokens": 16384,
    "max_concurrent_requests": 512,
    "pooling": null
  },
  "metrics": {
    "total_requests": 12456,
    "avg_latency_ms": 23.5,
    "error_rate": 0.001
  }
}
```

**Use Cases:**
- Debugging instance issues
- Runtime configuration inspection
- Performance monitoring
- Operational dashboards

---

### 5. All Instances Health Endpoint

**Endpoint:** `GET /health/all`

**Description:**
Add endpoint to get health status of all instances in a single request, providing a system-wide health overview.

**Proposed Response:**
```json
{
  "timestamp": "2025-11-19T14:38:49Z",
  "total_instances": 3,
  "healthy_count": 2,
  "unhealthy_count": 1,
  "overall_status": "degraded",
  "instances": [
    {
      "name": "small",
      "healthy": true,
      "last_check": "2025-11-19T14:38:49Z",
      "consecutive_failures": 0,
      "uptime_seconds": 3600
    },
    {
      "name": "medium",
      "healthy": true,
      "last_check": "2025-11-19T14:38:48Z",
      "consecutive_failures": 0,
      "uptime_seconds": 3598
    },
    {
      "name": "splade",
      "healthy": false,
      "last_check": "2025-11-19T14:38:47Z",
      "consecutive_failures": 5,
      "uptime_seconds": 120,
      "error": "connection refused"
    }
  ]
}
```

**Overall Status Values:**
- `healthy` - All instances healthy
- `degraded` - Some instances unhealthy but system operational
- `critical` - Majority of instances unhealthy
- `down` - All instances unhealthy

**Use Cases:**
- System-wide health dashboards
- Aggregated monitoring/alerting
- Quick status checks without per-instance details
- Load balancer health checks for entire service
- CI/CD verification of full stack

---

## Verification Steps

### HF Cache Sharing ✅

**Tested:** Volume mount for Hugging Face model cache
**Result:** Working correctly

**Configuration:**
```yaml
volumes:
  - ${HOME}/.cache/huggingface:/root/.cache/huggingface
```

**Verification:**
```bash
# Host
$ ls ~/.cache/huggingface/hub/ | grep bge-small
models--BAAI--bge-small-en-v1.5

# Container
$ docker exec chimera-tei-manager ls /root/.cache/huggingface/hub/ | grep bge-small
models--BAAI--bge-small-en-v1.5
```

**Conclusion:** Models are NOT being re-downloaded. Cache sharing works as designed.

---

## Environment Details

**Host:**
- OS: Linux 6.17.0-6-generic
- GPU: NVIDIA GeForce RTX 4080 (16GB)
- Driver: 580.105.08
- CUDA: 13.0

**Container:**
- Image: `nazq/tei-manager:latest` (Docker Hub)
- GPU Access: Verified (nvidia-smi works)
- Config File: `/etc/tei-manager/config.toml`
- State File: `/data/tei-manager-state.toml`

**TEI Models Tested:**
- `BAAI/bge-small-en-v1.5` (384d, dense)
- `sentence-transformers/all-mpnet-base-v2` (768d, dense)
- `naver/splade-cocondenser-ensembledistil` (sparse)

---

## Priority

1. **Critical:** Fix duplicate prometheus-port argument (blocking all usage)
2. **High:** Implement GPU configuration support (performance critical)
3. **Medium:** Add `/instances/:name/health` endpoint (operational improvement)
4. **Medium:** Add `/instances/:name/info` endpoint (debugging/monitoring)
5. **Medium:** Add `/health/all` endpoint (system-wide health overview)

---

## Additional Notes

- State file restoration (`auto_restore_on_restart = true`) works correctly
- Manager API is accessible and responds on port 9000
- Instance registry tracking is functional
- Health check monitoring loop is running (just checks are failing due to crash)

The core architecture of TEI Manager is sound - these are implementation bugs rather than design issues.
