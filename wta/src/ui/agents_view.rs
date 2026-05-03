use ratatui::{
    layout::Rect,
    style::{Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};
use std::time::SystemTime;

use crate::agent_sessions::{AgentSession, AgentSessionRegistry, AgentStatus};

pub fn render(
    f:    &mut Frame,
    area: Rect,
    reg:  &AgentSessionRegistry,
    list_state: &mut ListState,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Agents  (F2 / Ctrl+Tab to switch · ↑↓ select · Enter activate · Del remove) ");

    let rows: Vec<ListItem> = reg.iter_sorted().into_iter().map(row_for).collect();
    let list = List::new(rows)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, list_state);
}

fn row_for(s: &AgentSession) -> ListItem<'static> {
    let title  = format!("{} — {}", cli_label(s), cwd_basename(s));
    let status = status_label(s);
    let age    = relative_age(s.last_activity_at);

    let dim = matches!(s.status, AgentStatus::Ended | AgentStatus::Historical);
    let title_style  = if dim { Style::default().dim() } else { Style::default() };
    let status_style = match s.status {
        AgentStatus::Working   => Style::default().yellow(),
        AgentStatus::Attention => Style::default().magenta(),
        AgentStatus::Error     => Style::default().red(),
        _ => Style::default(),
    };

    let line = Line::from(vec![
        Span::styled(format!("{:<32}", trunc(&title, 32)), title_style),
        Span::raw("  "),
        Span::styled(format!("{:<10}", status), status_style),
        Span::raw("  "),
        Span::styled(format!("{:>4}", age), Style::default().dim()),
    ]);
    ListItem::new(line)
}

fn cli_label(s: &AgentSession) -> &'static str {
    use crate::agent_sessions::CliSource::*;
    match s.cli_source {
        Claude  => "claude",
        Copilot => "copilot",
        Gemini  => "gemini",
        _       => "agent",
    }
}

fn cwd_basename(s: &AgentSession) -> String {
    s.cwd.file_name().and_then(|n| n.to_str())
        .unwrap_or("?")
        .to_string()
}

fn status_label(s: &AgentSession) -> &'static str {
    match s.status {
        AgentStatus::Idle       => "IDLE",
        AgentStatus::Working    => "WORKING",
        AgentStatus::Attention  => "ATTENTION",
        AgentStatus::Error      => "ERROR",
        AgentStatus::Ended      => "",
        AgentStatus::Historical => "",
    }
}

fn relative_age(t: SystemTime) -> String {
    let secs = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    if secs < 60        { format!("{}s",  secs) }
    else if secs < 3600 { format!("{}m",  secs / 60) }
    else if secs < 86400{ format!("{}h",  secs / 3600) }
    else                { format!("{}d",  secs / 86400) }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() }
    else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}
