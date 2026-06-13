//! Phase C — **mTLS client-certificate authentication** for the gateway (and,
//! reused, the control plane).
//!
//! This is the transport-specific half of mTLS: it configures the rustls server
//! to request + verify client certificates against an operator CA
//! (`--client-ca`), and, after the handshake, extracts the peer leaf cert's
//! identity (CN, or first SAN) and maps it — via the **pure**
//! [`remux_authz::MtlsIdentities`] helper — to a [`remux_authz::Principal`]. That
//! principal is stashed in the connection's request extensions so the gateway's
//! existing per-route [`remux_authz::Permission`] middleware can prefer it over a
//! bearer (cert identity wins) and enforce the SAME RBAC decision.
//!
//! Two modes (`--mtls-mode`):
//! - `optional` (default): if a client presents a *valid* cert, its identity is
//!   used; otherwise the request falls back to the existing bearer (token/JWT)
//!   resolution.
//! - `require`: a valid client cert is mandatory — the rustls handshake refuses a
//!   connection without one (`WebPkiClientVerifier` built without
//!   `allow_unauthenticated`).
//!
//! The wiring uses an `axum-server` custom [`Accept`]or wrapping `RustlsAcceptor`:
//! it completes the TLS handshake, reads `peer_certificates()`, derives the
//! [`MtlsPrincipal`] extension, and layers it onto the service so middleware can
//! read it. `/health` and `/openapi.json` stay public.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::middleware::AddExtension;
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use futures_util::future::BoxFuture;
use rustls::pki_types::CertificateDer;
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;
use tower::Layer;

use remux_authz::{MtlsIdentities, Principal};

/// Whether a valid client certificate is mandatory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MtlsMode {
    /// A valid client cert is used if presented; else fall back to bearer auth.
    Optional,
    /// A valid client cert is mandatory (handshake refuses connections without one).
    Require,
}

impl MtlsMode {
    /// Parse `optional` / `require` (case-insensitive). Used by the CLI.
    pub fn parse(s: &str) -> Result<MtlsMode, String> {
        match s.to_ascii_lowercase().as_str() {
            "optional" => Ok(MtlsMode::Optional),
            "require" => Ok(MtlsMode::Require),
            other => Err(format!(
                "invalid --mtls-mode {other:?} (expected 'optional' or 'require')"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            MtlsMode::Optional => "optional",
            MtlsMode::Require => "require",
        }
    }
}

impl std::fmt::Display for MtlsMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The resolved mTLS configuration: the client-CA root store, the mode, and the
/// identity→roles map. `None` at the call sites means mTLS is disabled (no
/// `--client-ca`), in which case TLS serves exactly as before.
#[derive(Clone)]
pub struct MtlsConfig {
    roots: Arc<RootCertStore>,
    mode: MtlsMode,
    identities: Arc<MtlsIdentities>,
}

/// An error setting up mTLS (bad client-CA PEM, verifier build failure).
#[derive(Debug, thiserror::Error)]
pub enum MtlsSetupError {
    #[error("failed to read client CA {path:?}: {source}")]
    CaRead {
        path: String,
        source: std::io::Error,
    },
    #[error("client CA {path:?} contained no valid PEM certificates")]
    CaEmpty { path: String },
    #[error("failed to build client-certificate verifier: {0}")]
    Verifier(String),
    #[error("failed to build mTLS server config: {0}")]
    ServerConfig(String),
}

impl MtlsConfig {
    /// Build an mTLS config from a client-CA PEM bundle, the mode, and the
    /// resolved identity map.
    pub fn new(
        client_ca_pem: &[u8],
        ca_path_hint: &str,
        mode: MtlsMode,
        identities: MtlsIdentities,
    ) -> Result<Self, MtlsSetupError> {
        let mut roots = RootCertStore::empty();
        let certs = rustls_pemfile::certs(&mut &client_ca_pem[..])
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| MtlsSetupError::CaRead {
                path: ca_path_hint.to_string(),
                source: e,
            })?;
        if certs.is_empty() {
            return Err(MtlsSetupError::CaEmpty {
                path: ca_path_hint.to_string(),
            });
        }
        let (added, _) = roots.add_parsable_certificates(certs);
        if added == 0 {
            return Err(MtlsSetupError::CaEmpty {
                path: ca_path_hint.to_string(),
            });
        }
        Ok(Self {
            roots: Arc::new(roots),
            mode,
            identities: Arc::new(identities),
        })
    }

    /// Load a client CA from a path, returning the resolved config.
    pub fn from_paths(
        client_ca: impl AsRef<Path>,
        mode: MtlsMode,
        identities: MtlsIdentities,
    ) -> Result<Self, MtlsSetupError> {
        let path = client_ca.as_ref();
        let pem = std::fs::read(path).map_err(|source| MtlsSetupError::CaRead {
            path: path.display().to_string(),
            source,
        })?;
        Self::new(&pem, &path.display().to_string(), mode, identities)
    }

    /// The configured mode.
    pub fn mode(&self) -> MtlsMode {
        self.mode
    }

    /// The number of explicitly-mapped identities (for startup logging).
    pub fn identity_count(&self) -> usize {
        self.identities.len()
    }

    /// Build the rustls [`WebPkiClientVerifier`] for this config. In `require`
    /// mode a client cert is mandatory; in `optional` mode it is allowed to be
    /// absent (`allow_unauthenticated`).
    fn client_verifier(
        &self,
    ) -> Result<Arc<dyn rustls::server::danger::ClientCertVerifier>, MtlsSetupError> {
        let builder = WebPkiClientVerifier::builder(self.roots.clone());
        let builder = match self.mode {
            MtlsMode::Require => builder,
            MtlsMode::Optional => builder.allow_unauthenticated(),
        };
        builder
            .build()
            .map_err(|e| MtlsSetupError::Verifier(e.to_string()))
    }

    /// Build a server [`RustlsConfig`] that terminates TLS with `cert_pem`/`key_pem`
    /// AND requests + verifies client certificates per this mTLS config.
    pub fn server_config(
        &self,
        cert_pem: &[u8],
        key_pem: &[u8],
    ) -> Result<RustlsConfig, MtlsSetupError> {
        let certs = rustls_pemfile::certs(&mut &cert_pem[..])
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| MtlsSetupError::ServerConfig(e.to_string()))?;
        let key = rustls_pemfile::private_key(&mut &key_pem[..])
            .map_err(|e| MtlsSetupError::ServerConfig(e.to_string()))?
            .ok_or_else(|| MtlsSetupError::ServerConfig("no private key in PEM".to_string()))?;

        let verifier = self.client_verifier()?;
        let mut config = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .map_err(|e| MtlsSetupError::ServerConfig(e.to_string()))?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(RustlsConfig::from_config(Arc::new(config)))
    }

    /// Derive the [`MtlsPrincipal`] for an accepted connection's peer certificates
    /// (if any presented a usable identity). Returns `None` when no client cert was
    /// presented (only possible in `optional` mode) or none yielded an identity.
    fn principal_for_peer(
        &self,
        peer: Option<&[CertificateDer<'static>]>,
    ) -> Option<MtlsPrincipal> {
        let leaf = peer.and_then(|chain| chain.first())?;
        let identity = extract_identity(leaf.as_ref())?;
        Some(MtlsPrincipal(self.identities.principal_for(&identity)))
    }
}

/// The mTLS-derived [`Principal`], injected as a request extension by the
/// [`MtlsAcceptor`]. Its presence means a valid client cert was verified; the
/// auth middleware then prefers it over any bearer (cert identity wins).
#[derive(Clone, Debug)]
pub struct MtlsPrincipal(pub Principal);

/// Extract the certificate identity from a DER leaf cert: its first CN, else its
/// first SAN (DNS name, RFC822/email, or URI). Returns `None` if neither is present
/// or the cert fails to parse.
pub fn extract_identity(der: &[u8]) -> Option<String> {
    use x509_parser::extensions::GeneralName;
    use x509_parser::prelude::*;

    let (_, cert) = X509Certificate::from_der(der).ok()?;

    // Prefer the subject CN.
    if let Some(cn) = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|a| a.as_str().ok())
    {
        let cn = cn.trim();
        if !cn.is_empty() {
            return Some(cn.to_string());
        }
    }

    // Fall back to the first usable SAN.
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for name in &san.value.general_names {
            match name {
                GeneralName::DNSName(s) | GeneralName::RFC822Name(s) | GeneralName::URI(s) => {
                    let s = s.trim();
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// An `axum-server` [`Accept`]or that wraps [`RustlsAcceptor`], completes the TLS
/// handshake, extracts the peer cert → [`MtlsPrincipal`], and layers it onto the
/// service as a request extension. Connections without a client cert in
/// `optional` mode simply carry no extension (bearer auth then applies).
#[derive(Clone)]
pub struct MtlsAcceptor {
    inner: RustlsAcceptor,
    config: MtlsConfig,
}

impl MtlsAcceptor {
    /// Build the acceptor from a server [`RustlsConfig`] (already carrying the
    /// client-cert verifier) and the [`MtlsConfig`] used to derive identities.
    pub fn new(rustls: RustlsConfig, config: MtlsConfig) -> Self {
        Self {
            inner: RustlsAcceptor::new(rustls).handshake_timeout(Duration::from_secs(10)),
            config,
        }
    }
}

impl<I, S> Accept<I, S> for MtlsAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = TlsStream<I>;
    type Service = AddExtension<S, Option<MtlsPrincipal>>;
    type Future = BoxFuture<'static, std::io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let acceptor = self.inner.clone();
        let config = self.config.clone();
        Box::pin(async move {
            let (stream, service) = acceptor.accept(stream, service).await?;
            let server_conn = stream.get_ref().1;
            let peer = server_conn.peer_certificates().map(|c| c.to_vec());
            let principal = config.principal_for_peer(peer.as_deref());
            let service = axum::Extension(principal).layer(service);
            Ok((stream, service))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mode() {
        assert_eq!(MtlsMode::parse("optional").unwrap(), MtlsMode::Optional);
        assert_eq!(MtlsMode::parse("REQUIRE").unwrap(), MtlsMode::Require);
        assert!(MtlsMode::parse("nope").is_err());
        assert_eq!(MtlsMode::Optional.to_string(), "optional");
    }

    #[test]
    fn extract_identity_prefers_cn_then_san() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        // With an explicit CN, the CN wins over the SAN.
        let mut params =
            rcgen::CertificateParams::new(vec!["san-only.example".to_string()]).expect("params");
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "operator-cn");
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.self_signed(&key).expect("cert");
        assert_eq!(extract_identity(cert.der()).unwrap(), "operator-cn");

        // With NO CN, the first SAN is used.
        let mut params2 = rcgen::CertificateParams::new(vec!["san-fallback.example".to_string()])
            .expect("params");
        params2.distinguished_name = rcgen::DistinguishedName::new();
        let cert2 = params2.self_signed(&key).expect("cert2");
        assert_eq!(
            extract_identity(cert2.der()).unwrap(),
            "san-fallback.example"
        );
    }

    #[test]
    fn extract_identity_none_on_garbage() {
        assert!(extract_identity(b"not a cert").is_none());
    }
}
