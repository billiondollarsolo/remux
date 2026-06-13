//! Phase C — the **pure** mTLS identity layer: map a client certificate's
//! identity (its CN, or first SAN) to a [`Principal`] via an operator-provided
//! mapping, with a configurable default-role fallback for unmapped-but-valid
//! certs.
//!
//! This stays in `remux-authz` precisely because it is pure: it does *no* TLS,
//! no X.509 parsing, no network. The transport-specific part — completing the
//! TLS handshake and extracting the subject CN / SAN out of the peer's DER
//! certificate — lives in the services (`remux_gateway::mtls`). Here we only turn
//! an already-extracted identity string into a [`Principal`], exactly as the JWT
//! and static-token resolvers do, so the `Policy`/`permits` decision and audit
//! shape are unchanged.
//!
//! Mapping file format (TOML):
//! ```toml
//! [[identities]]
//! subject = "ops-laptop"      # matches the cert CN or first SAN
//! roles   = ["operator"]
//!
//! [[identities]]
//! subject = "ci.example.com"
//! roles   = ["viewer"]
//! ```
//!
//! A cert whose identity is not listed gets the configured **default roles**
//! (none by default → it authenticates but `permits()` denies every route, i.e.
//! `403`, until an operator maps it).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::principal::Principal;

/// One `[[identities]]` mapping entry: a cert identity (CN or SAN) → role names.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityEntry {
    /// The certificate identity this entry matches (the cert's CN, or its first
    /// SAN when there is no CN).
    pub subject: String,
    /// The role names a cert with this identity is granted.
    #[serde(default)]
    pub roles: Vec<String>,
}

/// The on-disk mTLS identity-mapping document (`--mtls-identities <TOML>`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MtlsIdentityFile {
    /// The identity → roles entries.
    #[serde(default)]
    pub identities: Vec<IdentityEntry>,
}

/// An error loading or parsing an mTLS identity-mapping file.
#[derive(Debug, thiserror::Error)]
pub enum MtlsIdentityError {
    /// The file could not be read.
    #[error("failed to read mtls identities {path:?}: {source}")]
    Io {
        /// The path that failed to read.
        path: String,
        /// The underlying IO error.
        source: std::io::Error,
    },
    /// The file was not valid TOML.
    #[error("failed to parse mtls identities {path:?}: {source}")]
    Parse {
        /// The path that failed to parse.
        path: String,
        /// The underlying TOML error.
        source: toml::de::Error,
    },
}

/// A resolved cert-identity → roles map plus the default roles for unmapped certs.
/// Cheap to clone; built once at startup and consulted per mTLS request.
#[derive(Debug, Clone, Default)]
pub struct MtlsIdentities {
    by_subject: BTreeMap<String, Vec<String>>,
    default_roles: Vec<String>,
}

impl MtlsIdentities {
    /// Build from an explicit identity map and default roles.
    pub fn new(
        entries: impl IntoIterator<Item = (String, Vec<String>)>,
        default_roles: Vec<String>,
    ) -> Self {
        Self {
            by_subject: entries.into_iter().collect(),
            default_roles,
        }
    }

    /// Build from a parsed [`MtlsIdentityFile`] and the default roles.
    pub fn from_file(file: &MtlsIdentityFile, default_roles: Vec<String>) -> Self {
        let by_subject = file
            .identities
            .iter()
            .map(|e| (e.subject.clone(), e.roles.clone()))
            .collect();
        Self {
            by_subject,
            default_roles,
        }
    }

    /// The default roles applied to an unmapped (but cryptographically valid) cert.
    pub fn default_roles(&self) -> &[String] {
        &self.default_roles
    }

    /// The number of mapped identities.
    pub fn len(&self) -> usize {
        self.by_subject.len()
    }

    /// Whether the identity map is empty (no explicit mappings).
    pub fn is_empty(&self) -> bool {
        self.by_subject.is_empty()
    }

    /// Resolve a (already-extracted) certificate `identity` (its CN, or first SAN)
    /// to a [`Principal`]. A mapped identity gets its configured roles; an unmapped
    /// one gets the default roles (possibly empty → authenticated but unauthorised
    /// until mapped). The `subject` is always the cert identity itself (for audit).
    pub fn principal_for(&self, identity: &str) -> Principal {
        let roles = self
            .by_subject
            .get(identity)
            .cloned()
            .unwrap_or_else(|| self.default_roles.clone());
        Principal::new(identity.to_string(), roles)
    }
}

/// Load an mTLS identity-mapping file from `path` and combine it with
/// `default_roles` into [`MtlsIdentities`].
pub fn load_mtls_identities(
    path: impl AsRef<Path>,
    default_roles: Vec<String>,
) -> Result<MtlsIdentities, MtlsIdentityError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path).map_err(|source| MtlsIdentityError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let file: MtlsIdentityFile =
        toml::from_str(&text).map_err(|source| MtlsIdentityError::Parse {
            path: path.display().to_string(),
            source,
        })?;
    Ok(MtlsIdentities::from_file(&file, default_roles))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_identity_gets_its_roles() {
        let ids = MtlsIdentities::new([("ops".to_string(), vec!["operator".to_string()])], vec![]);
        let p = ids.principal_for("ops");
        assert_eq!(p.subject, "ops");
        assert_eq!(p.roles, vec!["operator".to_string()]);
    }

    #[test]
    fn unmapped_identity_gets_default_roles() {
        let ids = MtlsIdentities::new([], vec!["viewer".to_string()]);
        let p = ids.principal_for("stranger");
        assert_eq!(p.subject, "stranger");
        assert_eq!(p.roles, vec!["viewer".to_string()]);
    }

    #[test]
    fn unmapped_with_no_default_has_no_roles() {
        let ids = MtlsIdentities::default();
        let p = ids.principal_for("nobody");
        assert_eq!(p.subject, "nobody");
        assert!(p.roles.is_empty());
    }

    #[test]
    fn parses_identity_file() {
        let toml = r#"
            [[identities]]
            subject = "ops-laptop"
            roles = ["operator"]

            [[identities]]
            subject = "ci.example.com"
            roles = ["viewer"]
        "#;
        let file: MtlsIdentityFile = toml::from_str(toml).unwrap();
        let ids = MtlsIdentities::from_file(&file, vec![]);
        assert_eq!(ids.len(), 2);
        assert_eq!(
            ids.principal_for("ops-laptop").roles,
            vec!["operator".to_string()]
        );
        assert_eq!(
            ids.principal_for("ci.example.com").roles,
            vec!["viewer".to_string()]
        );
        assert!(ids.principal_for("unknown").roles.is_empty());
    }

    #[test]
    fn unknown_field_rejected() {
        let toml = r#"
            [[identities]]
            subject = "x"
            roles = []
            extra = "no"
        "#;
        assert!(toml::from_str::<MtlsIdentityFile>(toml).is_err());
    }

    #[test]
    fn load_reads_a_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("remux-authz-mtls-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            r#"
                [[identities]]
                subject = "agent-7"
                roles = ["operator"]
            "#,
        )
        .unwrap();
        let ids = load_mtls_identities(&path, vec!["viewer".to_string()]).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            ids.principal_for("agent-7").roles,
            vec!["operator".to_string()]
        );
        // Unmapped falls back to the default.
        assert_eq!(ids.principal_for("other").roles, vec!["viewer".to_string()]);
        assert!(matches!(
            load_mtls_identities(dir.join("nope-remux.toml"), vec![]),
            Err(MtlsIdentityError::Io { .. })
        ));
    }
}
