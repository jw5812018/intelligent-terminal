use ratatui::prelude::*;

use crate::app::{App, AppMode};

use super::{chat, debug_panel, input, notification_banner, permission, recommendations, setup, status_bar};

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();

    // If in Setup mode, render the setup wizard full-screen
    if app.mode == AppMode::Setup {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // status bar
                Constraint::Min(1),   // setup wizard
            ])
            .split(area);
        status_bar::render(frame, app, chunks[0]);
        setup::render(frame, app, chunks[1]);
        return;
    }

    // Split horizontally if debug panel is visible
    let (main_area, debug_area) = if app.show_debug_panel {
        let h = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area);
        (h[0], Some(h[1]))
    } else {
        (area, None)
    };

    let banner_h = notification_banner::banner_height(app);

    let recommendations_height = if app.recommendations.is_some() {
        Constraint::Length(8)
    } else {
        Constraint::Length(0)
    };

    let input_height = input::input_height(&app.input, app.cursor_pos, main_area.width);

    // Layout: status bar | notification banner | recommendations | chat | input
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),         // status bar
            Constraint::Length(banner_h),  // notification banner
            recommendations_height,
            Constraint::Min(1),            // chat area
            Constraint::Length(input_height),
        ])
        .split(main_area);

    status_bar::render(frame, app, chunks[0]);
    notification_banner::render(frame, app, chunks[1]);
    recommendations::render(frame, app, chunks[2]);
    chat::render(frame, app, chunks[3]);
    input::render(frame, app, chunks[4]);

    // Debug panel (right side)
    if let Some(debug_area) = debug_area {
        debug_panel::render(frame, app, debug_area);
    }

    // Permission modal overlay (rendered last, on top)
    if app.permission.is_some() {
        permission::render(frame, app, area);
    }
}

pub fn input_cursor_position(app: &App, area: Rect) -> Option<Position> {
    // No cursor in setup mode
    if app.mode == AppMode::Setup {
        return None;
    }

    let main_area = if app.show_debug_panel {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(area)[0]
    } else {
        area
    };

    let banner_h = notification_banner::banner_height(app);

    let recommendations_height = if app.recommendations.is_some() {
        Constraint::Length(8)
    } else {
        Constraint::Length(0)
    };

    let input_height = input::input_height(&app.input, app.cursor_pos, main_area.width);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(banner_h),
            recommendations_height,
            Constraint::Min(1),
            Constraint::Length(input_height),
        ])
        .split(main_area);

    input::cursor_position(app, chunks[4])
}
