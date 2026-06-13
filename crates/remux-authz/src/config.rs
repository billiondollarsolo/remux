//! The shared auth-config file format (serde/TOML) used by both services, and
//! [`load_auth_config`] which parses it into a [`Policy`] (built-ins merged with
//! custom roles) plus the `(token, Principal)` pairs to seed a [`TokenStore`].
//!
//! Format:
//! ```toml
//! [[tokens]]
//! token = "…"
//! subject = "ci-bot"
//! roles = ["operator"]
//!
//! [[roles]]               # optional custom roles
//! name = "deployer"
//! permissions = ["session.create", "session.input", "session.read"]
//! ```
//!
//! Custom roles are merged **over** the built-ins: a custom role with a built-in
//! name overrides it; new names are added.

use std::path::Path;

use serde::Deserialize;

use crate::permission::Permission;
use crate::policy::{Policy, Role};
use crate::principal::Principal;

/// The on-disk auth-config document.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfigFile {
    /// Token → principal entries.
    #[serde(default)]
    pub tokens: Vec<TokenEntry>,
    /// Optional custom role definitions (merged over the built-ins).
    #[serde(default)]
    pub roles: Vec<RoleEntry>,
}

/// One `[[tokens]]` entry: a bearer token bound to a subject and its roles.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenEntry {
    /// The bearer token secret.
    pub token: String,
    /// The subject id this token authenticates as.
    pub subject: String,
    /// The role names this token's principal holds.
    #[serde(default)]
    pub roles: Vec<String>,
}

/// One `[[roles]]` entry: a custom role and its permission names.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleEntry {
    /// The custom role's name.
    pub name: String,
    /// The permission names this role grants (parsed via [`Permission::from_str`]).
    pub permissions: Vec<Permission>,
}

/// An error loading or parsing an auth-config file.
#[derive(Debug, thiserror::Error)]
pub enum AuthConfigError {
    /// The file could not be read.
    #[error("failed to read auth config {path:?}: {source}")]
    Io {
        /// The path that failed to read.
        path: String,
        /// The underlying IO error.
        source: std::io::Error,
    },
    /// The file was not valid TOML or referenced an unknown permission name.
    #[error("failed to parse auth config {path:?}: {source}")]
    Parse {
        /// The path that failed to parse.
        path: String,
        /// The underlying TOML/deserialization error.
        source: toml::de::Error,
    },
}

impl AuthConfigFile {
    /// Parse an auth-config document from a TOML string.
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Resolve this document into a [`Policy`] (built-ins merged with the custom
    /// roles) and the `(token, Principal)` pairs.
    pub fn resolve(&self) -> (Policy, Vec<(String, Principal)>) {
        let mut policy = Policy::builtin();
        for entry in &self.roles {
            policy.insert(Role::new(
                entry.name.clone(),
                entry.permissions.iter().copied(),
            ));
        }
        let tokens = self
            .tokens
            .iter()
            .map(|t| {
                (
                    t.token.clone(),
                    Principal::new(t.subject.clone(), t.roles.clone()),
                )
            })
            .collect();
        (policy, tokens)
    }
}

/// Load an auth-config file from `path`, returning the merged [`Policy`] and the
/// `(token, Principal)` pairs to register in a [`crate::TokenStore`].
///
/// Custom roles are merged **over** the built-ins (override by name, add by new
/// name). The built-in policy is always the base, so a config that defines no
/// roles still yields the full set of built-in roles.
pub fn load_auth_config(
    path: impl AsRef<Path>,
) -> Result<(Policy, Vec<(String, Principal)>), AuthConfigError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| AuthConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let doc = AuthConfigFile::from_toml(&text).map_err(|source| AuthConfigError::Parse {
        path: path.display().to_string(),
        source,
    })?;
    Ok(doc.resolve())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::principal::permits;

    #[test]
    fn parses_tokens_and_custom_roles() {
        let toml = r#"
            [[tokens]]
            token = "ci-secret"
            subject = "ci-bot"
            roles = ["operator"]

            [[tokens]]
            token = "dep-secret"
            subject = "deploy-bot"
            roles = ["deployer"]

            [[roles]]
            name = "deployer"
            permissions = ["session.create", "session.input", "session.read"]
        "#;
        let doc = AuthConfigFile::from_toml(toml).unwrap();
        let (policy, tokens) = doc.resolve();

        // Custom role merged over builtins; builtins still present.
        assert!(policy.contains("deployer"));
        assert!(policy.contains("operator"));
        let deployer = policy.get("deployer").unwrap();
        assert!(deployer.permissions.contains(&Permission::SessionCreate));
        assert!(deployer.permissions.contains(&Permission::SessionInput));
        assert!(!deployer.permissions.contains(&Permission::SessionKill));

        // Tokens map to principals.
        assert_eq!(tokens.len(), 2);
        let (tok, princ) = &tokens[0];
        assert_eq!(tok, "ci-secret");
        assert_eq!(princ.subject, "ci-bot");
        assert_eq!(princ.roles, vec!["operator".to_string()]);

        // The deployer principal can create+input+read but not kill (via policy).
        let dep = &tokens[1].1;
        assert!(permits(&policy, dep, Permission::SessionCreate));
        assert!(permits(&policy, dep, Permission::SessionInput));
        assert!(permits(&policy, dep, Permission::SessionRead));
        assert!(!permits(&policy, dep, Permission::SessionKill));
    }

    #[test]
    fn custom_role_overrides_builtin_by_name() {
        let toml = r#"
            [[roles]]
            name = "viewer"
            permissions = ["session.list"]
        "#;
        let (policy, _) = AuthConfigFile::from_toml(toml).unwrap().resolve();
        let viewer = policy.get("viewer").unwrap();
        assert_eq!(viewer.permissions.len(), 1);
        assert!(viewer.permissions.contains(&Permission::SessionList));
    }

    #[test]
    fn empty_config_yields_only_builtins() {
        let (policy, tokens) = AuthConfigFile::default().resolve();
        assert!(tokens.is_empty());
        assert_eq!(policy.len(), 7);
        assert!(policy.contains("admin"));
    }

    #[test]
    fn unknown_permission_name_is_a_parse_error() {
        let toml = r#"
            [[roles]]
            name = "bad"
            permissions = ["session.read", "session.nope"]
        "#;
        assert!(AuthConfigFile::from_toml(toml).is_err());
    }

    #[test]
    fn unknown_field_is_rejected() {
        let toml = r#"
            [[tokens]]
            token = "t"
            subject = "s"
            roles = []
            extra = "nope"
        "#;
        assert!(AuthConfigFile::from_toml(toml).is_err());
    }

    #[test]
    fn load_auth_config_reads_a_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("remux-authz-test-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
                [[tokens]]
                token = "filetok"
                subject = "file-bot"
                roles = ["viewer"]
            "#,
        )
        .unwrap();
        let (policy, tokens) = load_auth_config(&path).unwrap();
        assert!(policy.contains("viewer"));
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].0, "filetok");
        let _ = std::fs::remove_file(&path);

        // Missing file is an Io error.
        assert!(matches!(
            load_auth_config(dir.join("does-not-exist-remux.toml")),
            Err(AuthConfigError::Io { .. })
        ));
    }
}
