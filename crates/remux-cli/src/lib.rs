//! Library surface of the `remux` CLI.
//!
//! The CLI is primarily a binary (`src/main.rs`), but a few pieces are also
//! exposed here so integration tests can drive the *exact* production code
//! without a real SSH server:
//!
//! * [`client`] — the transport client (`RemuxClient::connect_via_command`)
//!   used by the SSH remote path.
//! * [`cmd::fleet`] — the client-side fleet discovery (AW6 v1). Its pure
//!   aggregation helpers and the `gather_sessions` fan-out (with its injectable
//!   connector) are unit- and integration-tested through this surface.
//! * [`render`] — rendering helpers the fleet aggregation reuses.
pub mod client;
pub mod render;

/// CLI command handlers re-exposed for integration tests. The binary
/// (`src/main.rs`) declares the full `cmd` tree; here we surface only the parts
/// tests drive directly.
#[path = "cmd/mod_fleet.rs"]
pub mod cmd;
