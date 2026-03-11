use ratatui::prelude::*;

use crate::app::App;

use super::{chat, input, permission, status_bar};

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // Layout: status bar (1 line) | chat (fill) | input (3 lines)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // status bar
            Constraint::Min(1),    // chat area
            Constraint::Length(3), // input box
        ])
        .split(area);

    status_bar::render(frame, app, chunks[0]);
    chat::render(frame, app, chunks[1]);
    input::render(frame, app, chunks[2]);

    // Permission modal overlay (rendered last, on top)
    if app.permission.is_some() {
        permission::render(frame, app, area);
    }
}
