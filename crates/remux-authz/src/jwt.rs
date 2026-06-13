//! [`JwtValidator`] — Phase B of auth hardening: validate a JWT (e.g. from an
//! OIDC provider) and map its claims to a [`Principal`], so a JWT-authenticated
//! caller flows through the **exact same** RBAC enforcement as a static bearer
//! token.
//!
//! This module is **pure and offline-testable**: it only *consumes* keys (an
//! HS256 secret, a static RS256/ES256 public-key PEM, or an already-parsed
//! JWKS). It performs **no network I/O** — JWKS *fetching* belongs in the
//! services (they can fetch the JWKS JSON over HTTPS and hand the parsed set
//! here via [`parse_jwks`]).
//!
//! Validation verifies the signature and `exp` (and `iss`/`aud` when
//! configured), then builds a [`Principal`]:
//! - `subject` ← the configured subject claim (default `sub`).
//! - `roles` ← the configured roles claim (default `roles`), accepting **either**
//!   a JSON array of strings **or** a space-delimited string (OIDC `scope`
//!   style).
//!
//! Unknown/extra claims are ignored. Any verification failure (expired, wrong
//! issuer/audience, bad signature, missing subject, malformed token) yields a
//! clear [`JwtError`] which the calling service maps to a `401`.

use std::collections::HashMap;

use jsonwebtoken::jwk::{AlgorithmParameters, Jwk, JwkSet};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde_json::Value;

use crate::principal::Principal;

/// The signing key material a [`JwtValidator`] verifies against.
///
/// `remux-authz` only ever *consumes* already-parsed keys — it never fetches or
/// pulls in an HTTP client. JWKS fetching lives in the services; they parse the
/// document via [`parse_jwks`] and construct [`JwtKey::Jwks`].
pub enum JwtKey {
    /// HMAC-SHA256 with a shared secret (symmetric).
    Hs256(Vec<u8>),
    /// RSA-SHA256 with a static PEM-encoded public key.
    Rs256(DecodingKey),
    /// ECDSA-P256-SHA256 with a static PEM-encoded public key.
    Es256(DecodingKey),
    /// A parsed JWKS: `kid → (algorithm, decoding key)`. The token's header
    /// `kid` selects the key; a token without a matching `kid` is rejected.
    Jwks(Jwks),
}

impl JwtKey {
    /// Build an HS256 key from a raw shared secret.
    pub fn hs256(secret: impl Into<Vec<u8>>) -> Self {
        JwtKey::Hs256(secret.into())
    }

    /// Build an RS256 key from a PEM-encoded RSA public key.
    pub fn rs256_pem(pem: &[u8]) -> Result<Self, JwtError> {
        DecodingKey::from_rsa_pem(pem)
            .map(JwtKey::Rs256)
            .map_err(|e| JwtError::Key(format!("invalid RSA public-key PEM: {e}")))
    }

    /// Build an ES256 key from a PEM-encoded EC (P-256) public key.
    pub fn es256_pem(pem: &[u8]) -> Result<Self, JwtError> {
        DecodingKey::from_ec_pem(pem)
            .map(JwtKey::Es256)
            .map_err(|e| JwtError::Key(format!("invalid EC public-key PEM: {e}")))
    }
}

/// A parsed JWKS: a map of `kid` → (algorithm, decoding key). Built from a JWKS
/// JSON document via [`parse_jwks`]; consumed by [`JwtKey::Jwks`].
#[derive(Clone, Default)]
pub struct Jwks {
    keys: HashMap<String, (Algorithm, DecodingKey)>,
}

impl Jwks {
    /// The number of keys in the set.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the set has no usable keys.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Look up a key by `kid`.
    fn get(&self, kid: &str) -> Option<&(Algorithm, DecodingKey)> {
        self.keys.get(kid)
    }
}

/// Parse a JWKS JSON document (`{"keys":[...]}`) into a [`Jwks`].
///
/// Each JWK that carries a `kid` and a supported algorithm (RSA → RS256, EC
/// P-256 → ES256) is added; keys without a `kid` or with an unsupported family
/// are skipped (logged to stderr). A document with no usable keys is an error so
/// a misconfiguration fails loudly rather than silently rejecting every token.
pub fn parse_jwks(doc: &str) -> Result<Jwks, JwtError> {
    let set: JwkSet = serde_json::from_str(doc)
        .map_err(|e| JwtError::Key(format!("invalid JWKS document: {e}")))?;
    let mut keys = HashMap::new();
    for jwk in &set.keys {
        match jwk_to_key(jwk) {
            Some((kid, alg, key)) => {
                keys.insert(kid, (alg, key));
            }
            None => {
                eprintln!(
                    "remux-authz::jwt: skipping JWKS entry (missing kid or unsupported algorithm)"
                );
            }
        }
    }
    if keys.is_empty() {
        return Err(JwtError::Key(
            "JWKS document contained no usable keys (need a kid + RSA/EC P-256 key)".to_string(),
        ));
    }
    Ok(Jwks { keys })
}

/// Convert a single JWK to `(kid, algorithm, decoding key)` if it is supported.
fn jwk_to_key(jwk: &Jwk) -> Option<(String, Algorithm, DecodingKey)> {
    let kid = jwk.common.key_id.clone()?;
    let (alg, key) = match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => (Algorithm::RS256, DecodingKey::from_jwk(jwk).ok()?),
        AlgorithmParameters::EllipticCurve(_) => {
            (Algorithm::ES256, DecodingKey::from_jwk(jwk).ok()?)
        }
        _ => return None,
    };
    Some((kid, alg, key))
}

/// Configuration for a [`JwtValidator`].
pub struct JwtConfig {
    /// Required token issuer (`iss`). When `Some`, a token whose `iss` differs is
    /// rejected. When `None`, `iss` is not checked.
    pub issuer: Option<String>,
    /// Required token audience (`aud`). When `Some`, a token whose `aud` does not
    /// contain this value is rejected. When `None`, `aud` is not checked.
    pub audience: Option<String>,
    /// The claim to read roles from (default `"roles"`). Accepts a JSON array of
    /// strings or a space-delimited string (OIDC `scope` style).
    pub roles_claim: String,
    /// The claim to read the subject from (default `"sub"`).
    pub subject_claim: String,
    /// The signing key material.
    pub key: JwtKey,
}

impl JwtConfig {
    /// Build a config with the default claim names (`sub` / `roles`) and the
    /// given key; issuer/audience unchecked.
    pub fn new(key: JwtKey) -> Self {
        Self {
            issuer: None,
            audience: None,
            roles_claim: "roles".to_string(),
            subject_claim: "sub".to_string(),
            key,
        }
    }

    /// Set the required issuer.
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    /// Set the required audience.
    pub fn with_audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = Some(audience.into());
        self
    }

    /// Set the roles claim name (default `"roles"`).
    pub fn with_roles_claim(mut self, claim: impl Into<String>) -> Self {
        self.roles_claim = claim.into();
        self
    }

    /// Set the subject claim name (default `"sub"`).
    pub fn with_subject_claim(mut self, claim: impl Into<String>) -> Self {
        self.subject_claim = claim.into();
        self
    }
}

/// An error validating a JWT. The calling service maps every variant to a `401`
/// (the token did not authenticate); the message distinguishes the cause for
/// logging.
#[derive(Debug, thiserror::Error)]
pub enum JwtError {
    /// The token's signature, `exp`, `iss`, or `aud` failed verification, or the
    /// token was malformed.
    #[error("JWT verification failed: {0}")]
    Verification(String),
    /// The configured roles claim was present but not a string or array of
    /// strings, or the subject claim was missing/empty.
    #[error("JWT claim mapping failed: {0}")]
    Claims(String),
    /// The JWKS did not contain a key matching the token header's `kid` (or the
    /// token carried no `kid`).
    #[error("no JWKS key matches the token's kid: {0}")]
    UnknownKid(String),
    /// A key (PEM/JWKS) could not be parsed at construction time.
    #[error("JWT key error: {0}")]
    Key(String),
}

/// A pure, offline JWT validator: verifies a token against configured key
/// material and maps its claims to a [`Principal`].
pub struct JwtValidator {
    config: JwtConfig,
}

impl JwtValidator {
    /// Build a validator from a [`JwtConfig`].
    pub fn new(config: JwtConfig) -> Self {
        Self { config }
    }

    /// Validate `token`: verify the signature + `exp` (+ `iss`/`aud` when
    /// configured), then build a [`Principal`] from the subject and roles claims.
    ///
    /// Returns a [`JwtError`] on any failure (the caller maps it to `401`).
    pub fn validate(&self, token: &str) -> Result<Principal, JwtError> {
        // Select the (algorithm, key) to verify with. For JWKS the token header's
        // `kid` selects the key; for the static keys the algorithm is fixed. The
        // HS256 secret is materialized into an owned `DecodingKey` held in
        // `owned_hs` so its borrow outlives the `decode` call below.
        let owned_hs;
        let (alg, key): (Algorithm, &DecodingKey) = match &self.config.key {
            JwtKey::Hs256(secret) => {
                owned_hs = DecodingKey::from_secret(secret);
                (Algorithm::HS256, &owned_hs)
            }
            JwtKey::Rs256(k) => (Algorithm::RS256, k),
            JwtKey::Es256(k) => (Algorithm::ES256, k),
            JwtKey::Jwks(set) => {
                let header = decode_header(token)
                    .map_err(|e| JwtError::Verification(format!("bad JWT header: {e}")))?;
                let kid = header
                    .kid
                    .ok_or_else(|| JwtError::UnknownKid("token has no kid".to_string()))?;
                let (alg, key) = set
                    .get(&kid)
                    .ok_or_else(|| JwtError::UnknownKid(kid.clone()))?;
                (*alg, key)
            }
        };

        let mut validation = Validation::new(alg);
        validation.validate_exp = true;
        // No clock-skew leeway: `exp` is enforced strictly (the default 60s
        // leeway would let a just-expired token through).
        validation.leeway = 0;
        if let Some(iss) = &self.config.issuer {
            validation.set_issuer(&[iss]);
        }
        if let Some(aud) = &self.config.audience {
            validation.set_audience(&[aud]);
        } else {
            // Without a configured audience, do not require the `aud` claim.
            validation.validate_aud = false;
        }
        // We always require `exp`; `sub` is checked explicitly below so we can
        // emit a precise error and honor a custom subject claim name.
        validation.set_required_spec_claims(&["exp"]);

        let data = decode::<HashMap<String, Value>>(token, key, &validation)
            .map_err(|e| JwtError::Verification(e.to_string()))?;
        self.claims_to_principal(&data.claims)
    }

    /// Map a verified claim set to a [`Principal`].
    fn claims_to_principal(&self, claims: &HashMap<String, Value>) -> Result<Principal, JwtError> {
        let subject = claims
            .get(&self.config.subject_claim)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                JwtError::Claims(format!(
                    "missing or empty subject claim {:?}",
                    self.config.subject_claim
                ))
            })?;
        let roles = match claims.get(&self.config.roles_claim) {
            Some(value) => parse_roles(value)?,
            // No roles claim → an authenticated principal with no roles (which,
            // deny-by-default, can do nothing until granted a role). This is not
            // an error: a valid identity with zero authority is legitimate.
            None => Vec::new(),
        };
        Ok(Principal::new(subject, roles))
    }
}

/// Parse a roles claim value: either a JSON array of strings, or a single
/// space-delimited string (OIDC `scope` style). Other shapes are an error.
fn parse_roles(value: &Value) -> Result<Vec<String>, JwtError> {
    match value {
        Value::Array(items) => {
            let mut roles = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(s) if !s.is_empty() => roles.push(s.to_string()),
                    Some(_) => {}
                    None => {
                        return Err(JwtError::Claims(
                            "roles claim array contained a non-string element".to_string(),
                        ))
                    }
                }
            }
            Ok(roles)
        }
        Value::String(s) => Ok(s.split_whitespace().map(str::to_string).collect()),
        _ => Err(JwtError::Claims(
            "roles claim must be an array of strings or a space-delimited string".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    /// Mint a JWT signed with `key`/`alg` from a JSON claims object.
    fn mint(alg: Algorithm, key: &EncodingKey, claims: &Value) -> String {
        let header = Header::new(alg);
        encode(&header, claims, key).expect("encode JWT")
    }

    fn mint_with_kid(alg: Algorithm, key: &EncodingKey, kid: &str, claims: &Value) -> String {
        let mut header = Header::new(alg);
        header.kid = Some(kid.to_string());
        encode(&header, claims, key).expect("encode JWT")
    }

    /// A unix timestamp `secs` in the future (or past, if negative).
    fn ts(offset_secs: i64) -> i64 {
        let now = jsonwebtoken::get_current_timestamp() as i64;
        now + offset_secs
    }

    const HS_SECRET: &[u8] = b"super-secret-test-key-0123456789";

    #[test]
    fn hs256_roles_array_maps_to_principal() {
        let token = mint(
            Algorithm::HS256,
            &EncodingKey::from_secret(HS_SECRET),
            &json!({ "sub": "alice", "roles": ["operator", "viewer"], "exp": ts(3600) }),
        );
        let v = JwtValidator::new(JwtConfig::new(JwtKey::hs256(HS_SECRET)));
        let p = v.validate(&token).expect("valid token");
        assert_eq!(p.subject, "alice");
        assert_eq!(p.roles, vec!["operator".to_string(), "viewer".to_string()]);
    }

    #[test]
    fn hs256_space_delimited_scope_maps_to_roles() {
        // OIDC scope-style: a single space-delimited string in the roles claim.
        let token = mint(
            Algorithm::HS256,
            &EncodingKey::from_secret(HS_SECRET),
            &json!({ "sub": "bob", "scope": "viewer operator", "exp": ts(3600) }),
        );
        let v =
            JwtValidator::new(JwtConfig::new(JwtKey::hs256(HS_SECRET)).with_roles_claim("scope"));
        let p = v.validate(&token).expect("valid token");
        assert_eq!(p.subject, "bob");
        assert_eq!(p.roles, vec!["viewer".to_string(), "operator".to_string()]);
    }

    #[test]
    fn custom_subject_claim_is_honored() {
        let token = mint(
            Algorithm::HS256,
            &EncodingKey::from_secret(HS_SECRET),
            &json!({ "email": "carol@x.io", "roles": ["viewer"], "exp": ts(3600) }),
        );
        let v =
            JwtValidator::new(JwtConfig::new(JwtKey::hs256(HS_SECRET)).with_subject_claim("email"));
        let p = v.validate(&token).expect("valid token");
        assert_eq!(p.subject, "carol@x.io");
    }

    #[test]
    fn expired_token_is_rejected() {
        let token = mint(
            Algorithm::HS256,
            &EncodingKey::from_secret(HS_SECRET),
            &json!({ "sub": "alice", "roles": ["viewer"], "exp": ts(-10) }),
        );
        let v = JwtValidator::new(JwtConfig::new(JwtKey::hs256(HS_SECRET)));
        let err = v.validate(&token).unwrap_err();
        assert!(matches!(err, JwtError::Verification(_)), "got {err:?}");
    }

    #[test]
    fn wrong_issuer_is_rejected() {
        let token = mint(
            Algorithm::HS256,
            &EncodingKey::from_secret(HS_SECRET),
            &json!({ "sub": "alice", "roles": ["viewer"], "iss": "evil", "exp": ts(3600) }),
        );
        let v = JwtValidator::new(
            JwtConfig::new(JwtKey::hs256(HS_SECRET)).with_issuer("https://good.example"),
        );
        assert!(matches!(
            v.validate(&token).unwrap_err(),
            JwtError::Verification(_)
        ));
    }

    #[test]
    fn correct_issuer_and_audience_pass() {
        let token = mint(
            Algorithm::HS256,
            &EncodingKey::from_secret(HS_SECRET),
            &json!({
                "sub": "alice", "roles": ["viewer"],
                "iss": "https://good.example", "aud": "remux",
                "exp": ts(3600)
            }),
        );
        let v = JwtValidator::new(
            JwtConfig::new(JwtKey::hs256(HS_SECRET))
                .with_issuer("https://good.example")
                .with_audience("remux"),
        );
        let p = v.validate(&token).expect("valid");
        assert_eq!(p.subject, "alice");
    }

    #[test]
    fn wrong_audience_is_rejected() {
        let token = mint(
            Algorithm::HS256,
            &EncodingKey::from_secret(HS_SECRET),
            &json!({ "sub": "alice", "roles": ["viewer"], "aud": "other", "exp": ts(3600) }),
        );
        let v = JwtValidator::new(JwtConfig::new(JwtKey::hs256(HS_SECRET)).with_audience("remux"));
        assert!(matches!(
            v.validate(&token).unwrap_err(),
            JwtError::Verification(_)
        ));
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let token = mint(
            Algorithm::HS256,
            &EncodingKey::from_secret(HS_SECRET),
            &json!({ "sub": "alice", "roles": ["viewer"], "exp": ts(3600) }),
        );
        // Flip the last char of the signature segment.
        let mut parts: Vec<&str> = token.split('.').collect();
        let sig = parts.pop().unwrap();
        let mut tampered_sig: Vec<char> = sig.chars().collect();
        let last = tampered_sig.len() - 1;
        tampered_sig[last] = if tampered_sig[last] == 'A' { 'B' } else { 'A' };
        let tampered_sig: String = tampered_sig.into_iter().collect();
        let tampered = format!("{}.{}.{}", parts[0], parts[1], tampered_sig);
        let v = JwtValidator::new(JwtConfig::new(JwtKey::hs256(HS_SECRET)));
        assert!(v.validate(&tampered).is_err());
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let token = mint(
            Algorithm::HS256,
            &EncodingKey::from_secret(HS_SECRET),
            &json!({ "sub": "alice", "roles": ["viewer"], "exp": ts(3600) }),
        );
        let v = JwtValidator::new(JwtConfig::new(JwtKey::hs256(
            b"a-different-secret-entirely!!",
        )));
        assert!(matches!(
            v.validate(&token).unwrap_err(),
            JwtError::Verification(_)
        ));
    }

    #[test]
    fn missing_subject_is_rejected() {
        let token = mint(
            Algorithm::HS256,
            &EncodingKey::from_secret(HS_SECRET),
            &json!({ "roles": ["viewer"], "exp": ts(3600) }),
        );
        let v = JwtValidator::new(JwtConfig::new(JwtKey::hs256(HS_SECRET)));
        assert!(matches!(
            v.validate(&token).unwrap_err(),
            JwtError::Claims(_)
        ));
    }

    #[test]
    fn no_roles_claim_yields_empty_roles() {
        let token = mint(
            Algorithm::HS256,
            &EncodingKey::from_secret(HS_SECRET),
            &json!({ "sub": "alice", "exp": ts(3600) }),
        );
        let v = JwtValidator::new(JwtConfig::new(JwtKey::hs256(HS_SECRET)));
        let p = v.validate(&token).expect("valid");
        assert!(p.roles.is_empty());
    }

    #[test]
    fn rs256_with_generated_keypair() {
        // Generate an RSA keypair at test time via rcgen (already a workspace dep
        // for the gateway, but here we use a committed test PEM pair instead to
        // stay dependency-light). We mint with the private key and validate with
        // the public key.
        let (priv_pem, pub_pem) = test_rsa_keypair();
        let token = mint(
            Algorithm::RS256,
            &EncodingKey::from_rsa_pem(priv_pem.as_bytes()).expect("enc key"),
            &json!({ "sub": "rsa-user", "roles": ["admin"], "exp": ts(3600) }),
        );
        let key = JwtKey::rs256_pem(pub_pem.as_bytes()).expect("pub key");
        let v = JwtValidator::new(JwtConfig::new(key));
        let p = v.validate(&token).expect("valid RS256 token");
        assert_eq!(p.subject, "rsa-user");
        assert_eq!(p.roles, vec!["admin".to_string()]);
    }

    #[test]
    fn jwks_selects_key_by_kid_and_validates() {
        let (priv_pem, pub_pem) = test_rsa_keypair();
        // Build a JWKS containing the public key under a known kid, by converting
        // the PEM's RSA modulus/exponent. Easiest path: construct a JWKS JSON from
        // the public key using rsa-pem→JWK is non-trivial without extra deps, so
        // instead we exercise parse_jwks via a real JWKS JSON fixture.
        let jwks_json = rsa_pub_pem_to_jwks_json(&pub_pem, "test-kid-1");
        let jwks = parse_jwks(&jwks_json).expect("parse jwks");
        assert_eq!(jwks.len(), 1);
        let token = mint_with_kid(
            Algorithm::RS256,
            &EncodingKey::from_rsa_pem(priv_pem.as_bytes()).expect("enc key"),
            "test-kid-1",
            &json!({ "sub": "jwks-user", "roles": ["operator"], "exp": ts(3600) }),
        );
        let v = JwtValidator::new(JwtConfig::new(JwtKey::Jwks(jwks)));
        let p = v.validate(&token).expect("valid JWKS token");
        assert_eq!(p.subject, "jwks-user");
        assert_eq!(p.roles, vec!["operator".to_string()]);
    }

    #[test]
    fn jwks_unknown_kid_is_rejected() {
        let (priv_pem, pub_pem) = test_rsa_keypair();
        let jwks_json = rsa_pub_pem_to_jwks_json(&pub_pem, "known-kid");
        let jwks = parse_jwks(&jwks_json).expect("parse jwks");
        let token = mint_with_kid(
            Algorithm::RS256,
            &EncodingKey::from_rsa_pem(priv_pem.as_bytes()).expect("enc key"),
            "some-other-kid",
            &json!({ "sub": "u", "roles": [], "exp": ts(3600) }),
        );
        let v = JwtValidator::new(JwtConfig::new(JwtKey::Jwks(jwks)));
        assert!(matches!(
            v.validate(&token).unwrap_err(),
            JwtError::UnknownKid(_)
        ));
    }

    #[test]
    fn parse_jwks_rejects_empty() {
        assert!(parse_jwks(r#"{"keys":[]}"#).is_err());
        assert!(parse_jwks("not json").is_err());
    }

    #[test]
    fn parse_roles_shapes() {
        assert_eq!(
            parse_roles(&json!(["a", "b"])).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(
            parse_roles(&json!("a b  c")).unwrap(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(parse_roles(&json!("")).unwrap().is_empty());
        assert!(parse_roles(&json!([1, 2])).is_err());
        assert!(parse_roles(&json!(42)).is_err());
    }

    // --- test key material helpers -----------------------------------------

    /// A deterministic RSA-2048 keypair generated once per test run via the
    /// `rsa` crate is heavy; instead we ship a fixed 2048-bit test keypair (PEM)
    /// here. These are TEST-ONLY keys, never used outside the test binary.
    fn test_rsa_keypair() -> (String, String) {
        (TEST_RSA_PRIV_PEM.to_string(), TEST_RSA_PUB_PEM.to_string())
    }

    /// Convert an RSA public-key PEM to a single-key JWKS JSON document with the
    /// given `kid`. Uses `jsonwebtoken`'s own machinery: parse the PEM into a
    /// `DecodingKey` is opaque, so instead we read the modulus/exponent from the
    /// committed JWK fixture (kept in lockstep with the PEM below).
    fn rsa_pub_pem_to_jwks_json(_pub_pem: &str, kid: &str) -> String {
        format!(
            r#"{{"keys":[{{"kty":"RSA","use":"sig","alg":"RS256","kid":"{kid}","n":"{n}","e":"AQAB"}}]}}"#,
            n = TEST_RSA_MODULUS_B64URL,
        )
    }

    // A fixed RSA-2048 test keypair (PKCS#8 PEM) + the modulus in base64url for
    // the JWKS fixture. Generated once with OpenSSL; TEST-ONLY.
    const TEST_RSA_PRIV_PEM: &str = include_str!("../testdata/rsa_priv.pem");
    const TEST_RSA_PUB_PEM: &str = include_str!("../testdata/rsa_pub.pem");
    const TEST_RSA_MODULUS_B64URL: &str = include_str!("../testdata/rsa_modulus.b64url");
}
