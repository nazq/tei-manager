# RunPod Deployment Guide

Deploy TEI Manager on RunPod's GPU cloud platform for scalable embedding inference.

## Table of Contents
- [Quick Start](#quick-start)
- [RunPod Template](#runpod-template)
- [Environment Configuration](#environment-configuration)
- [Port Configuration](#port-configuration)
- [Connecting to Your Instance](#connecting-to-your-instance)
- [Example Workflows](#example-workflows)
- [Troubleshooting](#troubleshooting)

---

## Quick Start

### 1. Deploy on RunPod

#### Option A: Using Docker Hub Image (Recommended)
```bash
# Use the pre-built image
docker pull nazq/tei-manager:latest
```

#### Option B: Custom Deployment
1. Go to RunPod Console
2. Click "Deploy" → "Custom"
3. Use the following settings:
   - **Container Image**: `nazq/tei-manager:latest`
   - **Container Disk**: 20 GB (minimum)
   - **Volume**: 50 GB (recommended for model caching)
   - **Expose HTTP Ports**: `8000,9000`
   - **Expose TCP Ports**: `8080-8089` (for TEI instances)

### 2. Configure Environment Variables

Set these in RunPod's environment variable section:

```bash
# Required
TEI_MANAGER_API_PORT=8000          # RunPod HTTP port
TEI_MANAGER_STATE_FILE=/workspace/state.toml

# Optional
TEI_MANAGER_HEALTH_CHECK_INTERVAL=30
TEI_BINARY_PATH=/usr/local/bin/text-embeddings-router
RUST_LOG=info
```

### 3. Start the Service

RunPod will automatically start the container. The manager will be available at:
```
https://{POD_ID}-8000.proxy.runpod.net
```

---

## RunPod Template

Save this as a RunPod template for easy deployment:

```json
{
  "name": "TEI Manager",
  "description": "Dynamic multi-instance manager for HuggingFace Text Embeddings Inference",
  "imageName": "nazq/tei-manager:latest",
  "dockerArgs": "",
  "ports": "8000/http,9000/http,8080/tcp,8081/tcp,8082/tcp,8083/tcp,8084/tcp,8085/tcp,8086/tcp,8087/tcp,8088/tcp,8089/tcp",
  "volumeInGb": 50,
  "containerDiskInGb": 20,
  "env": [
    {
      "key": "TEI_MANAGER_API_PORT",
      "value": "8000"
    },
    {
      "key": "TEI_MANAGER_STATE_FILE",
      "value": "/workspace/state.toml"
    },
    {
      "key": "RUST_LOG",
      "value": "info"
    }
  ]
}
```

---

## Environment Configuration

### RunPod-Specific Variables

TEI Manager automatically detects RunPod environment and configures accordingly:

| Variable | Default | Description |
|----------|---------|-------------|
| `RUNPOD_POD_ID` | (auto) | Pod identifier (set by RunPod) |
| `RUNPOD_DC_ID` | (auto) | Data center ID (set by RunPod) |
| `RUNPOD_GPU_COUNT` | (auto) | Number of GPUs available |

### TEI Manager Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `TEI_MANAGER_API_PORT` | `8000` | API port (use 8000 for RunPod HTTP) |
| `TEI_MANAGER_STATE_FILE` | `/workspace/state.toml` | State persistence file |
| `TEI_MANAGER_HEALTH_CHECK_INTERVAL` | `30` | Health check interval (seconds) |
| `TEI_MANAGER_HEALTH_CHECK_INITIAL_DELAY` | `60` | Initial delay before health checks |
| `TEI_BINARY_PATH` | `/usr/local/bin/text-embeddings-router` | TEI binary path |

---

## Port Configuration

### Primary Ports

| Port | Protocol | Purpose | RunPod Exposure |
|------|----------|---------|-----------------|
| 8000 | HTTP | TEI Manager API | `https://{POD_ID}-8000.proxy.runpod.net` |
| 9000 | HTTP | Alternative API port | `https://{POD_ID}-9000.proxy.runpod.net` |

### TEI Instance Ports

Reserve ports for TEI instances (configured when creating instances):

| Port Range | Purpose |
|------------|---------|
| 8080-8089 | TEI embedding instances (up to 10) |
| 9100-9109 | Prometheus metrics per instance |

**Example**: Create instance on port 8080, access at:
```
https://{POD_ID}-8080.proxy.runpod.net/embed
```

---

## Connecting to Your Instance

### 1. Get Your Pod URL

After deployment, RunPod provides a URL like:
```
https://abc123xyz-8000.proxy.runpod.net
```

### 2. Verify Manager is Running

```bash
export POD_URL="https://abc123xyz-8000.proxy.runpod.net"

# Health check
curl $POD_URL/health

# Expected response:
# {"status":"healthy"}
```

### 3. Create a TEI Instance

```bash
# Create instance
curl -X POST $POD_URL/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "bge-small",
    "model_id": "BAAI/bge-small-en-v1.5",
    "port": 8080,
    "gpu_id": 0
  }'
```

### 4. Access TEI Instance

```bash
# Get the instance endpoint
export TEI_URL="https://abc123xyz-8080.proxy.runpod.net"

# Generate embeddings
curl -X POST $TEI_URL/embed \
  -H "Content-Type: application/json" \
  -d '{"inputs": "Hello from RunPod!"}'
```

---

## Example Workflows

### Multi-Model Setup on Single GPU

Deploy multiple models on one GPU:

```bash
export API="https://{POD_ID}-8000.proxy.runpod.net"

# Dense embedding model
curl -X POST $API/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "bge-small",
    "model_id": "BAAI/bge-small-en-v1.5",
    "port": 8080,
    "gpu_id": 0
  }'

# Larger model
curl -X POST $API/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "e5-large",
    "model_id": "intfloat/e5-large-v2",
    "port": 8081,
    "gpu_id": 0
  }'

# Sparse model (SPLADE)
curl -X POST $API/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "splade",
    "model_id": "naver/splade-cocondenser-ensembledistil",
    "port": 8082,
    "gpu_id": 0,
    "pooling": "splade"
  }'
```

### Multi-GPU Setup

Distribute models across GPUs:

```bash
export API="https://{POD_ID}-8000.proxy.runpod.net"

# Model on GPU 0
curl -X POST $API/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "model-gpu0",
    "model_id": "BAAI/bge-large-en-v1.5",
    "port": 8080,
    "gpu_id": 0
  }'

# Model on GPU 1
curl -X POST $API/instances \
  -H "Content-Type: application/json" \
  -d '{
    "name": "model-gpu1",
    "model_id": "BAAI/bge-large-en-v1.5",
    "port": 8081,
    "gpu_id": 1
  }'
```

### Production Configuration with State Persistence

```bash
# Deploy with auto-restore enabled
# Add to environment variables:
# TEI_MANAGER_AUTO_RESTORE=true

# This ensures instances are restored after pod restart
```

---

## Monitoring

### Check Instance Status

```bash
export API="https://{POD_ID}-8000.proxy.runpod.net"

# List all instances
curl $API/instances

# Get specific instance
curl $API/instances/bge-small

# Prometheus metrics
curl $API/metrics
```

### Health Checks

TEI Manager automatically monitors all instances:
- Health checks every 30 seconds (configurable)
- Auto-restart after 3 consecutive failures
- Graceful degradation on errors

---

## Troubleshooting

### Manager Not Accessible

**Problem**: Cannot reach `https://{POD_ID}-8000.proxy.runpod.net`

**Solutions**:
1. Verify port 8000 is exposed in RunPod settings
2. Check `TEI_MANAGER_API_PORT` is set to 8000
3. View logs in RunPod console
4. Restart the pod

### TEI Instance Fails to Start

**Problem**: Instance creation fails or instance status is "failed"

**Solutions**:

```bash
# Check instance logs
curl $API/instances/{name}

# Common issues:
# 1. Port conflict - use different port
# 2. GPU not available - check gpu_id
# 3. Model not found - verify model_id on HuggingFace
# 4. Out of memory - reduce concurrent instances
```

### Model Download Issues

**Problem**: Long startup time or download failures

**Solutions**:
1. Use volume mount to persist HuggingFace cache:
   ```bash
   # In RunPod, mount /workspace to persist cache
   # Models cached at: ~/.cache/huggingface
   ```

2. Pre-download models:
   ```bash
   # SSH into pod and pre-download
   huggingface-cli download BAAI/bge-small-en-v1.5
   ```

### State Persistence Not Working

**Problem**: Instances don't restore after restart

**Solutions**:
1. Ensure `TEI_MANAGER_STATE_FILE` points to `/workspace` (persisted volume)
2. Set `auto_restore_on_restart: true` in config or environment
3. Check file permissions on `/workspace`

---

## Best Practices

### 1. Use Persistent Volume
Always mount RunPod volume to `/workspace` for:
- Model caching (faster restarts)
- State persistence (instance recovery)
- Configuration files

### 2. Port Planning
- Reserve 8000 for TEI Manager API
- Use 8080-8089 for TEI instances (up to 10)
- Document port assignments for team

### 3. GPU Assignment
- Assign specific GPU IDs for predictable performance
- Monitor GPU usage via Prometheus metrics
- Balance models across GPUs based on size/usage

### 4. Health Monitoring
- Keep default health check intervals
- Monitor `/metrics` endpoint
- Set up alerts for failed instances

### 5. Cost Optimization
- Stop unused instances (not delete, to preserve config)
- Use smaller models where possible
- Share GPUs when traffic is low

---

## Advanced Configuration

### Custom Startup Script

Create `/workspace/startup.sh`:

```bash
#!/bin/bash
set -e

# Pre-download common models
echo "Pre-downloading models..."
huggingface-cli download BAAI/bge-small-en-v1.5
huggingface-cli download BAAI/bge-large-en-v1.5

# Start TEI Manager
exec /usr/local/bin/tei-manager \
  --log-format json \
  --log-level info
```

### Config File Deployment

Create `/workspace/tei-manager.toml`:

```toml
api_port = 8000
state_file = "/workspace/state.toml"
auto_restore_on_restart = true
max_instances = 10

health_check_interval_secs = 30
health_check_initial_delay_secs = 60
max_failures_before_restart = 3

[[instances]]
name = "bge-small"
model_id = "BAAI/bge-small-en-v1.5"
port = 8080
gpu_id = 0
```

Then start with:
```bash
/usr/local/bin/tei-manager --config /workspace/tei-manager.toml
```

---

## Support

- **Documentation**: https://github.com/nazq/tei-manager
- **Issues**: https://github.com/nazq/tei-manager/issues
- **RunPod Docs**: https://docs.runpod.io/

---

## Example: Complete Production Setup

```bash
# 1. Deploy pod with template (see RunPod Template section)

# 2. Set environment variables in RunPod:
TEI_MANAGER_API_PORT=8000
TEI_MANAGER_STATE_FILE=/workspace/state.toml
RUST_LOG=info

# 3. Wait for pod to start, then configure instances:
export API="https://{POD_ID}-8000.proxy.runpod.net"

# Create production instances
curl -X POST $API/instances -H "Content-Type: application/json" -d '{
  "name": "bge-small-prod",
  "model_id": "BAAI/bge-small-en-v1.5",
  "port": 8080,
  "gpu_id": 0,
  "max_concurrent_requests": 512,
  "max_batch_tokens": 16384
}'

# 4. Verify everything is running:
curl $API/instances
curl https://{POD_ID}-8080.proxy.runpod.net/health

# 5. Start generating embeddings!
curl -X POST https://{POD_ID}-8080.proxy.runpod.net/embed \
  -H "Content-Type: application/json" \
  -d '{"inputs": ["Your text here", "Another text"]}'
```
