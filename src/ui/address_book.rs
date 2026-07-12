use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::app::App;

/// Format one address-book row (connected marker, protocol, name, host:port).
/// Pure helper so list rendering stays testable without a full TUI.
pub(crate) fn format_entry_line(
    connected: bool,
    protocol: crate::app::Protocol,
    name: &str,
    host: &str,
    port: u16,
) -> String {
    let prefix = if connected { "* " } else { "  " };
    let proto_tag = match protocol {
        crate::app::Protocol::Telnet => "[TEL]",
        crate::app::Protocol::Ssh => "[SSH]",
    };
    format!("{}{} {} - {}:{}", prefix, proto_tag, name, host, port)
}

/// Render a menu item with the hotkey highlighted.
fn menu_item<'a>(key: &'a str, label: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!(" {}", key),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {} ", label), Style::default().fg(Color::White)),
    ])
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    let status_lines = if app.status_message.is_empty() {
        1
    } else {
        (app.status_message.lines().count().clamp(1, 8) as u16) + 2
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),            // title
            Constraint::Min(5),               // list
            Constraint::Length(1),            // menu bar
            Constraint::Length(status_lines), // status
        ])
        .split(area);

    // Title
    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            "Nerd",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "Term",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .alignment(Alignment::Center)
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    // Address book list
    let items: Vec<ListItem> = app
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let connected = app.connected_entry == Some(i);
            let text = format_entry_line(connected, e.protocol, &e.name, &e.host, e.port);
            if connected {
                ListItem::new(text).style(Style::default().fg(Color::Green))
            } else {
                ListItem::new(text)
            }
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Address Book "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");

    let mut list_state = ListState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(list, chunks[1], &mut list_state);

    // Menu bar — show context-sensitive options
    let mut menu_items: Vec<Vec<Span>> = vec![
        menu_item(
            "Enter",
            if app.connected_entry == Some(app.selected) {
                "Resume"
            } else {
                "Connect"
            },
        )
        .spans,
        menu_item("A", "Add").spans,
        menu_item("E", "Edit").spans,
        menu_item("D", "Delete").spans,
        menu_item("S", "Settings").spans,
    ];
    if app.connected_entry.is_some() {
        menu_items.push(menu_item("^D", "Disconnect").spans);
    }
    menu_items.push(menu_item("Q", "Quit").spans);

    let menu = Line::from(menu_items.into_iter().flatten().collect::<Vec<Span>>())
        .alignment(Alignment::Center);

    let menu_bar = Paragraph::new(menu).style(Style::default().bg(Color::DarkGray));
    f.render_widget(menu_bar, chunks[2]);

    // Status line
    let status_text: &str = &app.status_message;

    if !status_text.is_empty() {
        let alignment = if status_text.contains('\n') {
            Alignment::Left
        } else {
            Alignment::Center
        };
        let status = Paragraph::new(status_text.to_owned())
            .alignment(alignment)
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            )
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(status, chunks[3]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Protocol;

    #[test]
    fn format_entry_line_marks_connected_and_protocol() {
        assert_eq!(
            format_entry_line(true, Protocol::Ssh, "modernbbs", "localhost", 2222),
            "* [SSH] modernbbs - localhost:2222"
        );
        assert_eq!(
            format_entry_line(false, Protocol::Telnet, "mud", "example.com", 4000),
            "  [TEL] mud - example.com:4000"
        );
    }
}
