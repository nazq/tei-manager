# TEI Manager Authentication Architecture

## Goals

1. **Secure by Default**: mTLS for service-to-service communication
2. **Pluggable Architecture**: Easy to add new auth methods via PRs
3. **Development Friendly**: Support dev hosts with self-signed certs
4. **Zero Breaking Changes**: New auth methods are additive

## Architecture Overview

```
┌─────────────────────────────────────────────────────┐
│            TEI Manager (External Cloud)              │
├─────────────────────────────────────────────────────┤
│                                                      │
│  ┌────────────────────────────────────────────┐    │
│  │         Authentication Middleware           │    │
│  │                                              │    │
│  │  ┌──────────────────────────────────────┐  │    │
│  │  │      AuthProvider Trait              │  │    │
│  │  │  - authenticate(&Request)            │  │    │
│  │  │  - supports_http() -> bool           │  │    │
│  │  │  - supports_grpc() -> bool           │  │    │
│  │  └──────────────────────────────────────┘  │    │
│  │                    │                        │    │
│  │       ┌────────────┼────────────┐           │    │
│  │       │            │            │           │    │
│  │  ┌────▼───┐  ┌────▼───┐  ┌────▼────┐      │    │
│  │  │ mTLS   │  │API Key │  │ OAuth2  │      │    │
│  │  │Provider│  │Provider│  │Provider │      │    │
│  │  └────────┘  └────────┘  └─────────┘      │    │
│  │  (built-in)  (via PR)    (via PR)         │    │
│  └────────────────────────────────────────────┘    │
│                                                      │
└─────────────────────────────────────────────────────┘
           ▲                          ▲
           │                          │
    ┌──────┴──────┐          ┌────────┴────────┐
    │   GKE Pod   │          │  Dev Host       │
    │  (prod)     │          │  (local dev)    │
    │             │          │                 │
    │ - Client    │          │ - Self-signed   │
    │   Cert      │          │   Client Cert   │
    │ - CA Trust  │          │ - CA Trust      │
    └─────────────┘          └─────────────────┘
```

## Pluggable Auth Design

### Core Trait: `AuthProvider`

```rust
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// Authenticate a request
    async fn authenticate(&self, request: &AuthRequest) -> Result<AuthResult, AuthError>;

    /// Whether this provider supports HTTP
    fn supports_http(&self) -> bool;

    /// Whether this provider supports gRPC
    fn supports_grpc(&self) -> bool;

    /// Provider name for logging/metrics
    fn name(&self) -> &str;
}

pub struct AuthRequest {
    pub protocol: Protocol,
    pub peer_addr: SocketAddr,
    pub headers: HeaderMap,           // For HTTP
    pub metadata: MetadataMap,        // For gRPC
    pub tls_info: Option<TlsInfo>,    // Client cert info
}

pub struct AuthResult {
    pub authenticated: bool,
    pub principal: Option<String>,    // User/service identity
    pub metadata: HashMap<String, String>,  // Additional context
}

pub enum Protocol {
    Http,
    Grpc,
}
```

### Auth Manager: Orchestration

```rust
pub struct AuthManager {
    providers: Vec<Box<dyn AuthProvider>>,
    config: AuthConfig,
}

impl AuthManager {
    /// Try each provider until one authenticates
    pub async fn authenticate(&self, request: &AuthRequest) -> Result<AuthResult, AuthError> {
        for provider in &self.providers {
            // Skip providers that don't support this protocol
            if !provider.supports_protocol(request.protocol) {
                continue;
            }

            match provider.authenticate(request).await {
                Ok(result) if result.authenticated => {
                    tracing::info!(
                        provider = provider.name(),
                        principal = result.principal,
                        "Authentication successful"
                    );
                    return Ok(result);
                }
                Ok(_) => continue,  // Try next provider
                Err(e) => {
                    tracing::warn!(
                        provider = provider.name(),
                        error = %e,
                        "Authentication failed"
                    );
                    continue;
                }
            }
        }

        Err(AuthError::Unauthenticated)
    }
}
```

## mTLS Implementation

### Configuration

```toml
[auth]
# Auth is required by default
enabled = true

# Multiple providers can be configured (tried in order)
providers = ["mtls"]

[auth.mtls]
enabled = true

# Production: Real CA
ca_cert = "/certs/ca.pem"
server_cert = "/certs/server.pem"
server_key = "/certs/server-key.pem"

# Development: Allow self-signed certs
allow_self_signed = false  # Set to true for dev

# Optional: Verify client cert subject
verify_subject = true
allowed_subjects = [
    "CN=main-service.example.com",  # Production GKE service
    "CN=*.dev.example.com",         # Dev hosts
]

# Optional: Verify client cert SAN
verify_san = true
allowed_sans = [
    "main-service.default.svc.cluster.local",  # GKE service DNS
    "localhost",                                # Dev
]
```

### mTLS Provider Implementation

```rust
pub struct MtlsProvider {
    config: MtlsConfig,
    ca_cert: Certificate,
}

#[async_trait]
impl AuthProvider for MtlsProvider {
    async fn authenticate(&self, request: &AuthRequest) -> Result<AuthResult, AuthError> {
        // Extract client cert from TLS session
        let tls_info = request.tls_info
            .as_ref()
            .ok_or(AuthError::MissingClientCert)?;

        let client_cert = tls_info.peer_certificate
            .as_ref()
            .ok_or(AuthError::MissingClientCert)?;

        // Verify cert is signed by our CA
        self.verify_cert_chain(client_cert)?;

        // Extract identity from cert
        let subject = extract_subject(client_cert)?;
        let san = extract_san(client_cert)?;

        // Verify against allowlist (if configured)
        if self.config.verify_subject {
            self.verify_subject(&subject)?;
        }

        if self.config.verify_san {
            self.verify_san(&san)?;
        }

        Ok(AuthResult {
            authenticated: true,
            principal: Some(subject),
            metadata: hashmap! {
                "auth_method" => "mtls",
                "cert_subject" => subject,
                "cert_san" => san.join(","),
            },
        })
    }

    fn supports_http(&self) -> bool { true }
    fn supports_grpc(&self) -> bool { true }
    fn name(&self) -> &str { "mtls" }
}
```

### HTTP Integration (Axum)

```rust
// Tower layer for HTTP
pub struct AuthLayer {
    auth_manager: Arc<AuthManager>,
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthMiddleware {
            inner,
            auth_manager: self.auth_manager.clone(),
        }
    }
}

pub struct AuthMiddleware<S> {
    inner: S,
    auth_manager: Arc<AuthManager>,
}

impl<S> Service<Request<Body>> for AuthMiddleware<S>
where
    S: Service<Request<Body>, Response = Response> + Send + 'static,
{
    async fn call(&mut self, req: Request<Body>) -> Result<Response, Error> {
        // Extract TLS info from connection
        let tls_info = req.extensions().get::<TlsInfo>().cloned();

        let auth_request = AuthRequest {
            protocol: Protocol::Http,
            peer_addr: req.extensions().get::<SocketAddr>().copied()?,
            headers: req.headers().clone(),
            metadata: MetadataMap::new(),
            tls_info,
        };

        // Authenticate
        match self.auth_manager.authenticate(&auth_request).await {
            Ok(result) if result.authenticated => {
                // Add principal to request extensions
                req.extensions_mut().insert(result);
                self.inner.call(req).await
            }
            Ok(_) | Err(_) => {
                Ok(Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Body::from("Authentication required"))?)
            }
        }
    }
}
```

### gRPC Integration (Tonic)

```rust
// gRPC interceptor
pub struct AuthInterceptor {
    auth_manager: Arc<AuthManager>,
}

impl tonic::service::Interceptor for AuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        // Extract TLS info from connection
        let tls_info = request.extensions().get::<TlsInfo>().cloned();

        let auth_request = AuthRequest {
            protocol: Protocol::Grpc,
            peer_addr: request.extensions().get::<SocketAddr>().copied()
                .ok_or_else(|| Status::internal("Missing peer address"))?,
            headers: HeaderMap::new(),
            metadata: request.metadata().clone(),
            tls_info,
        };

        // Authenticate (async in sync context - use block_on or spawn)
        let result = tokio::task::block_in_place(|| {
            Handle::current().block_on(
                self.auth_manager.authenticate(&auth_request)
            )
        });

        match result {
            Ok(result) if result.authenticated => {
                // Add principal to metadata
                request.extensions_mut().insert(result);
                Ok(request)
            }
            Ok(_) | Err(_) => {
                Err(Status::unauthenticated("Invalid credentials"))
            }
        }
    }
}
```

## Development Setup

### Certificate Generation Script

```bash
#!/bin/bash
# scripts/generate-dev-certs.sh

set -e

CERT_DIR="certs/dev"
mkdir -p "$CERT_DIR"

# Generate CA
openssl req -x509 -newkey rsa:4096 -days 365 -nodes \
    -keyout "$CERT_DIR/ca-key.pem" \
    -out "$CERT_DIR/ca.pem" \
    -subj "/CN=TEI Manager Dev CA"

# Generate Server Cert
openssl req -newkey rsa:4096 -nodes \
    -keyout "$CERT_DIR/server-key.pem" \
    -out "$CERT_DIR/server-req.pem" \
    -subj "/CN=localhost"

openssl x509 -req -in "$CERT_DIR/server-req.pem" -days 365 \
    -CA "$CERT_DIR/ca.pem" \
    -CAkey "$CERT_DIR/ca-key.pem" \
    -CAcreateserial \
    -out "$CERT_DIR/server.pem" \
    -extfile <(printf "subjectAltName=DNS:localhost,IP:127.0.0.1")

# Generate Client Cert (for dev host)
openssl req -newkey rsa:4096 -nodes \
    -keyout "$CERT_DIR/client-key.pem" \
    -out "$CERT_DIR/client-req.pem" \
    -subj "/CN=$(hostname).dev.local"

openssl x509 -req -in "$CERT_DIR/client-req.pem" -days 365 \
    -CA "$CERT_DIR/ca.pem" \
    -CAkey "$CERT_DIR/ca-key.pem" \
    -CAcreateserial \
    -out "$CERT_DIR/client.pem"

echo "✓ Development certificates generated in $CERT_DIR"
echo ""
echo "Server:"
echo "  CA:   $CERT_DIR/ca.pem"
echo "  Cert: $CERT_DIR/server.pem"
echo "  Key:  $CERT_DIR/server-key.pem"
echo ""
echo "Client:"
echo "  Cert: $CERT_DIR/client.pem"
echo "  Key:  $CERT_DIR/client-key.pem"
```

### Development Configuration

```toml
# config/dev.toml
[auth]
enabled = true
providers = ["mtls"]

[auth.mtls]
enabled = true
ca_cert = "certs/dev/ca.pem"
server_cert = "certs/dev/server.pem"
server_key = "certs/dev/server-key.pem"
allow_self_signed = true  # Dev only!
verify_subject = false    # Skip for dev
verify_san = false        # Skip for dev
```

## Production Setup (GKE → External Cloud)

### GKE Service Account & Cert-Manager

```yaml
# k8s/cert-manager-issuer.yaml
apiVersion: cert-manager.io/v1
kind: Issuer
metadata:
  name: tei-manager-issuer
  namespace: default
spec:
  ca:
    secretName: tei-manager-ca-secret

---
# Generate client cert for GKE service
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: main-service-tei-client
  namespace: default
spec:
  secretName: main-service-tei-client-cert
  issuerRef:
    name: tei-manager-issuer
  commonName: main-service.example.com
  dnsNames:
    - main-service.default.svc.cluster.local
  usages:
    - digital signature
    - key encipherment
    - client auth
```

### TEI Manager Configuration (Production)

```toml
# Production config
[auth]
enabled = true
providers = ["mtls"]

[auth.mtls]
enabled = true
ca_cert = "/certs/ca.pem"           # Same CA as GKE cert-manager
server_cert = "/certs/server.pem"
server_key = "/certs/server-key.pem"
allow_self_signed = false           # Production!
verify_subject = true
allowed_subjects = [
    "CN=main-service.example.com",
]
verify_san = true
allowed_sans = [
    "main-service.default.svc.cluster.local",
]
```

## Adding New Auth Providers (For Community PRs)

### Example: API Key Provider

```rust
// Someone could PR this
pub struct ApiKeyProvider {
    keys: HashSet<String>,
}

#[async_trait]
impl AuthProvider for ApiKeyProvider {
    async fn authenticate(&self, request: &AuthRequest) -> Result<AuthResult, AuthError> {
        // Extract API key from header or metadata
        let key = match request.protocol {
            Protocol::Http => {
                request.headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
            }
            Protocol::Grpc => {
                request.metadata
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
            }
        }.ok_or(AuthError::MissingCredentials)?;

        // Verify key
        if self.keys.contains(key) {
            Ok(AuthResult {
                authenticated: true,
                principal: Some(format!("api-key:{}", &key[..8])),
                metadata: hashmap! { "auth_method" => "api_key" },
            })
        } else {
            Err(AuthError::InvalidCredentials)
        }
    }

    fn supports_http(&self) -> bool { true }
    fn supports_grpc(&self) -> bool { true }
    fn name(&self) -> &str { "api_key" }
}
```

### Configuration for Multiple Providers

```toml
[auth]
enabled = true
# Try mTLS first, fallback to API key
providers = ["mtls", "api_key"]

[auth.mtls]
enabled = true
# ... mtls config ...

[auth.api_key]
enabled = true
keys = [
    "sha256:abc123...",  # Hashed keys
]
```

## File Structure

```
src/
├── auth/
│   ├── mod.rs              # Public API
│   ├── manager.rs          # AuthManager
│   ├── trait.rs            # AuthProvider trait
│   ├── middleware/
│   │   ├── mod.rs
│   │   ├── http.rs         # Axum middleware
│   │   └── grpc.rs         # Tonic interceptor
│   ├── providers/
│   │   ├── mod.rs
│   │   ├── mtls.rs         # mTLS provider (built-in)
│   │   └── noop.rs         # No-op for disabled auth
│   └── error.rs
├── config.rs               # Add auth config
└── main.rs                 # Wire up auth

scripts/
└── generate-dev-certs.sh   # Dev cert generation

docs/
└── AUTH_SETUP.md           # Setup guide
```

## Implementation Phases

### Phase 1: Foundation (Days 1-2)
- [ ] Define `AuthProvider` trait
- [ ] Implement `AuthManager`
- [ ] Add auth config schema
- [ ] Implement `NoopProvider` for disabled auth
- [ ] Add tests for auth framework

### Phase 2: mTLS Provider (Days 3-4)
- [ ] Implement `MtlsProvider`
- [ ] Certificate validation logic
- [ ] Subject/SAN verification
- [ ] Add tests for mTLS

### Phase 3: Integration (Days 5-6)
- [ ] HTTP middleware (Axum)
- [ ] gRPC interceptor (Tonic)
- [ ] TLS server configuration
- [ ] Wire into main.rs

### Phase 4: Tooling & Docs (Day 7)
- [ ] Dev cert generation script
- [ ] Example configs (dev, prod)
- [ ] Setup documentation
- [ ] Client examples (HTTP, gRPC)

## Testing Strategy

### Unit Tests
- Certificate validation
- Subject/SAN matching
- Provider selection logic

### Integration Tests
- HTTP with valid cert → 200
- HTTP without cert → 401
- gRPC with valid cert → Success
- gRPC without cert → Unauthenticated

### E2E Tests
- Full mTLS handshake
- Dev cert workflow
- Multi-provider fallback

## Security Considerations

### Production
- ✅ mTLS required by default
- ✅ Verify cert chain against CA
- ✅ Subject/SAN allowlist
- ✅ No self-signed certs
- ✅ Audit logging

### Development
- ⚠️ Self-signed certs allowed
- ⚠️ Skip subject/SAN verification
- ⚠️ Clear warnings in logs

## Next Steps

1. Review this architecture
2. Confirm GKE setup details (cert-manager, service mesh?)
3. Implement Phase 1 (auth framework)
4. Implement Phase 2 (mTLS provider)
5. Test with dev certs
6. Deploy to production with real certs
