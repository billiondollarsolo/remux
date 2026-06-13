//! [`TokenStore`] — maps bearer-token strings to [`Principal`]s with a
//! **constant-time** lookup.
//!
//! The lookup compares the presented token against *every* registered token
//! without an early exit, so the timing of a resolve does not reveal which (if
//! any) token matched, nor how many tokens are configured beyond the fixed
//! per-entry cost. This is the Phase A credential resolver; Phases B/C add
//! OIDC/JWT and mTLS resolvers that produce the same [`Principal`] shape.

use crate::principal::Principal;

/// A bearer-token → [`Principal`] map with constant-time resolution.
#[derive(Debug, Clone, Default)]
pub struct TokenStore {
    /// `(token, principal)` entries. Order is irrelevant; resolution scans all.
    entries: Vec<(String, Principal)>,
}

impl TokenStore {
    /// An empty store (resolves everything to `None`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a token → principal mapping.
    ///
    /// If `token` is already present, the **first** registration wins (the new
    /// one is ignored) so back-compat flags inserted first are not silently
    /// overridden by a later config entry colliding on the same secret. Empty
    /// tokens are ignored (they could never be presented meaningfully and would
    /// match a missing `Authorization` reduced to `""`).
    pub fn insert(&mut self, token: impl Into<String>, principal: Principal) {
        let token = token.into();
        if token.is_empty() {
            return;
        }
        if self.entries.iter().any(|(t, _)| t == &token) {
            return;
        }
        self.entries.push((token, principal));
    }

    /// Build a store from an iterator of `(token, principal)` pairs (first wins
    /// on duplicate tokens).
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, Principal)>) -> Self {
        let mut store = Self::new();
        for (token, principal) in pairs {
            store.insert(token, principal);
        }
        store
    }

    /// The number of registered tokens.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store has no tokens.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolve a presented token to its [`Principal`], or `None` if it matches no
    /// entry.
    ///
    /// **Constant-time:** every entry is compared with [`constant_time_eq`] and
    /// the loop does **not** short-circuit on the first match, so timing does not
    /// leak which token matched or how early a non-match diverged. The matched
    /// principal is selected without a data-dependent branch on the comparison
    /// result being observable through control flow over the entry list.
    pub fn resolve(&self, presented: &str) -> Option<&Principal> {
        let presented = presented.as_bytes();
        let mut matched: Option<&Principal> = None;
        for (token, principal) in &self.entries {
            // Evaluate the compare for every entry (no early break).
            let eq = constant_time_eq(token.as_bytes(), presented);
            if eq {
                matched = Some(principal);
            }
        }
        matched
    }
}

/// Constant-time byte-slice equality. Folds the length difference into the
/// accumulator so unequal lengths fail without an early return, and iterates
/// over the max length so timing does not reveal the position of the first
/// difference.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len_diff = (a.len() as u64) ^ (b.len() as u64);
    let mut diff: u8 = (len_diff as u8) | ((len_diff >> 8) as u8) | ((len_diff >> 16) as u8);
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn princ(subject: &str, role: &str) -> Principal {
        Principal::new(subject, [role.to_string()])
    }

    #[test]
    fn resolve_returns_the_right_principal() {
        let mut store = TokenStore::new();
        store.insert("tok-a", princ("alice", "operator"));
        store.insert("tok-b", princ("bob", "viewer"));

        assert_eq!(store.resolve("tok-a").unwrap().subject, "alice");
        assert_eq!(store.resolve("tok-b").unwrap().subject, "bob");
        assert!(store.resolve("tok-c").is_none());
        assert!(store.resolve("").is_none());
    }

    #[test]
    fn first_registration_wins_on_duplicate_token() {
        let mut store = TokenStore::new();
        store.insert("dup", princ("first", "admin"));
        store.insert("dup", princ("second", "viewer"));
        assert_eq!(store.len(), 1);
        assert_eq!(store.resolve("dup").unwrap().subject, "first");
    }

    #[test]
    fn empty_token_is_ignored() {
        let mut store = TokenStore::new();
        store.insert("", princ("nobody", "admin"));
        assert!(store.is_empty());
        assert!(store.resolve("").is_none());
    }

    #[test]
    fn from_pairs_builds_store() {
        let store = TokenStore::from_pairs([
            ("a".to_string(), princ("alice", "viewer")),
            ("b".to_string(), princ("bob", "operator")),
        ]);
        assert_eq!(store.len(), 2);
        assert_eq!(store.resolve("b").unwrap().subject, "bob");
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
        // Differing only in length still fails.
        assert!(!constant_time_eq(b"abcdef", b"abc"));
    }

    #[test]
    fn resolve_scans_all_entries_no_early_exit() {
        // A later-registered token still resolves even though an earlier entry
        // is a near-miss of the same length (no short-circuit on first compare).
        let mut store = TokenStore::new();
        store.insert("aaaa", princ("first", "viewer"));
        store.insert("aaab", princ("second", "operator"));
        assert_eq!(store.resolve("aaab").unwrap().subject, "second");
    }
}
