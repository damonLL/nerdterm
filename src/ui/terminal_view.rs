use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};
use tui_term::widget::PseudoTerminal;

use crate::app::{App, InputMode};

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let char_mode = app.input_mode == InputMode::Character;

    let chunks = if char_mode {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),    // terminal (full height)
                Constraint::Length(1), // status bar
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),    // terminal
                Constraint::Length(1), // status bar
                Constraint::Length(3), // input bar
            ])
            .split(area)
    };

    let term_area = chunks[0];
    app.emulator.resize(term_area.height, term_area.width);

    // RAII guard: reverts to live view when it drops at end of scope.
    {
        let view = app.emulator.scroll_view();
        let pseudo_term = PseudoTerminal::new(view.screen());
        f.render_widget(pseudo_term, term_area);
    }

    // Status bar — a single Line of spans so we can color the ● REC badge red.
    let scroll_info = if app.emulator.scroll_offset() > 0 {
        format!(" [scroll: -{}]", app.emulator.scroll_offset())
    } else {
        String::new()
    };
    let mode_label = if char_mode { "CHAR" } else { "LINE" };
    let rest = format!(
        " {}{} | [{}] Tab: mode | Shift+PgUp/Dn: scroll | Esc: disconnect",
        app.status_message, scroll_info, mode_label,
    );

    let mut spans: Vec<Span> = Vec::new();
    if app.capture.is_some() {
        spans.push(Span::styled(
            " ● REC ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::raw(rest));

    let status = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    f.render_widget(status, chunks[1]);

    // Input bar (line-buffered mode only)
    if !char_mode {
        let input = Paragraph::new(format!("> {}", app.input))
            .block(Block::default().borders(Borders::ALL).title(" Input "));
        f.render_widget(input, chunks[2]);

        let cursor_x = (chunks[2].x + 3 + app.input.len() as u16)
            .min(chunks[2].x + chunks[2].width.saturating_sub(1));
        f.set_cursor_position((cursor_x, chunks[2].y + 1));
    }
}
