//! [`Role`]s, the [`Policy`] that names them, and the built-in roles for both
//! surfaces.
//!
//! A role is a named set of [`Permission`]s. A policy maps role names to roles.
//! A [`crate::Principal`] references roles by name; the [`crate::Authorizer`]
//! resolves those names against a policy and unions their permissions.

use std::collections::{BTreeMap, BTreeSet};

use crate::permission::Permission;

/// A named set of permissions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    /// The role's stable name (referenced by principals + config).
    pub name: String,
    /// The permissions this role grants.
    pub permissions: BTreeSet<Permission>,
}

impl Role {
    /// Build a role from a name and an iterator of permissions.
    pub fn new(name: impl Into<String>, permissions: impl IntoIterator<Item = Permission>) -> Self {
        Self {
            name: name.into(),
            permissions: permissions.into_iter().collect(),
        }
    }
}

/// A named collection of roles. Lookups are by role name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    roles: BTreeMap<String, Role>,
}

impl Policy {
    /// An empty policy (no roles). Mostly useful as a base to insert into.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The built-in policy: every gateway and control-plane role
    /// (`viewer`/`operator`/`admin` and `registrar`/`fleet-viewer`/
    /// `fleet-operator`/`fleet-admin`).
    pub fn builtin() -> Self {
        let mut policy = Self::empty();
        for role in builtin_roles() {
            policy.insert(role);
        }
        policy
    }

    /// Insert (or replace) a role by name. A custom role with the same name as a
    /// built-in overrides it (the merge semantics used by config loading).
    pub fn insert(&mut self, role: Role) {
        self.roles.insert(role.name.clone(), role);
    }

    /// Look up a role by name.
    pub fn get(&self, name: &str) -> Option<&Role> {
        self.roles.get(name)
    }

    /// Whether a role with `name` exists.
    pub fn contains(&self, name: &str) -> bool {
        self.roles.contains_key(name)
    }

    /// Iterate over every role, in name order.
    pub fn roles(&self) -> impl Iterator<Item = &Role> {
        self.roles.values()
    }

    /// The number of roles in the policy.
    pub fn len(&self) -> usize {
        self.roles.len()
    }

    /// Whether the policy has no roles.
    pub fn is_empty(&self) -> bool {
        self.roles.is_empty()
    }
}

/// The built-in roles for both surfaces.
///
/// Gateway: `viewer` ⊂ `operator` ⊂ `admin`.
/// Control plane: `registrar`; `fleet-viewer` ⊂ `fleet-operator` ⊂ `fleet-admin`.
pub fn builtin_roles() -> Vec<Role> {
    use Permission::*;

    // Gateway.
    let viewer = [SessionList, SessionRead, SessionWait, EventsRead];
    let operator: Vec<Permission> = viewer
        .iter()
        .copied()
        .chain([
            SessionCreate,
            SessionInput,
            SessionResize,
            SessionKill,
            SessionRename,
            SessionStream,
        ])
        .collect();
    let admin: Vec<Permission> = Permission::GATEWAY.to_vec();

    // Control plane.
    let registrar = [HostRegister];
    let fleet_viewer = [FleetHostsRead, FleetSessionsRead];
    let fleet_operator: Vec<Permission> =
        fleet_viewer.iter().copied().chain([FleetResolve]).collect();
    let fleet_admin: Vec<Permission> = Permission::CONTROL_PLANE.to_vec();

    vec![
        Role::new("viewer", viewer),
        Role::new("operator", operator),
        Role::new("admin", admin),
        Role::new("registrar", registrar),
        Role::new("fleet-viewer", fleet_viewer),
        Role::new("fleet-operator", fleet_operator),
        Role::new("fleet-admin", fleet_admin),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use Permission::*;

    fn perms(policy: &Policy, name: &str) -> BTreeSet<Permission> {
        policy.get(name).unwrap().permissions.clone()
    }

    #[test]
    fn builtin_has_all_named_roles() {
        let p = Policy::builtin();
        for name in [
            "viewer",
            "operator",
            "admin",
            "registrar",
            "fleet-viewer",
            "fleet-operator",
            "fleet-admin",
        ] {
            assert!(p.contains(name), "missing builtin role {name}");
        }
        assert_eq!(p.len(), 7);
    }

    #[test]
    fn gateway_role_unions_are_nested() {
        let p = Policy::builtin();
        let viewer = perms(&p, "viewer");
        let operator = perms(&p, "operator");
        let admin = perms(&p, "admin");

        // viewer is exactly the read set.
        assert_eq!(
            viewer,
            BTreeSet::from([SessionList, SessionRead, SessionWait, EventsRead])
        );
        // viewer ⊂ operator ⊂ admin.
        assert!(viewer.is_subset(&operator));
        assert!(operator.is_subset(&admin));
        // operator gains the write perms but NOT all of admin (admin == all gw).
        assert!(operator.contains(&SessionInput));
        assert!(operator.contains(&SessionStream));
        assert_eq!(admin, Permission::GATEWAY.iter().copied().collect());
    }

    #[test]
    fn control_plane_role_unions_are_nested() {
        let p = Policy::builtin();
        let registrar = perms(&p, "registrar");
        let fleet_viewer = perms(&p, "fleet-viewer");
        let fleet_operator = perms(&p, "fleet-operator");
        let fleet_admin = perms(&p, "fleet-admin");

        assert_eq!(registrar, BTreeSet::from([HostRegister]));
        assert_eq!(
            fleet_viewer,
            BTreeSet::from([FleetHostsRead, FleetSessionsRead])
        );
        assert!(fleet_viewer.is_subset(&fleet_operator));
        assert!(fleet_operator.contains(&FleetResolve));
        // fleet-operator does NOT include HostRegister; fleet-admin does (all CP).
        assert!(!fleet_operator.contains(&HostRegister));
        assert_eq!(
            fleet_admin,
            Permission::CONTROL_PLANE.iter().copied().collect()
        );
    }

    #[test]
    fn insert_overrides_by_name() {
        let mut p = Policy::builtin();
        p.insert(Role::new("viewer", [SessionList]));
        assert_eq!(perms(&p, "viewer"), BTreeSet::from([SessionList]));
    }
}
