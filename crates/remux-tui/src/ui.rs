use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::app::App;

/// Draw the full TUI layout.
pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),     // title bar
            Constraint::Min(5),        // session table
            Constraint::Length(8),     // scrollback preview
            Constraint::Length(1),     // footer
        ])
        .split(f.size());

    draw_title_bar(f, app, chunks[0]);
    draw_session_table(f, app, chunks[1]);
    draw_scrollback_preview(f, app, chunks[2]);
    draw_footer(f, chunks[3]);
}

fn draw_title_bar(f: &mut Frame, app: &App, area: Rect) {
    let count = app.sessions.len();
    let title = format!(
        " Remux Session Manager  [{} session{}]",
        count,
        if count == 1 { "" } else { "s" }
    );
    let title_bar = Paragraph::new(Span::styled(
        title,
        Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(title_bar, area);
}

fn draw_session_table(f: &mut Frame, app: &App, area: Rect) {
    let header = Row::new(vec![
        Cell::from("Name").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Status").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Command").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Created").style(Style::default().add_modifier(Modifier::BOLD)),
    ])
    .style(Style::default().bg(Color::DarkGray))
    .height(1)
    .bottom_margin(0);

    let rows: Vec<Row> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, session)| {
            let status_style = match session.status {
                remux_core::SessionStatus::Starting => Style::default().fg(Color::Cyan),
                remux_core::SessionStatus::Running => Style::default().fg(Color::Green),
                remux_core::SessionStatus::Exited => Style::default().fg(Color::Yellow),
                remux_core::SessionStatus::Failed => Style::default().fg(Color::Red),
            };

            let cmd = session.command.join(" ");
            let created = session.created_at.format("%Y-%m-%d %H:%M").to_string();

            let row = Row::new(vec![
                Cell::from(session.name.as_str()),
                Cell::from(format!("{:?}", session.status)).style(status_style),
                Cell::from(truncate_str(&cmd, 40)),
                Cell::from(created),
            ]);

            if i == app.selected {
                row.style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::REVERSED),
                )
            } else {
                row
            }
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(25),
            Constraint::Percentage(15),
            Constraint::Percentage(35),
            Constraint::Percentage(25),
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::NONE))
    .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    // We manage selection highlighting ourselves in the row style,
    // but also set the table state for accessibility.
    let mut table_state = TableState::default();
    table_state.select(Some(app.selected));
    f.render_stateful_widget(table, area, &mut table_state);
}

fn draw_scrollback_preview(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .title(" Scrollback Preview ");

    let inner = block.inner(area);
    f.render_widget(block, area);

    let text = match &app.scrollback_preview {
        Some(data) => {
            let content = String::from_utf8_lossy(data);
            let lines: Vec<&str> = content.lines().rev().take(inner.height as usize).collect();
            let display: Vec<Line> = lines
                .into_iter()
                .rev()
                .map(|l| Line::from(truncate_str(l, inner.width as usize)))
                .collect();
            Paragraph::new(display)
        }
        None => Paragraph::new(Line::from(Span::styled(
            "  No session selected or no scrollback available",
            Style::default().fg(Color::DarkGray),
        ))),
    };

    f.render_widget(Clear, inner);
    f.render_widget(text, inner);
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(
            " Enter",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("=Attach "),
        Span::styled(
            "k",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("=Kill "),
        Span::styled(
            "r",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("=Refresh "),
        Span::styled(
            "Ctrl-Q",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("=Quit "),
        Span::styled(
            "\u{2191}\u{2193}",
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("=Navigate"),
    ]))
    .style(Style::default().bg(Color::DarkGray));
    f.render_widget(footer, area);
}

/// Truncate a string to fit within `max_len` visible characters, appending "..." if needed.
fn truncate_str(s: &str, max_len: usize) -> String {
    if max_len < 4 {
        return s.chars().take(max_len).collect();
    }
    if s.chars().count() <= max_len {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max_len - 3).collect();
    format!("{truncated}...")
}
