//! The in-memory host registry (AW6 §8.1).
//!
//! Gateways register **themselves** to the control plane (outbound), which
//! preserves the non-negotiable invariant that the daemon never grows an inbound
//! network listener: the control plane never dials into a host it was not first
//! told about by that host's own gateway.
//!
//! The registry is a `HashMap<String, HostEntry>` behind an `Arc<RwLock<_>>`,
//! keyed by host name. Registration is an idempotent upsert; a heartbeat (or a
//! re-register) refreshes `last_seen`. A host is **healthy** while
//! `now - last_seen < ttl`; an expired host is excluded from fan-out and listed
//! as unhealthy.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// The default time-to-live for a registration if the client omits `ttl_secs`.
/// A host must heartbeat (or re-register) within this window to stay healthy.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);

/// A registered host (one gateway) in the control-plane registry.
#[derive(Debug, Clone)]
pub struct HostEntry {
    /// Logical host name (the registry key); also the routing handle.
    pub name: String,
    /// The gateway's reachable base URL (e.g. `https://10.0.0.4:8443`).
    pub gateway_url: String,
    /// Selector labels (`env=dev`, `region=…`) used by fan-out/route filtering.
    pub labels: BTreeMap<String, String>,
    /// The gateway's bearer token the control plane uses to call its `/v1` API.
    /// Never serialized back out of the registry.
    pub gateway_token: String,
    /// When this host last registered or heartbeat.
    pub last_seen: SystemTime,
    /// The TTL after which, without a heartbeat, the host is unhealthy.
    pub ttl: Duration,
}

impl HostEntry {
    /// Whether the host is healthy: it has been seen within its TTL.
    pub fn is_healthy(&self, now: SystemTime) -> bool {
        match now.duration_since(self.last_seen) {
            Ok(elapsed) => elapsed < self.ttl,
            // `last_seen` is in the future (clock skew) — treat as fresh.
            Err(_) => true,
        }
    }

    /// Whether this host matches *all* of the given `k=v` label selectors.
    pub fn matches_labels(&self, selectors: &BTreeMap<String, String>) -> bool {
        selectors
            .iter()
            .all(|(k, v)| self.labels.get(k).map(|hv| hv == v).unwrap_or(false))
    }
}

/// The public, non-secret view of a host (`GET /cp/v1/hosts`). Omits the gateway
/// token; carries the computed `healthy` flag.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostView {
    pub name: String,
    pub url: String,
    pub labels: BTreeMap<String, String>,
    /// RFC3339 timestamp of the last register/heartbeat.
    pub last_seen: String,
    /// `now - last_seen < ttl`.
    pub healthy: bool,
}

/// The shared, cloneable registry handle.
#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<RwLock<HashMap<String, HostEntry>>>,
}

impl Registry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Idempotent upsert by name. Sets `last_seen = now`. Returns `true` if this
    /// created a new entry, `false` if it updated an existing one.
    pub async fn upsert(
        &self,
        name: String,
        gateway_url: String,
        labels: BTreeMap<String, String>,
        gateway_token: String,
        ttl: Duration,
    ) -> bool {
        let mut guard = self.inner.write().await;
        let is_new = !guard.contains_key(&name);
        guard.insert(
            name.clone(),
            HostEntry {
                name,
                gateway_url,
                labels,
                gateway_token,
                last_seen: SystemTime::now(),
                ttl,
            },
        );
        is_new
    }

    /// Refresh `last_seen` for an existing host. Returns `false` if no such host
    /// is registered (the caller turns that into `404`).
    pub async fn heartbeat(&self, name: &str) -> bool {
        let mut guard = self.inner.write().await;
        if let Some(entry) = guard.get_mut(name) {
            entry.last_seen = SystemTime::now();
            true
        } else {
            false
        }
    }

    /// Deregister a host. Returns `true` if a host was removed.
    pub async fn remove(&self, name: &str) -> bool {
        self.inner.write().await.remove(name).is_some()
    }

    /// Snapshot every host as a public [`HostView`] (no tokens), sorted by name
    /// for deterministic output.
    pub async fn views(&self) -> Vec<HostView> {
        let now = SystemTime::now();
        let guard = self.inner.read().await;
        let mut views: Vec<HostView> = guard
            .values()
            .map(|e| HostView {
                name: e.name.clone(),
                url: e.gateway_url.clone(),
                labels: e.labels.clone(),
                last_seen: rfc3339(e.last_seen),
                healthy: e.is_healthy(now),
            })
            .collect();
        views.sort_by(|a, b| a.name.cmp(&b.name));
        views
    }

    /// All currently **healthy** hosts matching every label selector, sorted by
    /// name (deterministic). Used by fan-out and intent routing.
    pub async fn healthy_matching(&self, selectors: &BTreeMap<String, String>) -> Vec<HostEntry> {
        let now = SystemTime::now();
        let guard = self.inner.read().await;
        let mut hosts: Vec<HostEntry> = guard
            .values()
            .filter(|e| e.is_healthy(now) && e.matches_labels(selectors))
            .cloned()
            .collect();
        hosts.sort_by(|a, b| a.name.cmp(&b.name));
        hosts
    }
}

/// Render a `SystemTime` as an RFC3339 string for the public host view.
fn rfc3339(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn upsert_is_idempotent_by_name() {
        let reg = Registry::new();
        assert!(
            reg.upsert(
                "a".into(),
                "https://a:8443".into(),
                labels(&[("env", "dev")]),
                "tok".into(),
                DEFAULT_TTL,
            )
            .await,
            "first upsert is new"
        );
        // Same name again updates rather than duplicating.
        assert!(
            !reg.upsert(
                "a".into(),
                "https://a-new:8443".into(),
                labels(&[("env", "prod")]),
                "tok2".into(),
                DEFAULT_TTL,
            )
            .await,
            "second upsert is an update"
        );
        let views = reg.views().await;
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].url, "https://a-new:8443");
        assert_eq!(views[0].labels, labels(&[("env", "prod")]));
    }

    #[tokio::test]
    async fn heartbeat_and_remove() {
        let reg = Registry::new();
        reg.upsert(
            "h".into(),
            "https://h:8443".into(),
            BTreeMap::new(),
            "t".into(),
            DEFAULT_TTL,
        )
        .await;
        assert!(reg.heartbeat("h").await);
        assert!(!reg.heartbeat("missing").await);
        assert!(reg.remove("h").await);
        assert!(!reg.remove("h").await);
        assert!(reg.views().await.is_empty());
    }

    #[tokio::test]
    async fn expired_host_is_unhealthy_and_excluded() {
        let reg = Registry::new();
        // A zero TTL is immediately expired.
        reg.upsert(
            "stale".into(),
            "https://stale:8443".into(),
            labels(&[("env", "dev")]),
            "t".into(),
            Duration::from_millis(0),
        )
        .await;
        // Ensure some time passes.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let views = reg.views().await;
        assert_eq!(views.len(), 1);
        assert!(!views[0].healthy, "zero-ttl host must be unhealthy");
        let healthy = reg.healthy_matching(&BTreeMap::new()).await;
        assert!(healthy.is_empty(), "expired host excluded from fan-out");
    }

    #[tokio::test]
    async fn label_filtering_matches_all_selectors() {
        let reg = Registry::new();
        reg.upsert(
            "dev".into(),
            "https://dev:8443".into(),
            labels(&[("env", "dev"), ("region", "us")]),
            "t".into(),
            DEFAULT_TTL,
        )
        .await;
        reg.upsert(
            "prod".into(),
            "https://prod:8443".into(),
            labels(&[("env", "prod"), ("region", "us")]),
            "t".into(),
            DEFAULT_TTL,
        )
        .await;

        // env=dev -> only the dev host.
        let only_dev = reg.healthy_matching(&labels(&[("env", "dev")])).await;
        assert_eq!(only_dev.len(), 1);
        assert_eq!(only_dev[0].name, "dev");

        // region=us -> both, sorted by name (dev before prod).
        let both = reg.healthy_matching(&labels(&[("region", "us")])).await;
        assert_eq!(
            both.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
            vec!["dev", "prod"]
        );

        // env=dev AND region=eu -> none (no host satisfies both).
        let none = reg
            .healthy_matching(&labels(&[("env", "dev"), ("region", "eu")]))
            .await;
        assert!(none.is_empty());
    }
}
