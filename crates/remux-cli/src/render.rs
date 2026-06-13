use chrono::{DateTime, Utc};
use remux_core::{SessionDetails, SessionSummary};

/// Render a list of sessions as a formatted table.
pub fn render_session_list(sessions: &[SessionSummary]) {
    if sessions.is_empty() {
        println!("No sessions found.");
        return;
    }

    // Header
    println!(
        "{:<20} {:<10} {:<8} {:<14} {:<30} CMD",
        "NAME", "STATUS", "PID", "CREATED", "CWD"
    );

    for session in sessions {
        let pid_str = match session.pid {
            Some(p) => p.to_string(),
            None => "-".to_string(),
        };
        let created = format_duration(session.created_at);
        let cmd = session.command.join(" ");
        let cwd_display = session.cwd.display().to_string();
        // Truncate long cwd for display
        let cwd_short = if cwd_display.len() > 28 {
            format!("...{}", &cwd_display[cwd_display.len() - 25..])
        } else {
            cwd_display
        };

        println!(
            "{:<20} {:<10} {:<8} {:<14} {:<30} {}",
            session.name,
            format!("{:?}", session.status),
            pid_str,
            created,
            cwd_short,
            cmd
        );
    }
}

/// Render full session details in human-readable format.
pub fn render_session_details(details: &SessionDetails) {
    let pid_str = match details.pid {
        Some(p) => p.to_string(),
        None => "-".to_string(),
    };
    let exit_str = match details.last_exit_code {
        Some(c) => c.to_string(),
        None => "-".to_string(),
    };
    let controller = details
        .controlling_client
        .as_ref()
        .map(|c| c.0.to_string())
        .unwrap_or_else(|| "-".to_string());
    let attached_count = details.attached_clients.len();

    println!("Session: {}", details.name);
    println!("  ID:        {}", details.id.0);
    println!("  Status:    {:?}", details.status);
    println!("  PID:       {pid_str}");
    println!("  Command:   {}", details.command.join(" "));
    println!("  CWD:       {}", details.cwd.display());
    println!("  Created:   {}", details.created_at);
    println!("  Updated:   {}", details.updated_at);
    println!("  Exit code: {exit_str}");
    println!(
        "  Size:      {}x{}",
        details.last_size.cols, details.last_size.rows
    );
    println!("  Controller: {controller}");
    println!("  Attached:  {attached_count} client(s)");
}

/// Format a timestamp as a human-readable duration (e.g., "3h ago", "22m ago").
pub fn format_duration(since: DateTime<Utc>) -> String {
    let now = Utc::now();
    let diff = now.signed_duration_since(since);
    let secs = diff.num_seconds();

    if secs < 0 {
        return "just now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = diff.num_minutes();
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = diff.num_hours();
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = diff.num_days();
    format!("{days}d ago")
}
