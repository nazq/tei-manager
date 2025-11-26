# mTLS Configuration

TEI Manager supports mutual TLS (mTLS) for securing gRPC connections. This guide covers certificate generation and configuration.

## Overview

mTLS provides:
- **Server authentication** - Clients verify the server's identity
- **Client authentication** - Server verifies client certificates
- **Encryption** - All traffic is encrypted

## Quick Start

### 1. Generate Certificates

Using OpenSSL to create a self-signed CA and certificates:

```bash
# Create directory for certs
mkdir -p certs && cd certs

# Generate CA key and certificate
openssl genrsa -out ca-key.pem 4096
openssl req -new -x509 -days 365 -key ca-key.pem -out ca.pem \
  -subj "/CN=tei-manager-ca/O=TEI Manager"

# Generate server key and CSR
openssl genrsa -out server-key.pem 4096
openssl req -new -key server-key.pem -out server.csr \
  -subj "/CN=tei-manager/O=TEI Manager"

# Sign server certificate with CA
openssl x509 -req -days 365 -in server.csr \
  -CA ca.pem -CAkey ca-key.pem -CAcreateserial \
  -out server.pem \
  -extfile <(echo "subjectAltName=DNS:localhost,DNS:tei-manager,IP:127.0.0.1")

# Generate client key and CSR
openssl genrsa -out client-key.pem 4096
openssl req -new -key client-key.pem -out client.csr \
  -subj "/CN=tei-client/O=TEI Manager"

# Sign client certificate with CA
openssl x509 -req -days 365 -in client.csr \
  -CA ca.pem -CAkey ca-key.pem -CAcreateserial \
  -out client.pem

# Clean up CSRs
rm -f *.csr
```

### 2. Configure Server

Add to `tei-manager.toml`:

```toml
[grpc.tls]
cert_path = "/path/to/certs/server.pem"
key_path = "/path/to/certs/server-key.pem"
ca_path = "/path/to/certs/ca.pem"
require_client_cert = true
```

Or via environment variables:

```bash
export TEI_MANAGER_GRPC_TLS_CERT=/path/to/certs/server.pem
export TEI_MANAGER_GRPC_TLS_KEY=/path/to/certs/server-key.pem
export TEI_MANAGER_GRPC_TLS_CA=/path/to/certs/ca.pem
export TEI_MANAGER_GRPC_TLS_REQUIRE_CLIENT_CERT=true
```

### 3. Connect with Client

Using the bench-client:

```bash
bench-client -e https://localhost:9001 -i my-instance \
  --cert certs/client.pem \
  --key certs/client-key.pem \
  --ca certs/ca.pem \
  --mode arrow --num-texts 1000
```

Using grpcurl:

```bash
grpcurl -cacert certs/ca.pem \
  -cert certs/client.pem \
  -key certs/client-key.pem \
  -d '{"target": {"instance_name": "my-instance"}, "request": {"inputs": "test"}}' \
  localhost:9001 tei_multiplexer.v1.TeiMultiplexer/Embed
```

## Certificate Requirements

### Server Certificate

- Must have Subject Alternative Names (SANs) matching how clients connect:
  ```
  DNS:localhost
  DNS:tei-manager
  DNS:tei-manager.namespace.svc.cluster.local
  IP:127.0.0.1
  ```
- Signed by a CA that clients trust

### Client Certificate

- Signed by a CA that the server trusts (same CA or cross-signed)
- Subject CN/O can be used for authorization (see below)

### CA Certificate

- Used by server to verify client certificates
- Used by clients to verify server certificate
- Can be the same CA or different CAs for client/server

## Subject-Based Authorization

TEI Manager can restrict access based on client certificate subject:

```toml
[grpc.tls]
cert_path = "server.pem"
key_path = "server-key.pem"
ca_path = "ca.pem"
require_client_cert = true

# Only allow clients with these subjects
allowed_subjects = [
  "CN=authorized-client,O=My Org",
  "CN=another-client,O=My Org"
]
```

If `allowed_subjects` is empty or not set, any valid client certificate is accepted.

## SAN-Based Authorization

Alternatively, authorize based on Subject Alternative Names:

```toml
[grpc.tls]
# ...
allowed_sans = [
  "DNS:client1.example.com",
  "DNS:client2.example.com"
]
```

## Production Recommendations

### Certificate Rotation

1. Generate new certificates before expiry
2. Update server config to use new certs
3. Restart TEI Manager (graceful)
4. Distribute new client certs

### Kubernetes with cert-manager

```yaml
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: tei-manager-server
spec:
  secretName: tei-manager-tls
  issuerRef:
    name: ca-issuer
    kind: ClusterIssuer
  dnsNames:
    - tei-manager
    - tei-manager.default.svc.cluster.local
  usages:
    - server auth
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: tei-manager-client
spec:
  secretName: tei-client-tls
  issuerRef:
    name: ca-issuer
    kind: ClusterIssuer
  commonName: tei-client
  usages:
    - client auth
```

Mount in deployment:

```yaml
volumeMounts:
  - name: tls
    mountPath: /certs
    readOnly: true
volumes:
  - name: tls
    secret:
      secretName: tei-manager-tls
```

### Security Checklist

- [ ] Use 4096-bit RSA or P-384 EC keys
- [ ] Set certificate expiry ≤ 1 year
- [ ] Store private keys securely (K8s secrets, Vault)
- [ ] Enable `require_client_cert = true` in production
- [ ] Use specific `allowed_subjects` or `allowed_sans` if multi-tenant
- [ ] Monitor certificate expiry dates

## Troubleshooting

### "certificate verify failed"

- Client doesn't trust server CA: Add `--ca ca.pem` to client
- Server doesn't trust client CA: Check `ca_path` in server config
- Certificate expired: Check `openssl x509 -in cert.pem -noout -dates`

### "no certificate provided"

- Server requires client cert but client didn't send one
- Add `--cert` and `--key` to client command

### "SAN mismatch"

- Server certificate SANs don't include the hostname client is connecting to
- Regenerate server cert with correct SANs

### Testing without mTLS

For development, disable mTLS by removing the `[grpc.tls]` section from config. The server will accept plaintext gRPC connections.
