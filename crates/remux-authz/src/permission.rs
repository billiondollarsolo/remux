//! The fine-grained [`Permission`] enum spanning both network surfaces (the
//! gateway's `/v1` API and the control plane's `/cp/v1` fleet API).
//!
//! Each variant has a **stable string name** (`"session.read"`,
//! `"fleet.resolve"`, `"host.register"`, …) used in the auth-config file, in
//! custom-role definitions, and in audit logs. The string form is the contract;
//! the enum order is not.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A single fine-grained capability. Permissions are surface-specific but live
/// in one enum so the [`crate::Authorizer`] and the [`crate::Policy`] are shared
/// across the gateway and the control plane.
///
/// String names are stable and used for config + audit; see [`Permission::name`]
/// and the [`FromStr`]/[`Display`] impls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Permission {
    // --- Gateway (`/v1`) ---
    /// List sessions (`GET /v1/sessions`).
    SessionList,
    /// Read a session: inspect / screen / scrollback (`GET /v1/sessions/{id}*`).
    SessionRead,
    /// Create a session (`POST /v1/sessions`).
    SessionCreate,
    /// Send input to a session (`POST /v1/sessions/{id}/input`).
    SessionInput,
    /// Resize a session's PTY (`POST /v1/sessions/{id}/resize`).
    SessionResize,
    /// Kill a session (`DELETE /v1/sessions/{id}`).
    SessionKill,
    /// Rename a session (`PATCH /v1/sessions/{id}`).
    SessionRename,
    /// Attach the interactive binary stream (`GET /v1/sessions/{id}/stream`).
    SessionStream,
    /// Wait on a session's semantic state (`POST /v1/sessions/{id}/wait`).
    SessionWait,
    /// Read the structured event channel (`GET /v1/sessions/{id}/events`).
    EventsRead,

    // --- Control plane (`/cp/v1`) ---
    /// List fleet hosts (`GET /cp/v1/hosts`).
    FleetHostsRead,
    /// Read federated sessions (`GET /cp/v1/sessions`).
    FleetSessionsRead,
    /// Intent-based session routing (`POST /cp/v1/resolve`).
    FleetResolve,
    /// Register / heartbeat / deregister a host (`POST /cp/v1/register`,
    /// `POST /cp/v1/heartbeat`, `DELETE /cp/v1/hosts/{name}`).
    HostRegister,
}

impl Permission {
    /// Every permission, in a stable order. Used to build the all-permissions
    /// built-in roles and to round-trip every name in tests.
    pub const ALL: &'static [Permission] = &[
        Permission::SessionList,
        Permission::SessionRead,
        Permission::SessionCreate,
        Permission::SessionInput,
        Permission::SessionResize,
        Permission::SessionKill,
        Permission::SessionRename,
        Permission::SessionStream,
        Permission::SessionWait,
        Permission::EventsRead,
        Permission::FleetHostsRead,
        Permission::FleetSessionsRead,
        Permission::FleetResolve,
        Permission::HostRegister,
    ];

    /// Every gateway (`/v1`) permission, in a stable order. Used for the
    /// gateway `admin` built-in role.
    pub const GATEWAY: &'static [Permission] = &[
        Permission::SessionList,
        Permission::SessionRead,
        Permission::SessionCreate,
        Permission::SessionInput,
        Permission::SessionResize,
        Permission::SessionKill,
        Permission::SessionRename,
        Permission::SessionStream,
        Permission::SessionWait,
        Permission::EventsRead,
    ];

    /// Every control-plane (`/cp/v1`) permission, in a stable order. Used for the
    /// `fleet-admin` built-in role.
    pub const CONTROL_PLANE: &'static [Permission] = &[
        Permission::FleetHostsRead,
        Permission::FleetSessionsRead,
        Permission::FleetResolve,
        Permission::HostRegister,
    ];

    /// The stable string name for this permission (config + audit form).
    pub const fn name(self) -> &'static str {
        match self {
            Permission::SessionList => "session.list",
            Permission::SessionRead => "session.read",
            Permission::SessionCreate => "session.create",
            Permission::SessionInput => "session.input",
            Permission::SessionResize => "session.resize",
            Permission::SessionKill => "session.kill",
            Permission::SessionRename => "session.rename",
            Permission::SessionStream => "session.stream",
            Permission::SessionWait => "session.wait",
            Permission::EventsRead => "events.read",
            Permission::FleetHostsRead => "fleet.hosts.read",
            Permission::FleetSessionsRead => "fleet.sessions.read",
            Permission::FleetResolve => "fleet.resolve",
            Permission::HostRegister => "host.register",
        }
    }
}

/// Error parsing a [`Permission`] from its string name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission name: {0:?}")]
pub struct ParsePermissionError(pub String);

impl FromStr for Permission {
    type Err = ParsePermissionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Permission::ALL
            .iter()
            .copied()
            .find(|p| p.name() == s)
            .ok_or_else(|| ParsePermissionError(s.to_string()))
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl From<Permission> for String {
    fn from(p: Permission) -> String {
        p.name().to_string()
    }
}

impl TryFrom<String> for Permission {
    type Error = ParsePermissionError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_roundtrips_for_every_permission() {
        for &p in Permission::ALL {
            let name = p.name();
            assert_eq!(name.parse::<Permission>().unwrap(), p, "roundtrip {name}");
            assert_eq!(p.to_string(), name);
        }
    }

    #[test]
    fn all_names_are_unique() {
        let mut names: Vec<&str> = Permission::ALL.iter().map(|p| p.name()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "permission names must be unique");
    }

    #[test]
    fn unknown_name_is_rejected() {
        assert!("session.nope".parse::<Permission>().is_err());
        assert!("".parse::<Permission>().is_err());
        assert_eq!(
            "x".parse::<Permission>().unwrap_err(),
            ParsePermissionError("x".to_string())
        );
    }

    #[test]
    fn gateway_and_cp_partition_all() {
        assert_eq!(
            Permission::GATEWAY.len() + Permission::CONTROL_PLANE.len(),
            Permission::ALL.len()
        );
    }

    #[test]
    fn serde_uses_string_name() {
        let json = serde_json::to_string(&Permission::FleetResolve).unwrap();
        assert_eq!(json, "\"fleet.resolve\"");
        let back: Permission = serde_json::from_str("\"host.register\"").unwrap();
        assert_eq!(back, Permission::HostRegister);
        assert!(serde_json::from_str::<Permission>("\"bogus\"").is_err());
    }
}
