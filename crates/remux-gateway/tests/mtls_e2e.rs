//! Phase C PART 2 — mTLS client-certificate authentication, end-to-end against a
//! real `remuxd` + a real `remux-gateway` served over mTLS in-process.
//!
//! PKI is generated with rcgen: a client CA, and client leaf certs whose CN is
//! mapped (via `--mtls-identities`) to a role. Proves:
//! - a cert mapped to `operator` → operator-level access (read AND write);
//! - a cert mapped to `viewer` → 200 read, 403 on a write route;
//! - `require` mode WITHOUT a client cert → the connection is refused (handshake
//!   fails);
//! - `optional` mode WITHOUT a cert → bearer auth still works;
//! - an unmapped valid cert with NO default roles → authenticates but is 403.
//! - precedence: an mTLS cert identity WINS over a bearer presented in the same
//!   request (the bearer would be admin; the cert is viewer → 403 on write).

mod common;

use std::path::PathBuf;
use std::time::Duration;

use remux_authz::MtlsIdentities;
use remux_gateway::app::AppState as GwState;
use remux_gateway::auth::AuthConfig as GwAuth;
use remux_gateway::mtls::{MtlsAcceptor, MtlsConfig, MtlsMode};
use remux_gateway::tls::TlsMaterial as GwTls;

use common::{ensure_crypto_provider, start_harness, TEST_TOKEN};

// --- rcgen PKI -------------------------------------------------------------

/// A generated PKI: the client CA (PEM) and a factory for client identities.
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
            .push(rcgen::DnType::CommonName, "remux-test-client-ca");
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

    /// Issue a client cert with common-name `cn`, returning a reqwest PEM identity
    /// bundle (leaf cert + key).
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

// --- mTLS gateway harness --------------------------------------------------

struct MtlsGateway {
    base_url: String,
}

/// Start a real gateway over mTLS with the given mode + identity map.
async fn start_mtls_gateway(
    socket_path: PathBuf,
    ca_pem: &[u8],
    mode: MtlsMode,
    identities: MtlsIdentities,
) -> MtlsGateway {
    ensure_crypto_provider();
    let tls = GwTls::generate_self_signed().expect("gw self-signed cert");
    let cfg = MtlsConfig::new(ca_pem, "test-ca", mode, identities).expect("mtls config");
    let rustls = cfg
        .server_config(&tls.cert_pem, &tls.key_pem)
        .expect("mtls server config");
    let acceptor = MtlsAcceptor::new(rustls, cfg);
    let (listener, addr) = remux_gateway::server::bind_listener("127.0.0.1:0".parse().unwrap())
        .expect("bind gateway port");
    // The static admin bearer (TEST_TOKEN) is configured too, so we can prove
    // precedence (cert wins over bearer) and the optional-mode bearer fallback.
    let state = GwState::new(socket_path, GwAuth::new(TEST_TOKEN.to_string()));
    tokio::spawn(async move {
        let _ = remux_gateway::server::serve_mtls(listener, acceptor, state).await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    MtlsGateway {
        base_url: format!("https://{addr}"),
    }
}

/// Build the operator/viewer identity map.
fn id_map() -> MtlsIdentities {
    MtlsIdentities::new(
        [
            ("ops-laptop".to_string(), vec!["operator".to_string()]),
            ("dashboard".to_string(), vec!["viewer".to_string()]),
        ],
        // No default roles: an unmapped valid cert authenticates but is 403.
        vec![],
    )
}

/// A reqwest client presenting `identity_pem` as its client cert (and accepting
/// the gateway's self-signed server cert).
fn client_with_identity(identity_pem: &[u8]) -> reqwest::Client {
    let identity = reqwest::Identity::from_pem(identity_pem).expect("identity");
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .identity(identity)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

/// A reqwest client with NO client cert.
fn client_no_cert() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

#[tokio::test]
async fn operator_cert_gets_operator_access() {
    let harness = start_harness().await;
    let pki = Pki::new();
    let gw = start_mtls_gateway(
        harness.socket_path().to_path_buf(),
        &pki.ca_pem,
        MtlsMode::Optional,
        id_map(),
    )
    .await;
    let http = client_with_identity(&pki.client_identity_pem("ops-laptop"));

    // Read: list sessions (operator holds session.list).
    let resp = http
        .get(format!("{}/v1/sessions", gw.base_url))
        .send()
        .await
        .expect("list");
    assert_eq!(resp.status(), 200, "operator cert may list");

    // Write: create a session (operator holds session.create).
    let resp = http
        .post(format!("{}/v1/sessions", gw.base_url))
        .json(&serde_json::json!({ "name": "mtls-op", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("create");
    assert_eq!(resp.status(), 201, "operator cert may create");
}

#[tokio::test]
async fn viewer_cert_is_403_on_write() {
    let harness = start_harness().await;
    let pki = Pki::new();
    let gw = start_mtls_gateway(
        harness.socket_path().to_path_buf(),
        &pki.ca_pem,
        MtlsMode::Optional,
        id_map(),
    )
    .await;
    let http = client_with_identity(&pki.client_identity_pem("dashboard"));

    // viewer may read.
    let resp = http
        .get(format!("{}/v1/sessions", gw.base_url))
        .send()
        .await
        .expect("list");
    assert_eq!(resp.status(), 200, "viewer cert may list");

    // viewer may NOT create → 403.
    let resp = http
        .post(format!("{}/v1/sessions", gw.base_url))
        .json(&serde_json::json!({ "name": "nope", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("create");
    assert_eq!(resp.status(), 403, "viewer cert forbidden on write");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["kind"], "forbidden");
}

#[tokio::test]
async fn cert_identity_wins_over_bearer() {
    // The viewer cert is presented ALONGSIDE the admin bearer. The cert wins, so
    // a write is 403 (viewer) despite the admin token that would otherwise allow it.
    let harness = start_harness().await;
    let pki = Pki::new();
    let gw = start_mtls_gateway(
        harness.socket_path().to_path_buf(),
        &pki.ca_pem,
        MtlsMode::Optional,
        id_map(),
    )
    .await;
    let http = client_with_identity(&pki.client_identity_pem("dashboard"));

    let resp = http
        .post(format!("{}/v1/sessions", gw.base_url))
        .bearer_auth(TEST_TOKEN) // admin bearer — must be overridden by the cert
        .json(&serde_json::json!({ "name": "nope", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("create");
    assert_eq!(
        resp.status(),
        403,
        "cert identity (viewer) wins over admin bearer → 403 on write"
    );
}

#[tokio::test]
async fn unmapped_valid_cert_authenticates_but_is_403() {
    let harness = start_harness().await;
    let pki = Pki::new();
    let gw = start_mtls_gateway(
        harness.socket_path().to_path_buf(),
        &pki.ca_pem,
        MtlsMode::Optional,
        id_map(), // no default roles
    )
    .await;
    // A valid cert whose CN is not in the map → authenticated (cert valid) but no
    // roles → 403 on every route (even a read).
    let http = client_with_identity(&pki.client_identity_pem("stranger"));
    let resp = http
        .get(format!("{}/v1/sessions", gw.base_url))
        .send()
        .await
        .expect("list");
    assert_eq!(
        resp.status(),
        403,
        "unmapped valid cert with no default roles → 403"
    );
}

#[tokio::test]
async fn optional_mode_without_cert_falls_back_to_bearer() {
    let harness = start_harness().await;
    let pki = Pki::new();
    let gw = start_mtls_gateway(
        harness.socket_path().to_path_buf(),
        &pki.ca_pem,
        MtlsMode::Optional,
        id_map(),
    )
    .await;
    // No client cert: optional mode → bearer auth applies. Admin bearer → write ok.
    let http = client_no_cert();
    let resp = http
        .post(format!("{}/v1/sessions", gw.base_url))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({ "name": "bearer-ok", "command": ["/bin/sh"] }))
        .send()
        .await
        .expect("create");
    assert_eq!(
        resp.status(),
        201,
        "optional mode: admin bearer still works"
    );

    // And no cert + no bearer → 401.
    let resp = http
        .get(format!("{}/v1/sessions", gw.base_url))
        .send()
        .await
        .expect("list no auth");
    assert_eq!(resp.status(), 401, "no cert + no bearer → 401");
}

#[tokio::test]
async fn require_mode_without_cert_refuses_connection() {
    let harness = start_harness().await;
    let pki = Pki::new();
    let gw = start_mtls_gateway(
        harness.socket_path().to_path_buf(),
        &pki.ca_pem,
        MtlsMode::Require,
        id_map(),
    )
    .await;
    // No client cert in `require` mode → the TLS handshake itself fails; reqwest
    // surfaces a transport error (no HTTP status), and the gateway never crashes.
    let http = client_no_cert();
    let result = http
        .get(format!("{}/v1/sessions", gw.base_url))
        .send()
        .await;
    assert!(
        result.is_err(),
        "require mode without a client cert must refuse the connection (handshake error)"
    );

    // A valid operator cert still works in require mode.
    let http_ok = client_with_identity(&pki.client_identity_pem("ops-laptop"));
    let resp = http_ok
        .get(format!("{}/v1/sessions", gw.base_url))
        .send()
        .await
        .expect("operator cert in require mode");
    assert_eq!(resp.status(), 200, "valid cert works in require mode");
}
