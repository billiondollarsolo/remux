//! Session-selector parsing, mirroring the CLI (`cmd/{kill,wait,send}.rs`):
//! a path segment that parses as a UUID is an `Id`, otherwise it is a `Name`.

use remux_core::{SessionId, SessionSelector};

/// Parse a `{id}` path segment into a [`SessionSelector`].
///
/// A valid UUID becomes [`SessionSelector::Id`]; anything else is treated as a
/// session name ([`SessionSelector::Name`]) — exactly the CLI's `parse_selector`.
pub fn parse_selector(raw: &str) -> SessionSelector {
    match uuid::Uuid::parse_str(raw) {
        Ok(uuid) => SessionSelector::Id(SessionId(uuid)),
        Err(_) => SessionSelector::Name(raw.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_segment_parses_as_id() {
        let id = SessionId::new();
        let sel = parse_selector(&id.0.to_string());
        assert_eq!(sel, SessionSelector::Id(id));
    }

    #[test]
    fn non_uuid_segment_parses_as_name() {
        let sel = parse_selector("build");
        assert_eq!(sel, SessionSelector::Name("build".to_string()));
    }
}
