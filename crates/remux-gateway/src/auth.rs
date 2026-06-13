//! Bearer-token authentication and **principal + RBAC** authorization for the
//! gateway (AW4, Phase A).
//!
//! Deny-by-default: every `/v1/*` route except `GET /v1/health` and
//! `GET /v1/openapi.json` requires a valid bearer token. For REST and WebSocket
//! routes the token may arrive in the `Authorization: Bearer <token>` header;
//! WebSocket routes ADDITIONALLY accept `?token=<token>` (browsers cannot set
//! `Authorization` on a WS handshake).
//!
//! The model now lives in [`remux_authz`]: a presented token resolves (in
//! **constant time**) to a [`Principal`], whose roles are evaluated against a
//! [`Policy`]. Each route declares a required [`Permission`]; an unknown/missing
//! token is `401`, a known token whose principal lacks the permission is `403`.
//!
//! Back-compat: `--token` maps to principal `{subject:"admin", roles:["admin"]}`
//! and `--read-token` to `{subject:"reader", roles:["viewer"]}`, preserving the
//! old read-write / read-only two-token behaviour on top of the new model. An
//! optional `--auth-config` file adds further principal-shaped tokens and custom
//! roles.
//!
//! Tokens are never logged in the clear; the audit line logs the subject, roles,
//! and a short non-reversible token id.

use std::path::Path;
use std::sync::Arc;

pub use remux_authz::{
    audit_id_for, bearer_from_header, load_auth_config, permits, AuthConfigError, Permission,
    Policy, Principal, TokenStore,
};

use crate::jwt_service::{AuthMethod, JwtAuth};

/// The gateway's resolved auth state: the token→principal store and the policy
/// the principals' roles are evaluated against. Cheap to clone (shared behind an
/// `Arc`) and handed into the axum app state.
#[derive(Clone)]
pub struct AuthConfig {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    store: TokenStore,
    policy: Policy,
    /// Phase B: an optional JWT/OIDC validator. When present, a presented bearer
    /// that misses the static [`TokenStore`] is tried as a JWT.
    jwt: Option<JwtAuth>,
}

impl AuthConfig {
    /// Build an auth config with a single admin token (back-compat `--token`).
    /// The token maps to `{subject:"admin", roles:["admin"]}`.
    pub fn new(admin_token: String) -> Self {
        Self::with_scopes(admin_token, None)
    }

    /// Build an auth config from the back-compat flags: the admin (`--token`)
    /// token maps to the `admin` role; an optional read-only (`--read-token`)
    /// token maps to the `viewer` role. A read-only token equal to the admin
    /// token is ignored (the admin mapping wins, granting the broader role).
    pub fn with_scopes(admin_token: String, read_token: Option<String>) -> Self {
        let policy = Policy::builtin();
        let mut store = TokenStore::new();
        store.insert(
            admin_token.clone(),
            Principal::new("admin", ["admin".to_string()]),
        );
        if let Some(ro) = read_token.filter(|t| !t.is_empty() && *t != admin_token) {
            store.insert(ro, Principal::new("reader", ["viewer".to_string()]));
        }
        Self {
            inner: Arc::new(AuthInner {
                store,
                policy,
                jwt: None,
            }),
        }
    }

    /// Build an auth config from the back-compat flags PLUS an optional
    /// auth-config file. The file's custom roles are merged over the built-ins
    /// and its `[[tokens]]` are registered after the back-compat flags (so a
    /// flag token wins a duplicate-secret collision).
    pub fn from_flags_and_config(
        admin_token: String,
        read_token: Option<String>,
        config_path: Option<&Path>,
    ) -> Result<Self, AuthConfigError> {
        let base = Self::with_scopes(admin_token, read_token);
        let Some(path) = config_path else {
            return Ok(base);
        };
        let (file_policy, pairs) = load_auth_config(path)?;
        // Start from the back-compat store/policy, layer the file on top.
        let mut store = base.inner.store.clone();
        for (token, principal) in pairs {
            store.insert(token, principal);
        }
        Ok(Self {
            inner: Arc::new(AuthInner {
                store,
                policy: file_policy,
                jwt: None,
            }),
        })
    }

    /// Attach a JWT/OIDC validator (Phase B). A presented bearer that misses the
    /// static [`TokenStore`] is then tried as a JWT; whichever yields a
    /// [`Principal`] flows through the same RBAC `permits` decision.
    pub fn with_jwt(self, jwt: Option<JwtAuth>) -> Self {
        let inner = self.inner;
        Self {
            inner: Arc::new(AuthInner {
                store: inner.store.clone(),
                policy: inner.policy.clone(),
                jwt,
            }),
        }
    }

    /// Resolve a presented bearer token to its [`Principal`] (constant-time), or
    /// `None` if it matches no configured token (the caller turns that into a
    /// `401`).
    ///
    /// This is the **static-token-only** resolve; [`AuthConfig::authenticate`] is
    /// the full static-then-JWT resolution that also reports the [`AuthMethod`].
    pub fn resolve(&self, presented: &str) -> Option<&Principal> {
        self.inner.store.resolve(presented)
    }

    /// Resolve a presented bearer to a [`Principal`] and the [`AuthMethod`] that
    /// produced it. The static [`TokenStore`] is tried FIRST (cheap, constant
    /// time); only on a miss, and only if a JWT validator is configured, is the
    /// bearer validated as a JWT. `None` means neither matched (caller → `401`).
    pub fn authenticate(&self, presented: &str) -> Option<(Principal, AuthMethod)> {
        if let Some(p) = self.inner.store.resolve(presented) {
            return Some((p.clone(), AuthMethod::Static));
        }
        if let Some(jwt) = &self.inner.jwt {
            match jwt.validate(presented) {
                Ok(p) => return Some((p, AuthMethod::Jwt)),
                Err(e) => {
                    // A presented bearer that is neither a known static token nor
                    // a valid JWT → unauthenticated (401). Log at debug; never log
                    // the token itself.
                    tracing::debug!(error = %e, "JWT validation failed for presented bearer");
                }
            }
        }
        None
    }

    /// Whether `principal` may exercise `perm` under this config's policy.
    pub fn permits(&self, principal: &Principal, perm: Permission) -> bool {
        permits(&self.inner.policy, principal, perm)
    }

    /// The number of configured tokens (for startup logging).
    pub fn token_count(&self) -> usize {
        self.inner.store.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_token_maps_to_admin_role() {
        let cfg = AuthConfig::new("rw".to_string());
        let p = cfg.resolve("rw").expect("admin principal");
        assert_eq!(p.subject, "admin");
        assert!(cfg.permits(p, Permission::SessionInput));
        assert!(cfg.permits(p, Permission::SessionKill));
        assert!(cfg.resolve("nope").is_none());
    }

    #[test]
    fn read_token_maps_to_viewer_role() {
        let cfg = AuthConfig::with_scopes("rw".to_string(), Some("ro".to_string()));
        let reader = cfg.resolve("ro").expect("reader principal").clone();
        assert_eq!(reader.subject, "reader");
        // viewer can read but not write.
        assert!(cfg.permits(&reader, Permission::SessionRead));
        assert!(cfg.permits(&reader, Permission::EventsRead));
        assert!(!cfg.permits(&reader, Permission::SessionInput));
        assert!(!cfg.permits(&reader, Permission::SessionStream));
        // admin still has everything.
        let admin = cfg.resolve("rw").unwrap().clone();
        assert!(cfg.permits(&admin, Permission::SessionInput));
    }

    #[test]
    fn read_token_equal_to_admin_is_ignored() {
        let cfg = AuthConfig::with_scopes("same".to_string(), Some("same".to_string()));
        assert_eq!(cfg.token_count(), 1);
        assert_eq!(cfg.resolve("same").unwrap().subject, "admin");
    }

    #[test]
    fn empty_read_token_is_ignored() {
        let cfg = AuthConfig::with_scopes("rw".to_string(), Some(String::new()));
        assert_eq!(cfg.token_count(), 1);
    }

    #[test]
    fn config_file_adds_custom_role_tokens() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("remux-gw-auth-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
                [[tokens]]
                token = "dep-token"
                subject = "deployer-bot"
                roles = ["deployer"]

                [[roles]]
                name = "deployer"
                permissions = ["session.create", "session.input", "session.read"]
            "#,
        )
        .unwrap();
        let cfg = AuthConfig::from_flags_and_config("rw".to_string(), None, Some(path.as_path()))
            .unwrap();
        let _ = std::fs::remove_file(&path);

        // Back-compat admin token still works.
        let admin = cfg.resolve("rw").unwrap().clone();
        assert!(cfg.permits(&admin, Permission::SessionKill));

        // Custom deployer can create+input but not kill.
        let dep = cfg.resolve("dep-token").expect("deployer").clone();
        assert!(cfg.permits(&dep, Permission::SessionCreate));
        assert!(cfg.permits(&dep, Permission::SessionInput));
        assert!(cfg.permits(&dep, Permission::SessionRead));
        assert!(!cfg.permits(&dep, Permission::SessionKill));
    }
}
