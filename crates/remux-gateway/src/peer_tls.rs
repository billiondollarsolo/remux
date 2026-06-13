//! Outbound-TLS trust configuration for the **client** side of the
//! gateway↔control-plane links — shared by the gateway's `--register` client
//! (gateway → CP) and the control plane's [`GatewayClient`](crate) (CP → gateway,
//! which lives in the control-plane crate but reuses this helper).
//!
//! Phase C replaces the old blanket `danger_accept_invalid_certs(true)` posture
//! (insecure-by-default) with three explicit, **secure-by-default** trust modes:
//!
//! - **CA bundle** (`--gateway-ca` / `--register-ca`): trust a PEM CA bundle for
//!   the peer (works with the peer's own self-signed cert used *as* a CA root).
//! - **SHA-256 leaf pin** (`--gateway-pin` / `--register-pin`, repeatable): accept
//!   ONLY a leaf certificate whose SHA-256 fingerprint matches one of the pins —
//!   no CA needed, perfect for a self-signed peer. Implemented via a custom
//!   [`rustls`] [`ServerCertVerifier`] plugged into reqwest with
//!   `use_preconfigured_tls`.
//! - **System roots** (the default when nothing else is set): verify against the
//!   OS/webpki trust store. A self-signed peer then fails the handshake with a
//!   clear, operator-actionable error.
//!
//! `--insecure` remains an explicit, loudly-logged dev-only opt-out
//! (`danger_accept_invalid_certs`).

use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::aws_lc_rs as rustls_aws_lc_rs;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

/// How the outbound client should verify the peer's TLS certificate. Resolved
/// once from the CLI flags; secure-by-default (`SystemRoots`) when nothing is set.
#[derive(Debug, Clone)]
pub enum PeerVerification {
    /// Verify against the OS / webpki trust store (the secure default). A
    /// self-signed peer fails the handshake.
    SystemRoots,
    /// Trust this PEM CA bundle for the peer (its own self-signed cert may be
    /// used as the CA root).
    CaBundle(Vec<u8>),
    /// Accept ONLY a leaf cert whose SHA-256 fingerprint matches one of these
    /// (lower-case hex, optional `:`/whitespace separators ignored). No CA needed.
    Pins(Vec<String>),
    /// DEV-ONLY: accept any certificate (`danger_accept_invalid_certs`). Explicit,
    /// loudly-logged opt-out.
    Insecure,
}

/// An error building the outbound TLS client (bad CA PEM, bad pin, rustls/reqwest
/// build failure). Rendered to a human string for the startup/config path.
#[derive(Debug, thiserror::Error)]
pub enum PeerTlsError {
    #[error("failed to read CA bundle {path:?}: {source}")]
    CaRead {
        path: String,
        source: std::io::Error,
    },
    #[error("CA bundle {path:?} contained no valid PEM certificates")]
    CaEmpty { path: String },
    #[error("invalid certificate pin {pin:?}: {reason}")]
    BadPin { pin: String, reason: String },
    #[error("no certificate pins were provided")]
    NoPins,
    #[error("failed to build outbound TLS client: {0}")]
    Build(String),
}

impl PeerVerification {
    /// Resolve the verification mode from the four mutually-prioritised inputs:
    /// an explicit `--insecure` wins (dev opt-out); else a CA bundle path; else
    /// one or more pins; else the secure system-roots default.
    ///
    /// `ca_pem` is the *already-read* CA bundle bytes (the caller reads the file so
    /// IO errors surface with the flag's path); `pins` are the raw `--*-pin`
    /// values. Returns the mode plus a short human label for logging.
    pub fn resolve(
        insecure: bool,
        ca_pem: Option<Vec<u8>>,
        pins: Vec<String>,
    ) -> (PeerVerification, &'static str) {
        if insecure {
            (
                PeerVerification::Insecure,
                "insecure (dev-only: accept any cert)",
            )
        } else if let Some(pem) = ca_pem {
            (PeerVerification::CaBundle(pem), "CA bundle")
        } else if !pins.is_empty() {
            (PeerVerification::Pins(pins), "SHA-256 leaf pin")
        } else {
            (
                PeerVerification::SystemRoots,
                "system roots (secure default)",
            )
        }
    }
}

/// Normalise a SHA-256 pin string to 64 lower-case hex chars, stripping `:` and
/// whitespace separators. Errors if it is not 32 bytes of hex.
fn normalise_pin(pin: &str) -> Result<String, PeerTlsError> {
    let cleaned: String = pin
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .flat_map(|c| c.to_lowercase())
        .collect();
    if cleaned.len() != 64 {
        return Err(PeerTlsError::BadPin {
            pin: pin.to_string(),
            reason: format!("expected 64 hex chars (32 bytes), got {}", cleaned.len()),
        });
    }
    if let Some(bad) = cleaned.chars().find(|c| !c.is_ascii_hexdigit()) {
        return Err(PeerTlsError::BadPin {
            pin: pin.to_string(),
            reason: format!("non-hex character {bad:?}"),
        });
    }
    Ok(cleaned)
}

/// The SHA-256 fingerprint of a DER certificate as lower-case hex (no separators).
pub fn sha256_fingerprint(der: &[u8]) -> String {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, der);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-256 fingerprint of the FIRST certificate in a PEM bundle, as
/// lower-case hex. Convenience for operators/tests pinning a self-signed peer
/// from its PEM file. Returns `None` if the PEM has no certificate.
pub fn sha256_fingerprint_of_pem(pem: &[u8]) -> Option<String> {
    let der = rustls_pemfile::certs(&mut &pem[..]).next()?.ok()?;
    Some(sha256_fingerprint(der.as_ref()))
}

/// A rustls [`ServerCertVerifier`] that accepts ONLY a leaf cert whose SHA-256
/// fingerprint matches one of the configured pins. Signature checks still run
/// (the handshake must prove possession of the pinned cert's key); only the chain
/// trust is replaced by the pin set. Works with a self-signed peer (no CA).
#[derive(Debug)]
struct PinnedCertVerifier {
    pins: Vec<String>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = sha256_fingerprint(end_entity);
        if self.pins.iter().any(|p| p == &fp) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "peer leaf certificate SHA-256 {fp} does not match any configured pin"
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build a rustls [`ClientConfig`] for the CA-bundle mode: an empty root store
/// seeded ONLY with the operator-provided CA bundle (the peer must chain to it).
fn ca_bundle_config(pem: &[u8], path_hint: &str) -> Result<ClientConfig, PeerTlsError> {
    let mut roots = RootCertStore::empty();
    let certs = rustls_pemfile::certs(&mut &pem[..])
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| PeerTlsError::CaRead {
            path: path_hint.to_string(),
            source: e,
        })?;
    if certs.is_empty() {
        return Err(PeerTlsError::CaEmpty {
            path: path_hint.to_string(),
        });
    }
    let (added, _ignored) = roots.add_parsable_certificates(certs);
    if added == 0 {
        return Err(PeerTlsError::CaEmpty {
            path: path_hint.to_string(),
        });
    }
    Ok(ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// Build a rustls [`ClientConfig`] for the pin mode using [`PinnedCertVerifier`].
fn pinned_config(pins: &[String]) -> Result<ClientConfig, PeerTlsError> {
    if pins.is_empty() {
        return Err(PeerTlsError::NoPins);
    }
    let normalised = pins
        .iter()
        .map(|p| normalise_pin(p))
        .collect::<Result<Vec<_>, _>>()?;
    let provider = rustls_aws_lc_rs::default_provider();
    let verifier = PinnedCertVerifier {
        pins: normalised,
        provider: Arc::new(provider),
    };
    Ok(ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth())
}

/// Build an outbound [`reqwest::Client`] honouring `verification`, with the given
/// per-request `timeout`. `path_hint` is the CA flag's path for error messages.
pub fn build_client(
    verification: &PeerVerification,
    timeout: Duration,
    path_hint: &str,
) -> Result<reqwest::Client, PeerTlsError> {
    let builder = reqwest::Client::builder().timeout(timeout);
    let builder = match verification {
        PeerVerification::Insecure => builder.danger_accept_invalid_certs(true),
        PeerVerification::SystemRoots => builder, // reqwest's default: webpki/system roots.
        PeerVerification::CaBundle(pem) => {
            let cfg = ca_bundle_config(pem, path_hint)?;
            builder.use_preconfigured_tls(cfg)
        }
        PeerVerification::Pins(pins) => {
            let cfg = pinned_config(pins)?;
            builder.use_preconfigured_tls(cfg)
        }
    };
    builder
        .build()
        .map_err(|e| PeerTlsError::Build(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_provider() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = rustls_aws_lc_rs::default_provider().install_default();
        });
    }

    #[test]
    fn normalise_pin_strips_separators_and_lowercases() {
        let raw = "AB:CD".to_string() + &"00".repeat(30);
        let p = normalise_pin(&raw).unwrap();
        assert_eq!(p.len(), 64);
        assert!(p.starts_with("abcd"));
    }

    #[test]
    fn normalise_pin_rejects_bad_length_and_nonhex() {
        assert!(normalise_pin("abcd").is_err());
        let nonhex = "zz".to_string() + &"00".repeat(31);
        assert!(normalise_pin(&nonhex).is_err());
    }

    #[test]
    fn sha256_fingerprint_is_64_hex() {
        ensure_provider();
        let fp = sha256_fingerprint(b"hello");
        assert_eq!(fp.len(), 64);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        // Known SHA-256("hello") prefix.
        assert!(fp.starts_with("2cf24dba5fb0a30e"));
    }

    #[test]
    fn resolve_prioritises_insecure_then_ca_then_pins_then_system() {
        let (m, _) = PeerVerification::resolve(true, Some(vec![1]), vec!["x".into()]);
        assert!(matches!(m, PeerVerification::Insecure));
        let (m, _) = PeerVerification::resolve(false, Some(vec![1]), vec!["x".into()]);
        assert!(matches!(m, PeerVerification::CaBundle(_)));
        let (m, _) = PeerVerification::resolve(false, None, vec!["x".into()]);
        assert!(matches!(m, PeerVerification::Pins(_)));
        let (m, label) = PeerVerification::resolve(false, None, vec![]);
        assert!(matches!(m, PeerVerification::SystemRoots));
        assert!(label.contains("secure"));
    }

    #[test]
    fn pinned_config_rejects_bad_pin() {
        ensure_provider();
        assert!(pinned_config(&["nope".to_string()]).is_err());
        assert!(pinned_config(&[]).is_err());
    }

    #[test]
    fn ca_bundle_config_rejects_empty() {
        ensure_provider();
        assert!(ca_bundle_config(b"not a pem", "x").is_err());
    }

    #[test]
    fn build_client_works_for_each_mode() {
        ensure_provider();
        let t = Duration::from_secs(5);
        assert!(build_client(&PeerVerification::SystemRoots, t, "").is_ok());
        assert!(build_client(&PeerVerification::Insecure, t, "").is_ok());
        let valid_pin = "ab".repeat(32);
        assert!(build_client(&PeerVerification::Pins(vec![valid_pin]), t, "").is_ok());
    }
}
