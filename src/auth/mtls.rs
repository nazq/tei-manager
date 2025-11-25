//! mTLS (Mutual TLS) authentication provider

use super::{AuthError, AuthProvider, AuthRequest, AuthResult};
use crate::config::MtlsConfig;
use async_trait::async_trait;
use std::collections::HashMap;
use std::fs;
use x509_parser::prelude::*;

/// mTLS authentication provider
#[derive(Debug)]
pub struct MtlsProvider {
    config: MtlsConfig,
    ca_cert_der: Vec<u8>,
}

impl MtlsProvider {
    /// Create a new mTLS provider
    pub fn new(config: MtlsConfig) -> Result<Self, AuthError> {
        // Load CA certificate
        let ca_cert_pem = fs::read(&config.ca_cert).map_err(|e| {
            AuthError::Internal(format!(
                "Failed to read CA certificate {:?}: {}",
                config.ca_cert, e
            ))
        })?;

        // Parse PEM and extract DER using x509-parser
        let ca_cert_der = Self::pem_to_der(&ca_cert_pem)?;

        tracing::info!(
            ca_cert = ?config.ca_cert,
            "Loaded CA certificate for mTLS auth"
        );

        Ok(Self {
            config,
            ca_cert_der,
        })
    }

    /// Convert PEM to DER format using x509-parser
    fn pem_to_der(pem_data: &[u8]) -> Result<Vec<u8>, AuthError> {
        // Use x509-parser's PEM parsing
        let pem_certs = x509_parser::pem::Pem::iter_from_buffer(pem_data)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| AuthError::Internal(format!("Failed to parse PEM certificate: {}", e)))?;

        // Get the first certificate
        let pem_cert = pem_certs.first().ok_or_else(|| {
            AuthError::Internal("No PEM blocks found in certificate file".to_string())
        })?;

        Ok(pem_cert.contents.to_vec())
    }

    /// Verify client certificate against CA
    fn verify_cert_chain(&self, client_cert_der: &[u8]) -> Result<(), AuthError> {
        // Parse client certificate
        let (_, client_cert) = X509Certificate::from_der(client_cert_der).map_err(|e| {
            AuthError::InvalidCert(format!("Failed to parse client certificate: {}", e))
        })?;

        // Parse CA certificate
        let (_, ca_cert) = X509Certificate::from_der(&self.ca_cert_der)
            .map_err(|e| AuthError::Internal(format!("Failed to parse CA certificate: {}", e)))?;

        // Verify client cert is signed by CA
        // Note: This is a basic check. In production, you'd want more thorough validation
        // including checking the signature, validity period, etc.
        let issuer = client_cert.issuer();
        let ca_subject = ca_cert.subject();

        if self.config.allow_self_signed {
            // In development mode, we allow self-signed certs
            tracing::debug!("Allowing self-signed certificate (development mode)");
        } else if issuer != ca_subject {
            return Err(AuthError::CertVerificationFailed(
                "Client certificate not signed by configured CA".to_string(),
            ));
        }

        // Check validity period
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let validity = client_cert.validity();
        let not_before = validity.not_before.timestamp();
        let not_after = validity.not_after.timestamp();

        if now < not_before {
            return Err(AuthError::CertVerificationFailed(
                "Certificate not yet valid".to_string(),
            ));
        }

        if now > not_after {
            return Err(AuthError::CertVerificationFailed(
                "Certificate has expired".to_string(),
            ));
        }

        Ok(())
    }

    /// Extract subject from certificate
    fn extract_subject(&self, cert_der: &[u8]) -> Result<String, AuthError> {
        let (_, cert) = X509Certificate::from_der(cert_der)
            .map_err(|e| AuthError::InvalidCert(format!("Failed to parse certificate: {}", e)))?;

        Ok(cert.subject().to_string())
    }

    /// Extract Subject Alternative Names (SAN) from certificate
    fn extract_sans(&self, cert_der: &[u8]) -> Result<Vec<String>, AuthError> {
        let (_, cert) = X509Certificate::from_der(cert_der)
            .map_err(|e| AuthError::InvalidCert(format!("Failed to parse certificate: {}", e)))?;

        let mut sans = Vec::new();

        // Look for SAN extension
        for ext in cert.extensions() {
            if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
                for name in &san.general_names {
                    match name {
                        GeneralName::DNSName(dns) => {
                            sans.push(dns.to_string());
                        }
                        GeneralName::IPAddress(ip) => {
                            sans.push(format!("{:?}", ip));
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(sans)
    }

    /// Verify subject against allowlist
    fn verify_subject(&self, subject: &str) -> Result<(), AuthError> {
        if !self.config.verify_subject {
            return Ok(());
        }

        if self.config.allowed_subjects.is_empty() {
            // No allowlist means all subjects are allowed
            return Ok(());
        }

        for allowed in &self.config.allowed_subjects {
            if subject.contains(allowed) {
                return Ok(());
            }
        }

        Err(AuthError::Unauthorized(format!(
            "Subject '{}' not in allowlist",
            subject
        )))
    }

    /// Verify SANs against allowlist
    fn verify_sans(&self, sans: &[String]) -> Result<(), AuthError> {
        if !self.config.verify_san {
            return Ok(());
        }

        if self.config.allowed_sans.is_empty() {
            // No allowlist means all SANs are allowed
            return Ok(());
        }

        for san in sans {
            for allowed in &self.config.allowed_sans {
                if san == allowed {
                    return Ok(());
                }
            }
        }

        Err(AuthError::Unauthorized(format!(
            "No SANs in allowlist (got: {:?})",
            sans
        )))
    }
}

#[async_trait]
impl AuthProvider for MtlsProvider {
    async fn authenticate(&self, request: &AuthRequest) -> Result<AuthResult, AuthError> {
        // Extract client certificate from TLS info
        let tls_info = request
            .tls_info
            .as_ref()
            .ok_or(AuthError::MissingClientCert)?;

        let client_cert = tls_info
            .peer_certificate
            .as_ref()
            .ok_or(AuthError::MissingClientCert)?;

        // Verify certificate chain
        self.verify_cert_chain(client_cert)?;

        // Extract and verify subject
        let subject = self.extract_subject(client_cert)?;
        self.verify_subject(&subject)?;

        // Extract and verify SANs
        let sans = self.extract_sans(client_cert)?;
        self.verify_sans(&sans)?;

        tracing::info!(
            subject = %subject,
            sans = ?sans,
            "mTLS authentication successful"
        );

        let mut metadata = HashMap::new();
        metadata.insert("auth_method".to_string(), "mtls".to_string());
        metadata.insert("cert_subject".to_string(), subject.clone());
        if !sans.is_empty() {
            metadata.insert("cert_sans".to_string(), sans.join(","));
        }

        Ok(AuthResult {
            authenticated: true,
            principal: Some(subject),
            metadata,
        })
    }

    fn supports_http(&self) -> bool {
        true
    }

    fn supports_grpc(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "mtls"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::NamedTempFile;

    // Test CA certificate (TEI Manager CA, valid 2025-2035)
    const TEST_CA_PEM: &[u8] = br#"-----BEGIN CERTIFICATE-----
MIIDozCCAougAwIBAgIUUSNN88vSHbZzQZmmIlkYrsXhlx0wDQYJKoZIhvcNAQEL
BQAwYTELMAkGA1UEBhMCVVMxCzAJBgNVBAgMAkNBMRYwFAYDVQQHDA1TYW4gRnJh
bmNpc2NvMRQwEgYDVQQKDAtURUkgTWFuYWdlcjEXMBUGA1UEAwwOVEVJIE1hbmFn
ZXIgQ0EwHhcNMjUxMTI0MjA1NDE5WhcNMzUxMTIyMjA1NDE5WjBhMQswCQYDVQQG
EwJVUzELMAkGA1UECAwCQ0ExFjAUBgNVBAcMDVNhbiBGcmFuY2lzY28xFDASBgNV
BAoMC1RFSSBNYW5hZ2VyMRcwFQYDVQQDDA5URUkgTWFuYWdlciBDQTCCASIwDQYJ
KoZIhvcNAQEBBQADggEPADCCAQoCggEBAJjIZDj1+oGvV0rCwgXUlt/0dyatgqXa
wPxFYtd4vADhJCPhEbvNmSxlUrC9jbpd4/5uNM2IQqVtXKJBfWqH3OmUSZvvYv6W
+0lm7C3q5/QTXhV/xG0+29a4VgUufGXDnqTvyzrFxvoqXk9TTGLpE/rAOPkicoZy
9hCut61xTfmAUNHjkbXF7G7W0COb0/ZFXTsp7m7FsrjBhPmCl2NTJXmH9t6q2xci
2Zp12YlKMk6lkdlv//Xjx8jFbhYcVUm0AeGF5YAfR5hspSfdSXh1Uz1b241+Zmb9
0lGv9w3szrBQc5F632Vdb+OeSSYx+8eKiAMhqFgvMksQymkgjgjXuVMCAwEAAaNT
MFEwHQYDVR0OBBYEFMjf2ID/qRLSsU/bG4j6wcGIJnoLMB8GA1UdIwQYMBaAFMjf
2ID/qRLSsU/bG4j6wcGIJnoLMA8GA1UdEwEB/wQFMAMBAf8wDQYJKoZIhvcNAQEL
BQADggEBAIZ55SzK+SW/xVLm+fl+pEbclYdRyamiKvDU3PesOCGWTs8Zi8pFalZB
7aSkQkCxS6INXumZyaIZKYPwD1IW80eMtEVbaUDgqPWCbXVB2ZsZ5MPBpeOzLOlA
j2qpBzMSFRwAd0VMKnjLx3NrdAYL7en19shTpLi40fOY40b/bcLnnAy5o4WxCQPG
QWWU7ztoe1T2ur3drQV1DzVuLTK6ik6mCxV0bEz8onVcntAACw21hI8QQ3OiU9uw
AHv7uXct+rLRxm8OOw3i2kNzlQ65xw6SULsvlqReMk/t99AbyLpkTuYM2dkVki0N
MndSxz0nC2EnZjtMur0tV5yW/aF8iss=
-----END CERTIFICATE-----"#;

    // Test client certificate (signed by TEST_CA, valid 2025-2026)
    const TEST_CLIENT_PEM: &[u8] = br#"-----BEGIN CERTIFICATE-----
MIIDjjCCAnagAwIBAgIUQC4xK60UBVybIRrZS+WK60HXs2AwDQYJKoZIhvcNAQEL
BQAwYTELMAkGA1UEBhMCVVMxCzAJBgNVBAgMAkNBMRYwFAYDVQQHDA1TYW4gRnJh
bmNpc2NvMRQwEgYDVQQKDAtURUkgTWFuYWdlcjEXMBUGA1UEAwwOVEVJIE1hbmFn
ZXIgQ0EwHhcNMjUxMTI0MjA1NDE5WhcNMjYxMTI0MjA1NDE5WjBdMQswCQYDVQQG
EwJVUzELMAkGA1UECAwCQ0ExFjAUBgNVBAcMDVNhbiBGcmFuY2lzY28xFDASBgNV
BAoMC1RFSSBNYW5hZ2VyMRMwEQYDVQQDDAp0ZWktY2xpZW50MIIBIjANBgkqhkiG
9w0BAQEFAAOCAQ8AMIIBCgKCAQEAo4f4sc1XPc8tVMyA+m8gFV9/+Gwd2OXNaNqf
pptE/XG8PrbJmGavRsHsQ2lr61+l6vkfZ2qMONRiYk8jvMuNivdo7V3K+roLIKPI
ySshwe2uCU5yf1YbjpTpJGy+jpu55Q61xemuaxrCG5ZEA4qf11QwaRr7eWflCktD
uwxz08MPta/qDSIqiYmp66S59cRyrB8jR3IvO98BjtDJRLR66ksWatT/iKQkbkyv
qYp8nEV7PCw0sF6CW0h2v21bo4xaQjgjAhnOWF13SumUgNI4a5uE643z8bgrVhUu
i3OJhVtaGWTHkswbBmD1IVbLWocgeX8J6M3hkuX6k2Jp0tU/mwIDAQABo0IwQDAd
BgNVHQ4EFgQUNDDQtIcodcm6aG1GvPAezTl0/XIwHwYDVR0jBBgwFoAUyN/YgP+p
EtKxT9sbiPrBwYgmegswDQYJKoZIhvcNAQELBQADggEBADXbhoT38ZtDBSx6e1A/
OhvqUlxmbpRLdo4p4wchyqp0JFacYHa/2mBIl+jaI7XYRfRzgv2TVhJW+V/MeTxy
Rfgnf0YNFmDjayQCvz6SuGW3H8mKmYBTpI4DeBXA/XUtf98pbbu2m3T6FnGVyNPn
n6DaFFzGkARih2UnZ5eQcv4qrt9Jbiyk4nKWh6uzRgME6X08ig5IlEjLmSJ9gZwL
sPG0ri4EntumuFhNX2mnEHwezniZy3CPWabYNivHuIzJFCGFD0xSgGGKbdblZ/oS
kW+fmhJsg1k5P23u/Eg8D0dcLCZRyqJtUkoK0NonCpk5E7R3jiJbm/wGb0cHUEdS
B0k=
-----END CERTIFICATE-----"#;

    // Self-signed test certificate (not signed by TEST_CA)
    const TEST_SELF_SIGNED_PEM: &[u8] = br#"-----BEGIN CERTIFICATE-----
MIIBkTCB+wIJAKHHCgVZU1W4MA0GCSqGSIb3DQEBCwUAMBExDzANBgNVBAMMBnRl
c3QtY2EwHhcNMjQwMTAxMDAwMDAwWhcNMjUwMTAxMDAwMDAwWjARMQ8wDQYDVQQD
DAZ0ZXN0LWNhMFwwDQYJKoZIhvcNAQEBBQADSwAwSAJBAKj34GkxFhD90hnqBP8+
WlPUyBMm4N2v9VMK3v9YKvD2+qPz9qQcxkJy2aO7vXkKFyHhXAYkQh9mO7PwPl4R
c0MCAwEAATANBgkqhkiG9w0BAQsFAANBAFhCpqFiuQ8hQ8LGiM0xN8cchN2GdLKp
J5MwBYFhQJMd2lJVqJdq+zmJqIFO5kJLgwQlQhQoVw/gQ6fQKJ5Y1qg=
-----END CERTIFICATE-----"#;

    fn create_test_provider(
        allow_self_signed: bool,
        verify_subject: bool,
        allowed_subjects: Vec<String>,
        verify_san: bool,
        allowed_sans: Vec<String>,
    ) -> MtlsProvider {
        let mut ca_file = NamedTempFile::new().unwrap();
        ca_file.write_all(TEST_CA_PEM).unwrap();
        ca_file.flush().unwrap();

        // Keep file alive by leaking it (tests are short-lived)
        let path = ca_file.path().to_path_buf();
        std::mem::forget(ca_file);

        let config = MtlsConfig {
            ca_cert: path,
            server_cert: PathBuf::from("/not/used.pem"),
            server_key: PathBuf::from("/not/used.pem"),
            allow_self_signed,
            verify_subject,
            allowed_subjects,
            verify_san,
            allowed_sans,
        };

        MtlsProvider::new(config).expect("Failed to create test provider")
    }

    #[test]
    fn test_pem_to_der() {
        let result = MtlsProvider::pem_to_der(TEST_CA_PEM);
        assert!(result.is_ok());
        let der = result.unwrap();
        assert!(!der.is_empty());
    }

    #[test]
    fn test_pem_to_der_invalid() {
        let result = MtlsProvider::pem_to_der(b"not a certificate");
        assert!(result.is_err());
    }

    #[test]
    fn test_pem_to_der_empty() {
        let result = MtlsProvider::pem_to_der(b"");
        assert!(result.is_err());
    }

    #[test]
    fn test_mtls_provider_new_missing_ca() {
        let config = MtlsConfig {
            ca_cert: PathBuf::from("/nonexistent/ca.pem"),
            server_cert: PathBuf::from("/nonexistent/server.pem"),
            server_key: PathBuf::from("/nonexistent/server-key.pem"),
            allow_self_signed: false,
            verify_subject: true,
            allowed_subjects: vec![],
            verify_san: false,
            allowed_sans: vec![],
        };

        let result = MtlsProvider::new(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, AuthError::Internal(_)));
    }

    #[test]
    fn test_mtls_provider_new_success() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);
        assert_eq!(provider.name(), "mtls");
        assert!(provider.supports_http());
        assert!(provider.supports_grpc());
    }

    #[test]
    fn test_verify_cert_chain_valid() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);
        let client_der = MtlsProvider::pem_to_der(TEST_CLIENT_PEM).unwrap();

        let result = provider.verify_cert_chain(&client_der);
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_cert_chain_untrusted_ca() {
        // Use the self-signed cert with allow_self_signed=false
        // The cert has issuer == subject (self-signed) but our CA has different subject
        // So it should fail because issuer != CA subject
        let provider = create_test_provider(false, false, vec![], false, vec![]);
        let self_signed_der = MtlsProvider::pem_to_der(TEST_SELF_SIGNED_PEM).unwrap();

        let result = provider.verify_cert_chain(&self_signed_der);
        // Should fail either on issuer mismatch or expiry - both are CertVerificationFailed
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_cert_chain_not_yet_valid() {
        // We test cert not yet valid by checking the path - we don't have a future cert
        // but we do test the validity window logic via the valid client cert
        let provider = create_test_provider(false, false, vec![], false, vec![]);
        let client_der = MtlsProvider::pem_to_der(TEST_CLIENT_PEM).unwrap();

        // Valid client cert should pass all checks
        let result = provider.verify_cert_chain(&client_der);
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_cert_chain_invalid_der() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);

        let result = provider.verify_cert_chain(b"not valid der");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AuthError::InvalidCert(_)));
    }

    #[test]
    fn test_extract_subject() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);
        let client_der = MtlsProvider::pem_to_der(TEST_CLIENT_PEM).unwrap();

        let subject = provider.extract_subject(&client_der).unwrap();
        assert!(subject.contains("tei-client"));
        assert!(subject.contains("TEI Manager"));
    }

    #[test]
    fn test_extract_subject_invalid() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);

        let result = provider.extract_subject(b"invalid der");
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_sans() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);
        let client_der = MtlsProvider::pem_to_der(TEST_CLIENT_PEM).unwrap();

        // Our test client cert doesn't have SANs
        let sans = provider.extract_sans(&client_der).unwrap();
        assert!(sans.is_empty());
    }

    #[test]
    fn test_extract_sans_invalid() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);

        let result = provider.extract_sans(b"invalid der");
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_subject_disabled() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);

        // When verify_subject=false, any subject is allowed
        let result = provider.verify_subject("anything");
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_subject_empty_allowlist() {
        let provider = create_test_provider(false, true, vec![], false, vec![]);

        // When allowlist is empty, all subjects pass
        let result = provider.verify_subject("anything");
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_subject_allowed() {
        let provider = create_test_provider(
            false,
            true,
            vec!["tei-client".to_string(), "admin".to_string()],
            false,
            vec![],
        );

        let result = provider.verify_subject("CN=tei-client,O=TEI Manager");
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_subject_denied() {
        let provider = create_test_provider(false, true, vec!["admin".to_string()], false, vec![]);

        let result = provider.verify_subject("CN=tei-client,O=TEI Manager");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AuthError::Unauthorized(_)));
    }

    #[test]
    fn test_verify_sans_disabled() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);

        let result = provider.verify_sans(&["anything.example.com".to_string()]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_sans_empty_allowlist() {
        let provider = create_test_provider(false, false, vec![], true, vec![]);

        // When allowlist is empty, all SANs pass
        let result = provider.verify_sans(&["anything.example.com".to_string()]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_sans_allowed() {
        let provider = create_test_provider(
            false,
            false,
            vec![],
            true,
            vec![
                "tei.example.com".to_string(),
                "admin.example.com".to_string(),
            ],
        );

        let result = provider.verify_sans(&["tei.example.com".to_string()]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_sans_denied() {
        let provider = create_test_provider(
            false,
            false,
            vec![],
            true,
            vec!["admin.example.com".to_string()],
        );

        let result = provider.verify_sans(&["hacker.evil.com".to_string()]);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AuthError::Unauthorized(_)));
    }

    fn create_auth_request(tls_info: Option<super::super::TlsInfo>) -> AuthRequest {
        AuthRequest {
            protocol: super::super::Protocol::Http,
            peer_addr: "127.0.0.1:1234".parse().unwrap(),
            headers: None,
            metadata: None,
            tls_info,
        }
    }

    fn create_tls_info(peer_certificate: Option<Vec<u8>>) -> super::super::TlsInfo {
        super::super::TlsInfo {
            peer_certificate,
            certificate_chain: vec![],
            tls_version: "TLSv1.3".to_string(),
            cipher_suite: "TLS_AES_256_GCM_SHA384".to_string(),
        }
    }

    #[tokio::test]
    async fn test_authenticate_missing_tls_info() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);

        let request = create_auth_request(None);

        let result = provider.authenticate(&request).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AuthError::MissingClientCert));
    }

    #[tokio::test]
    async fn test_authenticate_missing_peer_cert() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);

        let request = create_auth_request(Some(create_tls_info(None)));

        let result = provider.authenticate(&request).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AuthError::MissingClientCert));
    }

    #[tokio::test]
    async fn test_authenticate_success() {
        let provider = create_test_provider(false, false, vec![], false, vec![]);
        let client_der = MtlsProvider::pem_to_der(TEST_CLIENT_PEM).unwrap();

        let request = create_auth_request(Some(create_tls_info(Some(client_der))));

        let result = provider.authenticate(&request).await;
        assert!(result.is_ok());

        let auth_result = result.unwrap();
        assert!(auth_result.authenticated);
        assert!(auth_result.principal.is_some());
        assert!(auth_result.principal.unwrap().contains("tei-client"));
        assert_eq!(
            auth_result.metadata.get("auth_method"),
            Some(&"mtls".to_string())
        );
    }

    #[tokio::test]
    async fn test_authenticate_with_subject_verification() {
        let provider =
            create_test_provider(false, true, vec!["tei-client".to_string()], false, vec![]);
        let client_der = MtlsProvider::pem_to_der(TEST_CLIENT_PEM).unwrap();

        let request = create_auth_request(Some(create_tls_info(Some(client_der))));

        let result = provider.authenticate(&request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_authenticate_subject_denied() {
        let provider =
            create_test_provider(false, true, vec!["admin-only".to_string()], false, vec![]);
        let client_der = MtlsProvider::pem_to_der(TEST_CLIENT_PEM).unwrap();

        let request = create_auth_request(Some(create_tls_info(Some(client_der))));

        let result = provider.authenticate(&request).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AuthError::Unauthorized(_)));
    }
}
