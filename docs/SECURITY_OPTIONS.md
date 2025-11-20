# TEI Manager Security Options

## Current State
TEI Manager currently has **no authentication or authorization**. Anyone who can reach the HTTP API (port 9000) or gRPC endpoint (port 9001) can:
- Create/delete TEI instances
- Start/stop instances
- Access all instance configurations
- Monitor instance health
- Consume GPU resources

## Security Options

### Option 1: API Key Authentication (Recommended for MVP)
**Simple bearer token authentication for HTTP and metadata-based auth for gRPC**

#### Pros:
- Simple to implement and configure
- Low latency overhead
- Works well in automated systems (CI/CD, scripts)
- Easy to rotate keys
- No external dependencies

#### Cons:
- Keys need secure storage (env vars, secrets manager)
- No built-in expiration (unless implementing JWT-style keys)
- Single key compromise = full access

#### Implementation:
```rust
// HTTP: Check Authorization header
Authorization: Bearer <api-key>

// gRPC: Check metadata
authorization: Bearer <api-key>
```

#### Configuration:
```toml
[security]
enabled = true
api_key = "${TEI_MANAGER_API_KEY}"  # From env var
# Or hashed keys for multiple users:
api_keys = [
  "sha256:abc123...",  # User 1
  "sha256:def456...",  # User 2
]
```

**Effort**: Low (1-2 days)
**Security Level**: Medium

---

### Option 2: mTLS (Mutual TLS)
**Client certificate-based authentication**

#### Pros:
- Very strong authentication
- Certificate-based, no passwords/keys to leak
- Built into TLS layer (works for both HTTP and gRPC)
- Client identity cryptographically verified
- Standard for service-to-service auth

#### Cons:
- Complex certificate management (CA, signing, rotation)
- Harder to set up for development
- Client library support needed
- Certificate distribution complexity

#### Implementation:
```rust
// Both HTTP and gRPC use TLS layer
// Verify client cert against CA
tls_config.client_cert_verifier(...)
```

#### Configuration:
```toml
[security.mtls]
enabled = true
ca_cert = "/certs/ca.pem"
server_cert = "/certs/server.pem"
server_key = "/certs/server-key.pem"
require_client_cert = true
```

**Effort**: Medium (3-4 days)
**Security Level**: High

---

### Option 3: IP Allowlist
**Restrict access to specific IP addresses/CIDR ranges**

#### Pros:
- Very simple to implement
- Zero latency overhead
- Works as additional layer with other auth methods
- Good for known infrastructure

#### Cons:
- Not suitable for dynamic IPs
- NAT/proxy complications
- IP spoofing in some network configs
- Doesn't identify individual users

#### Implementation:
```rust
// Tower middleware to check peer IP
layer(IpAllowlistLayer::new(allowlist))
```

#### Configuration:
```toml
[security.ip_allowlist]
enabled = true
allowed_ips = [
  "10.0.0.0/8",      # Private network
  "192.168.1.100",   # Specific host
]
```

**Effort**: Low (1 day)
**Security Level**: Low-Medium (as additional layer)

---

### Option 4: OAuth2/OIDC Integration
**Delegate authentication to external provider (Auth0, Keycloak, Google, etc.)**

#### Pros:
- Enterprise-grade auth
- Centralized user management
- SSO support
- Fine-grained permissions possible
- Audit logging

#### Cons:
- External dependency (provider must be available)
- More complex setup
- Requires token validation on each request
- Higher latency (token verification)
- Overkill for simple deployments

#### Implementation:
```rust
// Verify JWT token from OAuth provider
// Check claims and scopes
Authorization: Bearer <jwt-token>
```

#### Configuration:
```toml
[security.oauth]
enabled = true
issuer = "https://auth.example.com"
audience = "tei-manager"
jwks_uri = "https://auth.example.com/.well-known/jwks.json"
```

**Effort**: Medium-High (4-5 days)
**Security Level**: High

---

### Option 5: Basic Auth (Not Recommended for Production)
**Username/password sent with each request**

#### Pros:
- Extremely simple
- Universal support
- Good for development/testing

#### Cons:
- Credentials sent with every request (even over TLS)
- No built-in expiration
- Password management complexity
- Not suitable for automated systems

#### Implementation:
```rust
Authorization: Basic base64(username:password)
```

**Effort**: Very Low (< 1 day)
**Security Level**: Low

---

### Option 6: Hybrid Approach (Recommended for Production)
**Combine multiple methods for defense in depth**

#### Recommended Stack:
1. **IP Allowlist** (perimeter defense)
2. **API Key** (application auth)
3. **TLS** (transport security)
4. **Rate Limiting** (abuse prevention)

#### Configuration:
```toml
[security]
# API Key (required)
api_key = "${TEI_MANAGER_API_KEY}"

# IP Allowlist (optional, additional layer)
[security.ip_allowlist]
enabled = true
allowed_ips = ["10.0.0.0/8", "172.16.0.0/12"]

# TLS (optional but recommended)
[security.tls]
enabled = true
cert = "/certs/server.pem"
key = "/certs/server-key.pem"

# Rate Limiting (DoS prevention)
[security.rate_limit]
enabled = true
requests_per_minute = 100
burst = 20
```

**Effort**: Medium (3-4 days for all components)
**Security Level**: High

---

## Implementation Priority

### Phase 1: Immediate (Week 1)
- **API Key Authentication** for both HTTP and gRPC
- **IP Allowlist** as optional additional layer
- Update all examples/docs with auth

### Phase 2: Near-term (Week 2-3)
- **TLS Support** (optional, user-provided certs)
- **Rate Limiting** per API key
- **Audit Logging** of authenticated requests

### Phase 3: Future
- **mTLS** support for service-to-service
- **OAuth2/OIDC** integration for enterprise users
- **Fine-grained permissions** (read-only vs admin)

---

## Recommended Configuration Examples

### Development (Minimal Security)
```toml
[security]
enabled = false  # WARNING: Development only!
```

### Production (API Key)
```toml
[security]
enabled = true
api_key = "${TEI_MANAGER_API_KEY}"  # Set via environment

[security.ip_allowlist]
enabled = true
allowed_ips = ["10.0.0.0/8"]  # Your VPC
```

### Production (High Security)
```toml
[security]
enabled = true
api_key = "${TEI_MANAGER_API_KEY}"

[security.tls]
enabled = true
cert = "/certs/server.pem"
key = "/certs/server-key.pem"

[security.ip_allowlist]
enabled = true
allowed_ips = ["10.0.0.0/8"]

[security.rate_limit]
enabled = true
requests_per_minute = 100
```

---

## Attack Scenarios & Mitigations

### Scenario 1: Public IP Exposure
**Attack**: Unauthorized user discovers public IP and creates instances to mine crypto or DoS
**Mitigation**: API Key (prevents unauthorized access) + Rate Limiting (prevents abuse)

### Scenario 2: API Key Leakage
**Attack**: API key leaked in logs, committed to GitHub, or intercepted
**Mitigation**: IP Allowlist (limits damage) + Key Rotation + TLS (prevents interception)

### Scenario 3: Internal Network Compromise
**Attack**: Attacker gains access to internal network
**Mitigation**: API Key (prevents lateral movement) + Audit Logging (detect intrusion)

### Scenario 4: DoS Attack
**Attack**: Flood API with requests to exhaust resources
**Mitigation**: Rate Limiting + Connection Limits + Request Timeouts

---

## Implementation Checklist

- [ ] Add `security` config section
- [ ] Implement API key verification middleware
- [ ] Add `Authorization` header check for HTTP
- [ ] Add metadata check for gRPC
- [ ] Implement IP allowlist middleware
- [ ] Add TLS configuration support
- [ ] Add rate limiting
- [ ] Update all examples with auth
- [ ] Add security documentation
- [ ] Add security testing
- [ ] Add audit logging

---

## Questions for Decision

1. **Primary Use Case**: Internal infrastructure or public-facing service?
2. **Authentication Needs**: Single key or multi-user?
3. **Network Environment**: Fixed IPs or dynamic?
4. **Compliance Requirements**: Any specific standards (SOC2, HIPAA, etc.)?
5. **Integration**: Need to integrate with existing auth system?
6. **Timeline**: When does this need to be production-ready?

---

## Recommended Next Steps

1. **Choose authentication method**: Start with API Key for simplicity
2. **Design API**: How keys are configured and verified
3. **Implement middleware**: Tower layers for HTTP and gRPC interceptors
4. **Add configuration**: Security section in config.toml
5. **Update examples**: Show authenticated requests
6. **Add tests**: Security bypass tests, auth tests
7. **Document**: Security best practices

