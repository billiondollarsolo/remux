//! `remux fleet` — client-side multi-host discovery (AW6 v1).
//!
//! This is the **client-side first slice** of the fleet model
//! (`docs/AGENT_API_PLAN.md` §8): a static host registry (`[[fleet.hosts]]` in
//! config) plus a concurrent fan-out over the **existing SSH transport**. There
//! is NO control-plane service, NO RBAC, NO gateway/daemon changes — discovery
//! is just `ssh <host> remux bridge` run against each registered host in
//! parallel, with results aggregated and tagged by host.
//!
//! Subcommands:
//!   * `fleet hosts` — list configured hosts.
//!   * `fleet ls`    — fan out, list sessions per host, aggregate (with errors).
//!   * `fleet attach <host>:<session>` — resolve host name → ssh target, attach.
//!
//! The "connect + list" step goes through an **injectable connector** (a
//! `Fn(&FleetHost) -> Command`): production builds `ssh <target> remux bridge`;
//! tests build `remux bridge --socket <harness.sock>` so the exact same
//! transport path is exercised without a real SSH server. The aggregation /
//! row-building logic is kept in **pure** functions ([`build_json`],
//! [`build_rows`]) so it is unit-testable in isolation.

use std::time::Duration;

use remux_core::{FleetHost, Request, Response, SessionSummary};
use serde::Serialize;
use tokio::process::Command;

use crate::client::RemuxClient;
use crate::render::format_duration;

/// Per-host outcome of the discovery fan-out: either the host's sessions, or a
/// human-readable error explaining why it was unreachable. Unreachable hosts are
/// reported, never fatal — one bad host must not abort the whole command.
#[derive(Debug, Clone)]
pub struct HostResult {
    pub host: String,
    pub ssh: String,
    pub result: Result<Vec<SessionSummary>, String>,
}

/// JSON shape emitted by `fleet ls --json`: an array of these.
#[derive(Debug, Serialize)]
pub struct HostResultJson {
    pub host: String,
    pub ssh: String,
    pub ok: bool,
    pub error: Option<String>,
    pub sessions: Vec<SessionSummary>,
}

/// How long to wait for a single host's "connect + handshake + list" before
/// giving up and marking it unreachable. Keeps the fan-out bounded so one
/// hanging host can't stall the whole command.
const PER_HOST_TIMEOUT: Duration = Duration::from_secs(20);

/// Filter `hosts` to those whose labels match ALL of the given `k=v` selectors.
/// A host matches when, for every selector, it has that label set to that value.
/// An empty selector list matches everything.
pub fn filter_hosts<'a>(
    hosts: &'a [FleetHost],
    selectors: &[(String, String)],
) -> Vec<&'a FleetHost> {
    hosts
        .iter()
        .filter(|h| {
            selectors
                .iter()
                .all(|(k, v)| h.labels.get(k).map(|val| val == v).unwrap_or(false))
        })
        .collect()
}

/// Parse a `--label key=value` argument into a `(key, value)` pair.
pub fn parse_label(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
        _ => Err(format!("invalid --label {s:?} (expected key=value)")),
    }
}

/// Connect over a pre-built `cmd` and list sessions, bounded by
/// [`PER_HOST_TIMEOUT`]. Returns the sessions or a human-readable error string;
/// it never panics, so many of these can run concurrently and each contributes
/// whatever it produced. `name`/`ssh` are carried through for the result.
async fn gather_one(name: String, ssh: String, cmd: Command) -> HostResult {
    let outcome = tokio::time::timeout(PER_HOST_TIMEOUT, async {
        let mut client = RemuxClient::connect_via_command(cmd)
            .await
            .map_err(|e| e.to_string())?;
        match client.send_request(Request::ListSessions).await {
            Ok(Response::SessionList(sessions)) => Ok(sessions),
            Ok(Response::Error(e)) => Err(e.to_string()),
            Ok(other) => Err(format!("unexpected response: {other:?}")),
            Err(e) => Err(e.to_string()),
        }
    })
    .await;

    let result = match outcome {
        Ok(inner) => inner,
        Err(_) => Err(format!("timed out after {}s", PER_HOST_TIMEOUT.as_secs())),
    };

    HostResult {
        host: name,
        ssh,
        result,
    }
}

/// Fan out across `hosts` **concurrently**, connecting to each via the injected
/// `connect` closure and listing its sessions. Returns one [`HostResult`] per
/// host, in the input order, regardless of individual successes/failures.
///
/// `connect` is the injection seam that makes this testable: production passes
/// [`ssh_bridge_connector`]; tests pass a closure that spawns
/// `remux bridge --socket <harness.sock>`. The `Command`s are built up front
/// (synchronously) so each host's connect+list runs as an independent `'static`
/// task on a [`tokio::task::JoinSet`], polled concurrently.
pub async fn gather_sessions(
    hosts: &[FleetHost],
    connect: impl Fn(&FleetHost) -> Command,
) -> Vec<HostResult> {
    let mut set = tokio::task::JoinSet::new();
    for (idx, host) in hosts.iter().enumerate() {
        let cmd = connect(host);
        let name = host.name.clone();
        let ssh = host.ssh.clone();
        set.spawn(async move { (idx, gather_one(name, ssh, cmd).await) });
    }

    // Collect, then restore input order (JoinSet completes out of order).
    let mut collected: Vec<(usize, HostResult)> = Vec::with_capacity(hosts.len());
    while let Some(joined) = set.join_next().await {
        // A task panic shouldn't abort the whole fan-out; it can't happen in
        // practice (gather_one never panics), but stay defensive and skip it.
        if let Ok(pair) = joined {
            collected.push(pair);
        }
    }
    collected.sort_by_key(|(idx, _)| *idx);
    collected.into_iter().map(|(_, r)| r).collect()
}

/// Production connector: build `ssh <target> remux bridge` for a host. This is
/// the exact command [`RemuxClient::connect_remote`] uses, exposed as a closure
/// so the fan-out can reuse it per host.
pub fn ssh_bridge_connector(host: &FleetHost) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg(&host.ssh).arg("remux").arg("bridge");
    cmd
}

/// Pure: turn per-host results into the JSON value emitted by `fleet ls --json`.
pub fn build_json(results: &[HostResult]) -> Vec<HostResultJson> {
    results
        .iter()
        .map(|r| match &r.result {
            Ok(sessions) => HostResultJson {
                host: r.host.clone(),
                ssh: r.ssh.clone(),
                ok: true,
                error: None,
                sessions: sessions.clone(),
            },
            Err(e) => HostResultJson {
                host: r.host.clone(),
                ssh: r.ssh.clone(),
                ok: false,
                error: Some(e.clone()),
                sessions: Vec::new(),
            },
        })
        .collect()
}

/// One rendered line of the `fleet ls` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetRow {
    pub host: String,
    pub name: String,
    pub status: String,
    pub pid: String,
    pub created: String,
    pub cmd: String,
}

/// Pure: flatten per-host results into renderable rows. Each session becomes a
/// row tagged with its host; an unreachable host becomes a single row whose
/// status is `unreachable` and whose `cmd` column carries the error. This keeps
/// the row-building (input: results, output: rows) trivially unit-testable.
pub fn build_rows(results: &[HostResult]) -> Vec<FleetRow> {
    let mut rows = Vec::new();
    for r in results {
        match &r.result {
            Ok(sessions) if sessions.is_empty() => {
                rows.push(FleetRow {
                    host: r.host.clone(),
                    name: "-".to_string(),
                    status: "no sessions".to_string(),
                    pid: "-".to_string(),
                    created: "-".to_string(),
                    cmd: String::new(),
                });
            }
            Ok(sessions) => {
                for s in sessions {
                    rows.push(FleetRow {
                        host: r.host.clone(),
                        name: s.name.clone(),
                        status: format!("{:?}", s.status),
                        pid: s
                            .pid
                            .map(|p| p.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        created: format_duration(s.created_at),
                        cmd: s.command.join(" "),
                    });
                }
            }
            Err(e) => {
                rows.push(FleetRow {
                    host: r.host.clone(),
                    name: "-".to_string(),
                    status: "unreachable".to_string(),
                    pid: "-".to_string(),
                    created: "-".to_string(),
                    cmd: e.clone(),
                });
            }
        }
    }
    rows
}

/// Print the aggregated rows as a table with a leading `HOST` column.
fn print_rows(rows: &[FleetRow]) {
    println!(
        "{:<16} {:<20} {:<12} {:<8} {:<14} CMD",
        "HOST", "NAME", "STATUS", "PID", "CREATED"
    );
    for row in rows {
        println!(
            "{:<16} {:<20} {:<12} {:<8} {:<14} {}",
            row.host, row.name, row.status, row.pid, row.created, row.cmd
        );
    }
}

/// `remux fleet hosts` — list configured hosts.
pub fn run_hosts(hosts: &[FleetHost], json: bool) {
    if json {
        let json_str = serde_json::to_string_pretty(hosts).unwrap_or_else(|_| "[]".to_string());
        println!("{json_str}");
        return;
    }
    if hosts.is_empty() {
        println!("No fleet hosts configured. Add [[fleet.hosts]] to your config.");
        return;
    }
    println!("{:<16} {:<28} LABELS", "NAME", "SSH");
    for h in hosts {
        let labels = h
            .labels
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        println!("{:<16} {:<28} {}", h.name, h.ssh, labels);
    }
}

/// `remux fleet ls` — fan out across (optionally label-filtered) hosts, list
/// sessions, and aggregate. Uses the production SSH connector.
pub async fn run_ls(hosts: &[FleetHost], selectors: &[(String, String)], json: bool) {
    let selected: Vec<FleetHost> = filter_hosts(hosts, selectors)
        .into_iter()
        .cloned()
        .collect();

    let results = gather_sessions(&selected, ssh_bridge_connector).await;

    if json {
        let value = build_json(&results);
        let json_str = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "[]".to_string());
        println!("{json_str}");
        return;
    }

    if selected.is_empty() {
        if hosts.is_empty() {
            println!("No fleet hosts configured. Add [[fleet.hosts]] to your config.");
        } else {
            println!("No hosts matched the given labels.");
        }
        return;
    }

    let rows = build_rows(&results);
    print_rows(&rows);
}

/// Parsed `<host>:<session>` selector for `fleet attach`.
pub struct FleetTarget {
    pub host: String,
    pub session: String,
}

/// Parse `host:session`. The host portion is a registry NAME (not an ssh
/// target); the session portion may itself contain no further `:` constraint.
pub fn parse_fleet_target(s: &str) -> Result<FleetTarget, String> {
    match s.split_once(':') {
        Some((host, session)) if !host.is_empty() && !session.is_empty() => Ok(FleetTarget {
            host: host.to_string(),
            session: session.to_string(),
        }),
        _ => Err(format!(
            "invalid target {s:?} (expected <host>:<session>, e.g. devbox:backend)"
        )),
    }
}

/// Resolve a host NAME against the registry, returning its ssh target. Errors
/// clearly (listing the known hosts) when the name isn't registered.
pub fn resolve_host<'a>(hosts: &'a [FleetHost], name: &str) -> Result<&'a FleetHost, String> {
    hosts.iter().find(|h| h.name == name).ok_or_else(|| {
        let known = hosts
            .iter()
            .map(|h| h.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if known.is_empty() {
            format!("host {name:?} is not in the fleet registry (no hosts configured)")
        } else {
            format!("host {name:?} is not in the fleet registry (known: {known})")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use remux_core::{SessionId, SessionStatus};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn host(name: &str, ssh: &str, labels: &[(&str, &str)]) -> FleetHost {
        let mut map = BTreeMap::new();
        for (k, v) in labels {
            map.insert(k.to_string(), v.to_string());
        }
        FleetHost {
            name: name.to_string(),
            ssh: ssh.to_string(),
            labels: map,
        }
    }

    fn summary(name: &str) -> SessionSummary {
        SessionSummary {
            id: SessionId(Uuid::new_v4()),
            name: name.to_string(),
            status: SessionStatus::Running,
            command: vec!["bash".to_string()],
            cwd: PathBuf::from("/tmp"),
            created_at: Utc::now(),
            pid: Some(42),
            attached_clients: 0,
        }
    }

    #[test]
    fn filter_hosts_matches_all_labels() {
        let hosts = vec![
            host("a", "a", &[("env", "dev"), ("project", "api")]),
            host("b", "b", &[("env", "dev"), ("project", "web")]),
            host("c", "c", &[("env", "prod"), ("project", "api")]),
        ];

        // No selectors -> everything.
        assert_eq!(filter_hosts(&hosts, &[]).len(), 3);

        // env=dev -> a, b.
        let sel = vec![("env".to_string(), "dev".to_string())];
        let got: Vec<_> = filter_hosts(&hosts, &sel)
            .iter()
            .map(|h| h.name.clone())
            .collect();
        assert_eq!(got, vec!["a", "b"]);

        // env=dev AND project=api -> only a.
        let sel = vec![
            ("env".to_string(), "dev".to_string()),
            ("project".to_string(), "api".to_string()),
        ];
        let got: Vec<_> = filter_hosts(&hosts, &sel)
            .iter()
            .map(|h| h.name.clone())
            .collect();
        assert_eq!(got, vec!["a"]);

        // A label value that doesn't match -> nothing.
        let sel = vec![("env".to_string(), "staging".to_string())];
        assert!(filter_hosts(&hosts, &sel).is_empty());
    }

    #[test]
    fn parse_label_ok_and_err() {
        assert_eq!(
            parse_label("env=dev").unwrap(),
            ("env".to_string(), "dev".to_string())
        );
        // value may contain '='
        assert_eq!(
            parse_label("k=a=b").unwrap(),
            ("k".to_string(), "a=b".to_string())
        );
        assert!(parse_label("noequals").is_err());
        assert!(parse_label("=v").is_err());
    }

    #[test]
    fn build_rows_mixed_ok_error_empty() {
        let results = vec![
            HostResult {
                host: "ok".to_string(),
                ssh: "u@ok".to_string(),
                result: Ok(vec![summary("backend"), summary("worker")]),
            },
            HostResult {
                host: "empty".to_string(),
                ssh: "u@empty".to_string(),
                result: Ok(vec![]),
            },
            HostResult {
                host: "down".to_string(),
                ssh: "u@down".to_string(),
                result: Err("connection refused".to_string()),
            },
        ];

        let rows = build_rows(&results);
        // 2 sessions + 1 "no sessions" + 1 "unreachable".
        assert_eq!(rows.len(), 4);

        assert_eq!(rows[0].host, "ok");
        assert_eq!(rows[0].name, "backend");
        assert_eq!(rows[1].name, "worker");

        assert_eq!(rows[2].host, "empty");
        assert_eq!(rows[2].status, "no sessions");

        assert_eq!(rows[3].host, "down");
        assert_eq!(rows[3].status, "unreachable");
        assert_eq!(rows[3].cmd, "connection refused");
    }

    #[test]
    fn build_json_mixed_ok_error() {
        let results = vec![
            HostResult {
                host: "ok".to_string(),
                ssh: "u@ok".to_string(),
                result: Ok(vec![summary("backend")]),
            },
            HostResult {
                host: "down".to_string(),
                ssh: "u@down".to_string(),
                result: Err("boom".to_string()),
            },
        ];

        let json = build_json(&results);
        assert_eq!(json.len(), 2);

        assert_eq!(json[0].host, "ok");
        assert!(json[0].ok);
        assert!(json[0].error.is_none());
        assert_eq!(json[0].sessions.len(), 1);
        assert_eq!(json[0].sessions[0].name, "backend");

        assert_eq!(json[1].host, "down");
        assert!(!json[1].ok);
        assert_eq!(json[1].error.as_deref(), Some("boom"));
        assert!(json[1].sessions.is_empty());

        // Serializes cleanly.
        let s = serde_json::to_string(&json).expect("serialize");
        assert!(s.contains("\"host\":\"ok\""));
        assert!(s.contains("\"ok\":false"));
    }

    #[test]
    fn parse_fleet_target_ok_and_err() {
        let t = parse_fleet_target("devbox:backend").unwrap();
        assert_eq!(t.host, "devbox");
        assert_eq!(t.session, "backend");
        assert!(parse_fleet_target("nohost").is_err());
        assert!(parse_fleet_target(":session").is_err());
        assert!(parse_fleet_target("host:").is_err());
    }

    #[test]
    fn resolve_host_found_and_missing() {
        let hosts = vec![
            host("devbox", "user@devbox", &[]),
            host("prod", "ops@prod", &[]),
        ];
        assert_eq!(resolve_host(&hosts, "devbox").unwrap().ssh, "user@devbox");
        let err = resolve_host(&hosts, "nope").unwrap_err();
        assert!(err.contains("nope"));
        assert!(err.contains("devbox"));

        let empty: Vec<FleetHost> = vec![];
        assert!(resolve_host(&empty, "x")
            .unwrap_err()
            .contains("no hosts configured"));
    }
}
