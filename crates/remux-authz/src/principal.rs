//! [`Principal`] — an authenticated subject with a set of role names — and the
//! [`Authorizer`] logic that decides whether a principal may exercise a
//! [`Permission`] under a [`Policy`].
//!
//! A principal grants the **union** of its roles' permissions. Deny-by-default:
//! an unknown role name grants nothing and is logged (never silently treated as
//! a wildcard).

use std::collections::BTreeSet;

use crate::permission::Permission;
use crate::policy::Policy;

/// An authenticated subject. In Phase A a principal comes from a bearer token in
/// the [`crate::TokenStore`]; in later phases the same shape is produced from an
/// OIDC/JWT claim set or an mTLS client certificate (the seam this model is
/// designed around).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// A stable identifier for this subject (`"admin"`, `"ci-bot"`, an OIDC
    /// `sub`, a cert CN, …). Used in audit logs; never a secret.
    pub subject: String,
    /// The role names this subject holds. Resolved against a [`Policy`] at
    /// authorization time.
    pub roles: Vec<String>,
}

impl Principal {
    /// Build a principal from a subject and its role names.
    pub fn new(subject: impl Into<String>, roles: impl IntoIterator<Item = String>) -> Self {
        Self {
            subject: subject.into(),
            roles: roles.into_iter().collect(),
        }
    }

    /// The principal's role names joined as a comma-separated string (audit form).
    pub fn roles_display(&self) -> String {
        self.roles.join(",")
    }
}

/// Stateless authorization logic over a [`Policy`] and a [`Principal`].
pub struct Authorizer;

impl Authorizer {
    /// The effective permission set for `principal` under `policy`: the union of
    /// every known role's permissions. Unknown role names are skipped (and
    /// logged by [`permits`]); they never widen the set.
    pub fn effective_permissions(policy: &Policy, principal: &Principal) -> BTreeSet<Permission> {
        let mut out = BTreeSet::new();
        for role in &principal.roles {
            if let Some(r) = policy.get(role) {
                out.extend(r.permissions.iter().copied());
            }
        }
        out
    }
}

/// Whether `principal` may exercise `perm` under `policy`.
///
/// Deny-by-default: returns `true` only if at least one of the principal's
/// **known** roles grants `perm`. An unknown role name is logged at `warn` and
/// contributes nothing — it is never silently treated as a grant.
pub fn permits(policy: &Policy, principal: &Principal, perm: Permission) -> bool {
    let mut granted = false;
    for role in &principal.roles {
        match policy.get(role) {
            Some(r) => {
                if r.permissions.contains(&perm) {
                    granted = true;
                }
            }
            None => {
                // Deny-by-default: log, do not grant.
                log_unknown_role(&principal.subject, role);
            }
        }
    }
    granted
}

/// Log an unknown role reference. Pulled out so the crate stays dependency-light
/// (no `tracing` here); we emit to `stderr` which the host services capture.
fn log_unknown_role(subject: &str, role: &str) {
    eprintln!(
        "remux-authz: principal {subject:?} references unknown role {role:?}; \
         granting no permissions for it (deny-by-default)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use Permission::*;

    fn gw_principal(roles: &[&str]) -> Principal {
        Principal::new("test", roles.iter().map(|s| s.to_string()))
    }

    #[test]
    fn viewer_can_read_not_write() {
        let p = Policy::builtin();
        let viewer = gw_principal(&["viewer"]);
        assert!(permits(&p, &viewer, SessionList));
        assert!(permits(&p, &viewer, SessionRead));
        assert!(permits(&p, &viewer, SessionWait));
        assert!(permits(&p, &viewer, EventsRead));
        // viewer cannot input / stream / kill / create.
        assert!(!permits(&p, &viewer, SessionInput));
        assert!(!permits(&p, &viewer, SessionStream));
        assert!(!permits(&p, &viewer, SessionKill));
        assert!(!permits(&p, &viewer, SessionCreate));
    }

    #[test]
    fn operator_can_input_and_stream() {
        let p = Policy::builtin();
        let op = gw_principal(&["operator"]);
        assert!(permits(&p, &op, SessionInput));
        assert!(permits(&p, &op, SessionStream));
        assert!(permits(&p, &op, SessionCreate));
        assert!(permits(&p, &op, SessionKill));
        // still has the viewer perms (union).
        assert!(permits(&p, &op, SessionRead));
    }

    #[test]
    fn admin_has_every_gateway_permission() {
        let p = Policy::builtin();
        let admin = gw_principal(&["admin"]);
        for &perm in Permission::GATEWAY {
            assert!(permits(&p, &admin, perm), "admin should permit {perm}");
        }
    }

    #[test]
    fn union_of_multiple_roles() {
        let p = Policy::builtin();
        // viewer + registrar: read perms plus host.register.
        let multi = gw_principal(&["viewer", "registrar"]);
        assert!(permits(&p, &multi, SessionRead));
        assert!(permits(&p, &multi, HostRegister));
        assert!(!permits(&p, &multi, SessionInput));
    }

    #[test]
    fn unknown_role_denies_and_does_not_grant() {
        let p = Policy::builtin();
        let bogus = gw_principal(&["does-not-exist"]);
        assert!(!permits(&p, &bogus, SessionRead));
        assert!(Authorizer::effective_permissions(&p, &bogus).is_empty());
        // A known role alongside an unknown one still works for its own perms.
        let mixed = gw_principal(&["viewer", "does-not-exist"]);
        assert!(permits(&p, &mixed, SessionRead));
        assert!(!permits(&p, &mixed, SessionInput));
    }

    #[test]
    fn effective_permissions_is_the_union() {
        let p = Policy::builtin();
        let op = gw_principal(&["operator"]);
        let eff = Authorizer::effective_permissions(&p, &op);
        assert!(eff.contains(&SessionInput));
        assert!(eff.contains(&SessionRead));
        assert!(!eff.contains(&HostRegister));
    }

    #[test]
    fn fleet_viewer_cannot_resolve() {
        let p = Policy::builtin();
        let fv = gw_principal(&["fleet-viewer"]);
        assert!(permits(&p, &fv, FleetHostsRead));
        assert!(permits(&p, &fv, FleetSessionsRead));
        assert!(!permits(&p, &fv, FleetResolve));
    }
}
