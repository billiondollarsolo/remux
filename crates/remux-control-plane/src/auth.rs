//! Bearer-token auth + **principal + RBAC** authorization for the control plane
//! (Phase A), mirroring the gateway's posture via the shared [`remux_authz`]
//! model.
//!
//! A presented token resolves (in **constant time**) to a [`Principal`] whose
//! roles are evaluated against a [`Policy`]. Each route declares a required
//! [`Permission`]: a missing/unknown token is `401`, a known principal lacking
//! the permission is `403`.
//!
//! Back-compat: `--token` (the admin/fleet token) maps to principal
//! `{subject:"fleet-admin", roles:["fleet-admin"]}`; `--register-token` maps to
//! `{subject:"registrar", roles:["registrar"]}`. An optional `--auth-config`
//! file adds further principal-shaped tokens and custom roles. Tokens are never
//! logged in the clear; the audit line logs the subject, roles, and a short,
//! non-reversible id.

use std::path::Path;
use std::sync::Arc;

pub use remux_authz::{
    audit_id_for, bearer_from_header, load_auth_config, permits, AuthConfigError, Permission,
    Policy, Principal, TokenStore,
};

/// The control plane's resolved auth state: the token→principal store and the
/// policy its principals' roles are evaluated against. Cheap to clone.
#[derive(Clone)]
pub struct AuthConfig {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    store: TokenStore,
    policy: Policy,
}

impl AuthConfig {
    /// Build an auth config from the back-compat tokens: the admin (`--token`)
    /// token maps to the `fleet-admin` role; the register (`--register-token`)
    /// token maps to the `registrar` role. An empty register token (or one equal
    /// to the admin token) is ignored.
    pub fn new(admin: String, register: String) -> Self {
        let policy = Policy::builtin();
        let mut store = TokenStore::new();
        store.insert(
            admin.clone(),
            Principal::new("fleet-admin", ["fleet-admin".to_string()]),
        );
        if !register.is_empty() && register != admin {
            store.insert(
                register,
                Principal::new("registrar", ["registrar".to_string()]),
            );
        }
        Self {
            inner: Arc::new(AuthInner { store, policy }),
        }
    }

    /// Build an auth config from the back-compat tokens PLUS an optional
    /// auth-config file. The file's custom roles are merged over the built-ins;
    /// its `[[tokens]]` are registered after the back-compat tokens (so a flag
    /// token wins a duplicate-secret collision).
    pub fn from_flags_and_config(
        admin: String,
        register: String,
        config_path: Option<&Path>,
    ) -> Result<Self, AuthConfigError> {
        let base = Self::new(admin, register);
        let Some(path) = config_path else {
            return Ok(base);
        };
        let (file_policy, pairs) = load_auth_config(path)?;
        let mut store = base.inner.store.clone();
        for (token, principal) in pairs {
            store.insert(token, principal);
        }
        Ok(Self {
            inner: Arc::new(AuthInner {
                store,
                policy: file_policy,
            }),
        })
    }

    /// Resolve a presented bearer token to its [`Principal`] (constant-time), or
    /// `None` if it matches no configured token (the caller turns that into a
    /// `401`).
    pub fn resolve(&self, presented: &str) -> Option<&Principal> {
        self.inner.store.resolve(presented)
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
    fn admin_maps_to_fleet_admin_register_to_registrar() {
        let cfg = AuthConfig::new("admin-tok".into(), "reg-tok".into());

        let admin = cfg.resolve("admin-tok").expect("admin").clone();
        assert_eq!(admin.subject, "fleet-admin");
        // fleet-admin has every CP permission.
        for perm in [
            Permission::FleetHostsRead,
            Permission::FleetSessionsRead,
            Permission::FleetResolve,
            Permission::HostRegister,
        ] {
            assert!(
                cfg.permits(&admin, perm),
                "fleet-admin should permit {perm}"
            );
        }

        let reg = cfg.resolve("reg-tok").expect("registrar").clone();
        assert_eq!(reg.subject, "registrar");
        // registrar can only register/heartbeat/deregister.
        assert!(cfg.permits(&reg, Permission::HostRegister));
        assert!(!cfg.permits(&reg, Permission::FleetHostsRead));
        assert!(!cfg.permits(&reg, Permission::FleetResolve));

        assert!(cfg.resolve("nope").is_none());
    }

    #[test]
    fn register_equal_to_admin_is_ignored() {
        let cfg = AuthConfig::new("same".into(), "same".into());
        assert_eq!(cfg.token_count(), 1);
        assert_eq!(cfg.resolve("same").unwrap().subject, "fleet-admin");
    }

    #[test]
    fn config_file_adds_fleet_viewer_token() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("remux-cp-auth-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
                [[tokens]]
                token = "viewer-token"
                subject = "dash"
                roles = ["fleet-viewer"]
            "#,
        )
        .unwrap();
        let cfg =
            AuthConfig::from_flags_and_config("admin".into(), "reg".into(), Some(path.as_path()))
                .unwrap();
        let _ = std::fs::remove_file(&path);

        let v = cfg.resolve("viewer-token").expect("fleet-viewer").clone();
        assert!(cfg.permits(&v, Permission::FleetHostsRead));
        assert!(cfg.permits(&v, Permission::FleetSessionsRead));
        // fleet-viewer cannot resolve.
        assert!(!cfg.permits(&v, Permission::FleetResolve));
    }
}
