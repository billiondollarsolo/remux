//! Public API surface, versioned by module path. `/v1` is the first (and
//! currently only) version. A future `/v2` would be added side-by-side as
//! `api::v2`, keeping `v1`'s DTOs and conversions frozen.

pub mod v1;
