# TEI Manager - Improvement Proposals

**Date:** 2025-11-19
**Version:** 1.8.3
**LOC:** ~1,810 lines (src/)

---

## Executive Summary

TEI Manager is a well-architected service with clean separation of concerns. The codebase is maintainable, tested, and production-ready. This document outlines potential improvements across architecture, features, operations, and code quality.

**Current Strengths:**
- Clean Rust architecture with proper error handling
- Thread-safe registry with Arc/RwLock patterns
- Comprehensive state persistence with atomic writes
- Health monitoring with auto-restart
- Prometheus metrics integration
- Smart port allocation with availability checking

**Priority Areas for Improvement:**
1. **High**: Port validation and conflict detection enhancements
2. **High**: Instance lifecycle event system
3. **Medium**: Advanced health check features
4. **Medium**: Resource limits and quotas
5. **Low**: Performance optimizations

---

## 1. Port Management & Validation

### 1.1 Validate TEI Service Ports

**Current State:**
Registry checks for port conflicts within managed instances, but doesn't verify if the port is actually free on the system.

**Issue:**
```rust
// src/registry.rs:40-47
for instance in instances.values() {
    if instance.config.port == config.port {
        anyhow::bail!("Port {} already in use...", config.port);
    }
}
// ❌ Doesn't check if port is free on system
```

**Proposed Fix:**
```rust
// Check internal conflicts
for instance in instances.values() {
    if instance.config.port == config.port {
        anyhow::bail!("Port {} in use by instance '{}'", config.port, instance.config.name);
    }
}

// Check system availability
if !Self::is_port_free(config.port) {
    anyhow::bail!("Port {} is already bound by another process", config.port);
}
```

**Implementation:**
```rust
/// Check if a port is available on the system
fn is_port_free(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_ok()
}
```

**Benefits:**
- Fail fast with clear error messages
- Prevent instance creation that will crash on startup
- Better user experience

---

### 1.2 Port Range Validation

**Issue:**
No validation for reserved/privileged ports (<1024) or valid port range.

**Current:**
```toml
port = 80  # ❌ Would fail with permission denied
port = 70000  # ❌ Invalid port number
```

**Proposed:**
```rust
pub fn validate(&self) -> Result<()> {
    // Existing validations...

    for instance in &self.instances {
        // Validate port range
        if instance.port < 1024 {
            anyhow::bail!(
                "Instance '{}': Port {} is privileged (requires root). Use port >= 1024",
                instance.name, instance.port
            );
        }

        if instance.port > 65535 {
            anyhow::bail!(
                "Instance '{}': Port {} is invalid. Must be <= 65535",
                instance.name, instance.port
            );
        }
    }

    Ok(())
}
```

---

### 1.3 Auto-Assign TEI Service Ports

**Current:**
Prometheus ports are auto-assigned, but TEI service ports must be manually specified.

**Enhancement:**
```rust
pub struct CreateInstanceRequest {
    pub name: String,
    pub model_id: String,

    #[serde(default)]
    pub port: Option<u16>,  // ← Make optional, auto-assign if None

    // ... other fields
}
```

**Implementation:**
```rust
// In Registry::add()
if config.port.is_none() {
    config.port = Some(Self::find_free_port(8080)?);  // Start from 8080
}
```

**Use Case:**
```bash
# No need to manage ports manually
curl -X POST /instances -d '{
  "name": "model1",
  "model_id": "BAAI/bge-small-en-v1.5"
}'
# Response: { "port": 8080, "prometheus_port": 9100 }

curl -X POST /instances -d '{
  "name": "model2",
  "model_id": "sentence-transformers/all-mpnet-base-v2"
}'
# Response: { "port": 8081, "prometheus_port": 9101 }
```

---

## 2. Instance Lifecycle & Events

### 2.1 Event System for Instance State Changes

**Problem:**
No way to hook into instance lifecycle events (created, started, stopped, failed, restarted).

**Use Cases:**
- Logging instance events to external systems
- Triggering webhooks on failures
- Custom monitoring integrations
- Audit trails

**Proposed Architecture:**
```rust
// src/events.rs
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub enum InstanceEvent {
    Created { name: String, model_id: String },
    Started { name: String, port: u16, pid: u32 },
    Stopped { name: String },
    Failed { name: String, error: String },
    Restarted { name: String, reason: String },
    Deleted { name: String },
}

#[async_trait]
pub trait EventHandler: Send + Sync {
    async fn handle(&self, event: InstanceEvent);
}

pub struct EventBus {
    handlers: Arc<RwLock<Vec<Arc<dyn EventHandler>>>>,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            handlers: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn subscribe(&self, handler: Arc<dyn EventHandler>) {
        self.handlers.write().await.push(handler);
    }

    pub async fn publish(&self, event: InstanceEvent) {
        let handlers = self.handlers.read().await;
        for handler in handlers.iter() {
            handler.handle(event.clone()).await;
        }
    }
}
```

**Built-in Handlers:**
```rust
// Structured logging handler
pub struct LoggingEventHandler;

#[async_trait]
impl EventHandler for LoggingEventHandler {
    async fn handle(&self, event: InstanceEvent) {
        match event {
            InstanceEvent::Started { name, port, pid } => {
                tracing::info!(
                    event = "instance_started",
                    instance = %name,
                    port = port,
                    pid = pid
                );
            }
            InstanceEvent::Failed { name, error } => {
                tracing::error!(
                    event = "instance_failed",
                    instance = %name,
                    error = %error
                );
            }
            // ... other events
        }
    }
}

// Webhook handler
pub struct WebhookEventHandler {
    url: String,
    client: reqwest::Client,
}

#[async_trait]
impl EventHandler for WebhookEventHandler {
    async fn handle(&self, event: InstanceEvent) {
        let _ = self.client
            .post(&self.url)
            .json(&event)
            .send()
            .await;
    }
}
```

**Integration:**
```rust
// In main.rs or Registry
let event_bus = Arc::new(EventBus::new());

// Subscribe built-in handlers
event_bus.subscribe(Arc::new(LoggingEventHandler)).await;

// Optional webhook handler from config
if let Some(webhook_url) = &config.webhook_url {
    event_bus.subscribe(Arc::new(WebhookEventHandler::new(webhook_url))).await;
}

// Emit events
event_bus.publish(InstanceEvent::Started {
    name: instance.config.name.clone(),
    port: instance.config.port,
    pid: pid,
}).await;
```

**Configuration:**
```toml
# config.toml
[events]
webhook_url = "https://hooks.slack.com/services/..."
log_events = true
```

---

### 2.2 Graceful Shutdown with Timeout

**Current:**
Instances are stopped on shutdown, but no timeout handling.

**Enhancement:**
```rust
// src/instance.rs
pub async fn stop_graceful(&self, timeout_secs: u64) -> Result<()> {
    let mut process = self.process.write().await;

    if let Some(mut child) = process.take() {
        // Send SIGTERM
        #[cfg(unix)]
        {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;

            let pid = Pid::from_raw(child.id() as i32);
            kill(pid, Signal::SIGTERM)?;
        }

        // Wait for graceful shutdown with timeout
        let timeout = tokio::time::Duration::from_secs(timeout_secs);

        match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(_)) => {
                tracing::info!(
                    instance = %self.config.name,
                    "Instance stopped gracefully"
                );
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "Failed to wait for process");
            }
            Err(_) => {
                // Timeout - force kill
                tracing::warn!(
                    instance = %self.config.name,
                    timeout_secs = timeout_secs,
                    "Graceful shutdown timeout, force killing"
                );
                child.kill().await?;
            }
        }
    }

    *self.status.write().await = InstanceStatus::Stopped;
    Ok(())
}
```

---

## 3. Advanced Health Checks

### 3.1 Custom Health Check Endpoints

**Current:**
Health checks only hit `/health` endpoint.

**Enhancement:**
```rust
pub struct HealthCheckConfig {
    pub endpoint: String,           // Default: "/health"
    pub method: HttpMethod,          // Default: GET
    pub expected_status: u16,        // Default: 200
    pub timeout_secs: u64,           // Default: 5
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
}
```

**Use Cases:**
- Check `/info` for model metadata validation
- POST to `/embed` with test input to verify functionality
- Custom headers for authenticated health checks

---

### 3.2 Health Check Result Metrics

**Current:**
Only tracks failure count.

**Enhancement:**
```rust
pub struct HealthCheckStats {
    pub total_checks: u64,
    pub successes: u64,
    pub failures: u64,
    pub avg_response_time_ms: f64,
    pub last_success_time: Option<DateTime<Utc>>,
    pub last_failure_time: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
}
```

**Prometheus Metrics:**
```rust
// src/metrics.rs
metrics::histogram!("health_check_duration_seconds")
    .record(duration.as_secs_f64());

metrics::counter!("health_check_total")
    .increment(1);

metrics::counter!("health_check_failures_total")
    .increment(1);
```

---

### 3.3 Startup Probes vs Liveness Probes

**Current:**
Single health check loop for all instances.

**Enhancement:**
```rust
pub enum ProbeType {
    Startup,   // Run only during instance startup
    Liveness,  // Run continuously
    Readiness, // Check if ready to serve traffic
}

pub struct HealthMonitor {
    // Startup probe: fast fail, no retry
    startup_probe: ProbeConfig,

    // Liveness probe: restart on failure
    liveness_probe: ProbeConfig,

    // Readiness probe: remove from load balancer
    readiness_probe: ProbeConfig,
}
```

**Kubernetes-style:**
```toml
[health]
startup_probe_endpoint = "/health"
startup_probe_period_secs = 5
startup_probe_failure_threshold = 6  # 30 seconds total

liveness_probe_endpoint = "/health"
liveness_probe_period_secs = 30
liveness_probe_failure_threshold = 3

readiness_probe_endpoint = "/info"
readiness_probe_period_secs = 10
```

---

## 4. Resource Management

### 4.1 VRAM Usage Tracking

**Problem:**
No visibility into GPU memory usage per instance.

**Proposed:**
```rust
// src/gpu.rs
use nvml_wrapper::Nvml;

pub struct GpuMonitor {
    nvml: Nvml,
}

impl GpuMonitor {
    pub fn get_process_memory(&self, pid: u32) -> Result<u64> {
        // Query NVML for process VRAM usage
        // ...
    }

    pub fn get_gpu_utilization(&self, gpu_id: u32) -> Result<GpuStats> {
        // Get GPU utilization, temperature, etc.
        // ...
    }
}

pub struct GpuStats {
    pub gpu_id: u32,
    pub memory_used_mb: u64,
    pub memory_total_mb: u64,
    pub utilization_percent: f32,
    pub temperature_c: u32,
}
```

**Integration:**
```rust
// Add to InstanceInfo
pub struct InstanceInfo {
    // ... existing fields
    pub gpu_memory_mb: Option<u64>,
    pub gpu_utilization: Option<f32>,
}
```

**API Endpoint:**
```http
GET /instances/:name/gpu-stats
{
  "gpu_id": 0,
  "memory_used_mb": 2048,
  "memory_total_mb": 16384,
  "utilization_percent": 87.5,
  "temperature_c": 72
}
```

---

### 4.2 Instance Resource Limits

**Enhancement:**
```toml
[[instances]]
name = "limited-instance"
model_id = "large-model"
port = 8080

[instances.limits]
max_memory_mb = 4096        # Kill if exceeds 4GB RAM
max_vram_mb = 8192          # Warn if exceeds 8GB VRAM
max_cpu_percent = 200       # 2 cores worth
max_requests_per_sec = 100  # Rate limiting
```

**Implementation:**
```rust
pub struct ResourceLimits {
    pub max_memory_mb: Option<u64>,
    pub max_vram_mb: Option<u64>,
    pub max_cpu_percent: Option<f32>,
    pub max_requests_per_sec: Option<u32>,
}

// Monitor in health check loop
if memory_usage > limits.max_memory_mb {
    tracing::error!("Instance exceeded memory limit, restarting");
    instance.restart().await?;
}
```

---

### 4.3 Request Rate Limiting

**Use Case:**
Prevent single instance from overwhelming GPU.

**Implementation:**
```rust
use tower::limit::RateLimitLayer;

// Per-instance rate limiting
let rate_limit = RateLimitLayer::new(
    100,  // requests
    Duration::from_secs(1),
);

Router::new()
    .route("/embed", post(embed_handler))
    .layer(rate_limit)
```

---

## 5. Operational Improvements

### 5.1 Instance Tagging & Filtering

**Use Case:**
Organize instances by environment, team, or purpose.

**API:**
```rust
pub struct InstanceConfig {
    // ... existing fields

    #[serde(default)]
    pub tags: HashMap<String, String>,
}
```

**Configuration:**
```toml
[[instances]]
name = "prod-embeddings"
tags = { env = "production", team = "ml", purpose = "embeddings" }
```

**API Queries:**
```http
GET /instances?tag=env:production
GET /instances?tag=team:ml
GET /instances?tag=purpose:embeddings
```

---

### 5.2 Bulk Operations

**API Endpoints:**
```http
POST /instances/bulk/start
POST /instances/bulk/stop
POST /instances/bulk/restart
POST /instances/bulk/delete

{
  "instances": ["model1", "model2", "model3"]
}
```

**Response:**
```json
{
  "succeeded": ["model1", "model2"],
  "failed": [
    {
      "instance": "model3",
      "error": "Instance not found"
    }
  ]
}
```

---

### 5.3 Instance Templates

**Use Case:**
Quickly spin up instances from predefined templates.

**Configuration:**
```toml
[templates.small-embeddings]
max_batch_tokens = 8192
max_concurrent_requests = 256
pooling = null

[templates.large-embeddings]
max_batch_tokens = 32768
max_concurrent_requests = 1024
gpu_id = 0

[templates.sparse-embeddings]
pooling = "splade"
max_batch_tokens = 16384
```

**API:**
```http
POST /instances/from-template/small-embeddings
{
  "name": "new-instance",
  "model_id": "BAAI/bge-small-en-v1.5",
  "port": 8080
}
```

---

## 6. Observability & Debugging

### 6.1 Instance Logs Endpoint

**Problem:**
No way to view TEI process logs via API.

**Proposed:**
```http
GET /instances/:name/logs?tail=100&follow=false
```

**Implementation:**
```rust
// Capture stdout/stderr when spawning
let child = cmd
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;

// Stream logs to rotating file
let log_file = format!("/var/log/tei-manager/{}.log", instance.name);
```

---

### 6.2 Structured Metrics Export

**Current:**
Prometheus endpoint at `/metrics`.

**Enhancement:**
Add JSON metrics endpoint for programmatic access.

```http
GET /metrics/json

{
  "instances": {
    "total": 3,
    "running": 2,
    "stopped": 1,
    "failed": 0
  },
  "instance_details": [
    {
      "name": "model1",
      "status": "running",
      "uptime_secs": 3600,
      "restarts": 0,
      "health_check_failures": 0,
      "requests_total": 1234,
      "avg_latency_ms": 23.5
    }
  ],
  "system": {
    "total_memory_mb": 32768,
    "used_memory_mb": 8192,
    "gpu_count": 2,
    "gpu_memory_total_mb": 32768,
    "gpu_memory_used_mb": 16384
  }
}
```

---

### 6.3 Request Tracing

**Enhancement:**
Distributed tracing integration for debugging request flows.

```rust
use opentelemetry::trace::TraceContextExt;
use tracing_opentelemetry::OpenTelemetryLayer;

// Add to instance start command
cmd.env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://jaeger:4317");
```

---

## 7. Performance Optimizations

### 7.1 Async Port Availability Check

**Current:**
Synchronous TcpListener binding blocks registry write lock.

**Issue:**
```rust
// src/registry.rs
let mut instances = self.instances.write().await;  // 🔒 Lock held
let free_port = Self::find_free_port(9100)?;      // ⏱️  Blocking I/O
```

**Fix:**
```rust
// Check port before acquiring lock
let free_port = Self::find_free_port(9100)?;

let mut instances = self.instances.write().await;
// ... add instance with pre-validated port
```

---

### 7.2 Connection Pooling for Health Checks

**Current:**
Creates new HTTP client for each health check.

**Fix:**
```rust
pub struct HealthMonitor {
    client: reqwest::Client,  // ← Reuse across checks
    // ... other fields
}

impl HealthMonitor {
    pub fn new(...) -> Self {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        Self { client, ... }
    }
}
```

---

### 7.3 Batch State Persistence

**Current:**
State saved on every instance change.

**Enhancement:**
```rust
pub struct StateManager {
    dirty_flag: Arc<AtomicBool>,
    auto_save_interval: Duration,
}

// Mark dirty on changes
async fn on_instance_change(&self) {
    self.dirty_flag.store(true, Ordering::Relaxed);
}

// Periodic save loop
async fn auto_save_loop(&self) {
    let mut interval = tokio::time::interval(self.auto_save_interval);

    loop {
        interval.tick().await;

        if self.dirty_flag.swap(false, Ordering::Relaxed) {
            let _ = self.save().await;
        }
    }
}
```

**Benefits:**
- Reduce I/O on high-churn workloads
- Configurable save interval
- Still immediate on shutdown

---

## 8. Security Enhancements

### 8.1 API Authentication

**Current:**
No authentication (relies on network security).

**Proposed:**
```toml
[auth]
enabled = true
api_key_header = "X-API-Key"
api_keys_file = "/etc/tei-manager/api-keys.txt"
```

**Implementation:**
```rust
use tower_http::auth::RequireAuthorizationLayer;

let auth_layer = RequireAuthorizationLayer::bearer(&api_key);

Router::new()
    .route("/instances", post(create_instance))
    .layer(auth_layer)
```

---

### 8.2 TLS Support

**Enhancement:**
```toml
[tls]
enabled = true
cert_file = "/etc/tei-manager/tls/cert.pem"
key_file = "/etc/tei-manager/tls/key.pem"
```

---

### 8.3 Instance Isolation

**Current:**
All instances run as same user.

**Enhancement:**
```rust
// Run each instance with dedicated user
cmd.uid(instance_uid);
cmd.gid(instance_gid);

// Use cgroups for resource isolation
cmd.env("CGROUP_NAME", format!("tei-instance-{}", instance.name));
```

---

## 9. Testing Improvements

### 9.1 Integration Tests

**Missing:**
Integration tests for full request lifecycle.

**Proposed:**
```rust
// tests/integration_test.rs
#[tokio::test]
async fn test_full_lifecycle() {
    let app = spawn_test_app().await;

    // Create instance
    let resp = app.post("/instances")
        .json(&create_req)
        .send()
        .await;

    assert_eq!(resp.status(), 201);

    // Wait for startup
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Check health
    let health = app.get("/instances/test").send().await;
    assert_eq!(health.json().status, "running");

    // Stop instance
    app.post("/instances/test/stop").send().await;

    // Verify stopped
    let status = app.get("/instances/test").send().await;
    assert_eq!(status.json().status, "stopped");
}
```

---

### 9.2 Chaos Testing

**Proposed:**
```rust
// tests/chaos_test.rs
#[tokio::test]
async fn test_instance_crash_recovery() {
    // Start instance
    let instance = create_instance().await;

    // Kill process directly (simulate crash)
    kill_process(instance.pid).await;

    // Wait for health monitor to detect and restart
    tokio::time::sleep(Duration::from_secs(30)).await;

    // Verify auto-restart worked
    assert!(instance.is_running().await);
    assert_eq!(instance.stats.restarts, 1);
}
```

---

### 9.3 Property-Based Testing

**Use Case:**
Test edge cases in port allocation, config validation.

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_port_allocation(ports in prop::collection::vec(1024u16..=65535, 1..100)) {
        // Test that allocating many ports doesn't conflict
        let registry = Registry::new(None);

        for port in ports {
            let config = InstanceConfig { port, .. };
            let result = registry.add(config).await;

            // Should succeed if port is unique
            prop_assert!(result.is_ok() || port_already_used);
        }
    }
}
```

---

## 10. Documentation Improvements

### 10.1 API Documentation (OpenAPI/Swagger)

**Proposed:**
Generate OpenAPI spec from code.

```rust
use utoipa::{OpenApi, ToSchema};

#[derive(OpenApi)]
#[openapi(
    paths(
        create_instance,
        list_instances,
        get_instance,
        delete_instance
    ),
    components(
        schemas(CreateInstanceRequest, InstanceInfo, HealthResponse)
    )
)]
struct ApiDoc;

// Serve at /api-docs
Router::new()
    .route("/api-docs/openapi.json", get(|| async {
        Json(ApiDoc::openapi())
    }))
```

---

### 10.2 Configuration Examples

**Missing:**
Production-ready configuration examples.

**Proposed:**
```
config/
├── tei-manager.example.toml
├── production.toml           # ← Production config
├── development.toml          # ← Dev config
├── kubernetes.toml           # ← K8s deployment
└── docker-compose.toml       # ← Docker Compose
```

---

### 10.3 Architecture Diagrams

**Missing:**
Visual architecture documentation.

**Proposed:**
```markdown
docs/
├── architecture.md
│   ├── Component diagram
│   ├── Sequence diagram (instance lifecycle)
│   └── Deployment diagram
├── development.md
└── operations.md
```

---

## Priority Matrix

| Feature | Priority | Complexity | Impact | Estimated LOC |
|---------|----------|------------|--------|---------------|
| Port validation on system | **High** | Low | High | +20 |
| Duplicate prometheus-port fix | **Critical** | Low | Critical | +10 |
| Instance event system | **High** | Medium | High | +200 |
| VRAM tracking | **High** | Medium | High | +150 |
| Health check enhancements | **Medium** | Medium | Medium | +100 |
| Bulk operations | **Medium** | Low | Medium | +50 |
| Instance templates | **Medium** | Low | Medium | +80 |
| Logs endpoint | **Medium** | Medium | High | +100 |
| API authentication | **Low** | Medium | Medium | +80 |
| Async port checks | **Low** | Low | Low | +15 |

---

## Implementation Roadmap

### Phase 1: Critical Fixes (Week 1)
- ✅ Fix duplicate prometheus-port argument crash
- ✅ Implement smart port allocation with system checks
- ⬜ Validate TEI service ports on system

### Phase 2: Core Features (Weeks 2-3)
- ⬜ Instance event system
- ⬜ VRAM usage tracking
- ⬜ Enhanced health checks (startup/liveness/readiness)
- ⬜ Instance logs endpoint

### Phase 3: Operational (Weeks 4-5)
- ⬜ Bulk operations API
- ⬜ Instance templates
- ⬜ Tagging & filtering
- ⬜ Graceful shutdown improvements

### Phase 4: Observability (Week 6)
- ⬜ JSON metrics endpoint
- ⬜ OpenAPI documentation
- ⬜ Request tracing integration
- ⬜ Structured logging enhancements

### Phase 5: Performance & Security (Week 7+)
- ⬜ Connection pooling
- ⬜ Batch state persistence
- ⬜ API authentication
- ⬜ TLS support

---

## Conclusion

TEI Manager is a solid foundation with clear improvement paths. The highest-value enhancements focus on:

1. **Reliability**: Better port validation and event tracking
2. **Observability**: VRAM monitoring and enhanced health checks
3. **Usability**: Templates, bulk ops, and better logging

The codebase is well-positioned for these enhancements due to its clean architecture and comprehensive test coverage.

**Estimated Total Enhancement Effort:** 8-10 weeks for full implementation
**Quick Wins (Week 1):** Port validation, event logging, logs endpoint
