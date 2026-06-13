//! TLS material for the control plane: operator-provided PEM cert/key, or a
//! self-signed cert generated for `127.0.0.1`/`localhost` so the service works
//! out of the box on loopback.
//!
//! This duplicates the gateway's tiny self-signed helper rather than exporting
//! the gateway's internals — the two services have independent TLS lifecycles.
//! TLS is **always on**; there is no plaintext listener.

use std::path::PathBuf;

use axum_server::tls_rustls::RustlsConfig;

/// PEM material backing the control-plane TLS listener plus provenance metadata
/// for the startup log.
#[derive(Debug)]
pub struct TlsMaterial {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    /// A short non-cryptographic fingerprint of the cert PEM, for the startup log.
    pub fingerprint: String,
    /// Whether the cert was generated (self-signed) vs operator-provided.
    pub self_signed: bool,
}

impl TlsMaterial {
    /// Load operator PEM if both paths are given, else generate a self-signed
    /// cert for loopback. Providing only one of the two paths is an error.
    pub fn resolve(cert_path: Option<PathBuf>, key_path: Option<PathBuf>) -> Result<Self, String> {
        match (cert_path, key_path) {
            (Some(cert), Some(key)) => {
                let cert_pem = std::fs::read(&cert)
                    .map_err(|e| format!("failed to read TLS cert {}: {e}", cert.display()))?;
                let key_pem = std::fs::read(&key)
                    .map_err(|e| format!("failed to read TLS key {}: {e}", key.display()))?;
                let fingerprint = fingerprint(&cert_pem);
                Ok(TlsMaterial {
                    cert_pem,
                    key_pem,
                    fingerprint,
                    self_signed: false,
                })
            }
            (None, None) => Self::generate_self_signed(),
            _ => Err(
                "both --tls-cert and --tls-key must be provided together (or neither, \
                      to generate a self-signed cert)"
                    .to_string(),
            ),
        }
    }

    /// Generate a self-signed cert/key for `127.0.0.1` and `localhost`.
    pub fn generate_self_signed() -> Result<Self, String> {
        let sans = vec!["127.0.0.1".to_string(), "localhost".to_string()];
        let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(sans)
            .map_err(|e| format!("failed to generate self-signed cert: {e}"))?;
        let cert_pem = cert.pem().into_bytes();
        let key_pem = key_pair.serialize_pem().into_bytes();
        let fingerprint = fingerprint(&cert_pem);
        Ok(TlsMaterial {
            cert_pem,
            key_pem,
            fingerprint,
            self_signed: true,
        })
    }

    /// Build the `axum-server` rustls config from this material.
    pub async fn into_rustls_config(self) -> Result<RustlsConfig, String> {
        RustlsConfig::from_pem(self.cert_pem, self.key_pem)
            .await
            .map_err(|e| format!("failed to build rustls config: {e}"))
    }
}

/// A short, non-cryptographic fingerprint of the cert PEM bytes (FNV-1a), for
/// logging only — an operator-convenience identifier, not a security control.
fn fingerprint(cert_pem: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in cert_pem {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash.to_be_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_generates_pems() {
        let m = TlsMaterial::generate_self_signed().expect("generate");
        assert!(m.self_signed);
        let cert = String::from_utf8(m.cert_pem.clone()).unwrap();
        let key = String::from_utf8(m.key_pem.clone()).unwrap();
        assert!(cert.contains("BEGIN CERTIFICATE"));
        assert!(key.contains("PRIVATE KEY"));
        assert!(!m.fingerprint.is_empty());
    }

    #[test]
    fn resolve_rejects_only_one_of_cert_key() {
        let err = TlsMaterial::resolve(Some(PathBuf::from("/x")), None).unwrap_err();
        assert!(err.contains("both"));
        let err = TlsMaterial::resolve(None, Some(PathBuf::from("/y"))).unwrap_err();
        assert!(err.contains("both"));
    }
}
