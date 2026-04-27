use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::app::{App, ConnectionState};
use crate::theme;

pub const HEIGHT: u16 = 1;

pub fn render(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(4)])
        .split(area);

    // ── Left: [●] AgentName [version] [∨] ───────────────────────────────────
    let display_name = if app.agent_name.is_empty() {
        "Agent".to_string()
    } else {
        app.agent_name.clone()
    };

    let (dot, dot_style) = match &app.state {
        ConnectionState::Connected => ("●", theme::STATUS_CONNECTED),
        ConnectionState::Connecting(_) => ("●", theme::STATUS_CONNECTING),
        ConnectionState::Failed(_) => ("●", theme::STATUS_FAILED),
        ConnectionState::Disconnected => ("●", theme::STATUS_DISCONNECTED),
    };

    let label = if let Some(ver) = &app.agent_version {
        format!("{display_name} {ver}")
    } else if let Some(model) = &app.agent_model {
        if !model.is_empty() {
            format!("{display_name} {model}")
        } else {
            display_name
        }
    } else {
        display_name
    };

    let spans = vec![
        Span::raw(" "),
        Span::styled(dot, dot_style),
        Span::raw(" "),
        Span::styled(label, Style::new().fg(Color::White)),
        Span::styled(" ∨", theme::DIM),
    ];

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(theme::PANEL_STYLE),
        chunks[0],
    );

    // ── Right: history button ────────────────────────────────────────────────
    frame.render_widget(
        Paragraph::new(" ↺  ").style(theme::PANEL_STYLE),
        chunks[1],
    );
}
