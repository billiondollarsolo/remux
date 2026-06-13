//! Phase C PART 2 — mTLS client-certificate authentication on the CONTROL PLANE,
//! end-to-end in-process over mTLS. Mirrors the gateway mTLS suite for the
//! `/cp/v1` fleet API.
//!
//! rcgen PKI: a client CA + client leaf certs mapped (via the identity map) to
//! fleet roles. Proves:
//! - a cert mapped to `fleet-admin` may list hosts AND resolve;
//! - a cert mapped to `fleet-viewer` may list hosts but is 403 on resolve;
//! - `require` mode without a client cert refuses the connection;
//! - an mTLS principal enforces the SAME per-route permissions as a bearer.

mod common;

use std::time::Duration;

use common::ensure_crypto_provider;
use remux_authz::MtlsIdentities;
use remux_control_plane::app::AppState as CpState;
use remux_control_plane::auth::AuthConfig as CpAuth;
use remux_control_plane::tls::TlsMaterial as CpTls;
use remux_gateway::mtls::{MtlsAcceptor, MtlsConfig, MtlsMode};
use serde_json::json;

const ADMIN_TOKEN: &str = "cp-admin-token-abc123";
const REGISTER_TOKEN: &str = "cp-register-token-xyz789";

struct Pki {
    ca_cert: rcgen::Certificate,
    ca_key: rcgen::KeyPair,
    ca_pem: Vec<u8>,
}

impl Pki {
    fn new() -> Self {
        let mut params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "remux-cp-test-client-ca");
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let ca_key = rcgen::KeyPair::generate().expect("ca key");
        let ca_cert = params.self_signed(&ca_key).expect("ca cert");
        let ca_pem = ca_cert.pem().into_bytes();
        Self {
            ca_cert,
            ca_key,
            ca_pem,
        }
    }

    fn client_identity_pem(&self, cn: &str) -> Vec<u8> {
        let mut params = rcgen::CertificateParams::new(Vec::new()).expect("leaf params");
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, cn);
        params.use_authority_key_identifier_extension = true;
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
        let leaf = params
            .signed_by(&leaf_key, &self.ca_cert, &self.ca_key)
            .expect("sign leaf");
        let mut pem = leaf.pem().into_bytes();
        pem.extend_from_slice(leaf_key.serialize_pem().as_bytes());
        pem
    }
}

async fn start_mtls_cp(ca_pem: &[u8], mode: MtlsMode, identities: MtlsIdentities) -> String {
    ensure_crypto_provider();
    let tls = CpTls::generate_self_signed().expect("cp self-signed cert");
    let cfg = MtlsConfig::new(ca_pem, "test-ca", mode, identities).expect("mtls config");
    let rustls = cfg
        .server_config(&tls.cert_pem, &tls.key_pem)
        .expect("mtls server config");
    let acceptor = MtlsAcceptor::new(rustls, cfg);
    let (listener, addr) =
        remux_control_plane::server::bind_listener("127.0.0.1:0".parse().unwrap())
            .expect("bind cp port");
    let auth = CpAuth::new(ADMIN_TOKEN.to_string(), REGISTER_TOKEN.to_string());
    let state = CpState::new(auth).with_gateway_timeout(Duration::from_secs(3));
    tokio::spawn(async move {
        let _ = remux_control_plane::server::serve_mtls(listener, acceptor, state).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    format!("https://{addr}")
}

fn fleet_id_map() -> MtlsIdentities {
    MtlsIdentities::new(
        [
            ("fleet-ops".to_string(), vec!["fleet-admin".to_string()]),
            ("fleet-dash".to_string(), vec!["fleet-viewer".to_string()]),
        ],
        vec![],
    )
}

fn client_with_identity(identity_pem: &[u8]) -> reqwest::Client {
    let identity = reqwest::Identity::from_pem(identity_pem).expect("identity");
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .identity(identity)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

#[tokio::test]
async fn fleet_admin_cert_lists_and_resolves() {
    let pki = Pki::new();
    let cp = start_mtls_cp(&pki.ca_pem, MtlsMode::Optional, fleet_id_map()).await;
    let http = client_with_identity(&pki.client_identity_pem("fleet-ops"));

    // fleet-admin holds fleet.hosts.read.
    let resp = http
        .get(format!("{cp}/cp/v1/hosts"))
        .send()
        .await
        .expect("hosts");
    assert_eq!(resp.status(), 200, "fleet-admin cert may list hosts");

    // fleet-admin holds fleet.resolve; no host matches → 404 (authorized, just
    // nothing to resolve to) — NOT 403.
    let resp = http
        .post(format!("{cp}/cp/v1/resolve"))
        .json(&json!({ "labels": {"env":"nope"}, "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("resolve");
    assert_eq!(
        resp.status(),
        404,
        "fleet-admin cert is authorized to resolve (404 = no host, not 403)"
    );
}

#[tokio::test]
async fn fleet_viewer_cert_is_403_on_resolve() {
    let pki = Pki::new();
    let cp = start_mtls_cp(&pki.ca_pem, MtlsMode::Optional, fleet_id_map()).await;
    let http = client_with_identity(&pki.client_identity_pem("fleet-dash"));

    // fleet-viewer may list hosts.
    let resp = http
        .get(format!("{cp}/cp/v1/hosts"))
        .send()
        .await
        .expect("hosts");
    assert_eq!(resp.status(), 200, "fleet-viewer cert may list hosts");

    // but LACKS fleet.resolve → 403.
    let resp = http
        .post(format!("{cp}/cp/v1/resolve"))
        .json(&json!({ "labels": {"env":"dev"}, "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("resolve");
    assert_eq!(resp.status(), 403, "fleet-viewer cert forbidden on resolve");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["kind"], "forbidden");
}

#[tokio::test]
async fn require_mode_without_cert_refuses_connection() {
    let pki = Pki::new();
    let cp = start_mtls_cp(&pki.ca_pem, MtlsMode::Require, fleet_id_map()).await;

    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    let result = http.get(format!("{cp}/cp/v1/hosts")).send().await;
    assert!(
        result.is_err(),
        "require mode without a client cert must refuse the connection"
    );
}
