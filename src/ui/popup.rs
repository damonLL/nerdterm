use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::app::{FormMode, FormPopup, Popup, PopupField};

/// Column width reserved for the label column ("Name:", "Pass:", etc.) in
/// every popup form field. Keep all draw_field calls consistent.
const LABEL_WIDTH: u16 = 6;

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

pub fn draw(f: &mut Frame, popup: &Popup) {
    match popup {
        Popup::DeleteConfirm => draw_delete_confirm(f),
        Popup::Password(input) => draw_password_prompt(f, input),
        Popup::Form(form) => draw_form(f, form),
        Popup::EditSettings(_) => {
            // Rendering implemented in Task 7. The popup is still
            // unreachable from the UI today (Task 8 wires the hotkey),
            // so a no-op draw is safe until then.
        }
        Popup::HostKeyTrust(p) => draw_host_key_trust(f, p),
        Popup::ChordHelp => draw_chord_help(f),
    }
}

fn draw_chord_help(f: &mut Frame) {
    let area = centered_rect(48, 9, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Local commands (Ctrl+] then…) ")
        .border_style(Style::default().fg(Color::Cyan));

    let text = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "l",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("    Toggle session capture (log)"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "?",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("    This help"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Esc", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" or any key to dismiss"),
        ]),
    ];

    let paragraph = Paragraph::new(text).block(block).alignment(Alignment::Left);
    f.render_widget(paragraph, area);
}

fn draw_host_key_trust(f: &mut Frame, popup: &crate::app::HostKeyTrustPopup) {
    let area = centered_rect(64, 11, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" New host key ")
        .border_style(Style::default().fg(Color::Yellow));

    let intro = format!(
        "The server at {}:{} presented a key we haven't seen before:",
        popup.host, popup.port,
    );

    let text = vec![
        Line::from(intro),
        Line::from(""),
        Line::from(format!("  Type:        {}", popup.key_type)),
        Line::from(format!("  Fingerprint: {}", popup.fingerprint)),
        Line::from(""),
        Line::from(vec![
            Span::raw("Trust and connect? ["),
            Span::styled(
                "y",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("/"),
            Span::styled(
                "N",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw("]"),
        ]),
    ];

    let paragraph = Paragraph::new(text).block(block).alignment(Alignment::Left);
    f.render_widget(paragraph, area);
}

fn draw_delete_confirm(f: &mut Frame) {
    let area = centered_rect(40, 5, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Delete Entry ")
        .border_style(Style::default().fg(Color::Red));

    let text = vec![
        Line::from("Delete this entry?"),
        Line::from(vec![
            Span::styled(
                " Y ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw("Yes  "),
            Span::styled(
                " N ",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("No"),
        ]),
    ];

    let paragraph = Paragraph::new(text)
        .block(block)
        .alignment(Alignment::Center);
    f.render_widget(paragraph, area);
}

fn draw_password_prompt(f: &mut Frame, password_input: &str) {
    let area = centered_rect(50, 6, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" SSH Password ")
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // password field
            Constraint::Length(1), // help
        ])
        .split(inner);

    // Password field (masked)
    let masked: String = "*".repeat(password_input.len());
    draw_field(f, "Pass:", &masked, true, chunks[0]);

    // Cursor after the masked chars
    let input_x = chunks[0].x + LABEL_WIDTH + password_input.len() as u16;
    f.set_cursor_position((input_x, chunks[0].y));

    let help = Paragraph::new(Line::from(vec![
        Span::styled("Enter", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" submit  "),
        Span::styled("Esc", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" cancel"),
    ]))
    .alignment(Alignment::Center)
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(help, chunks[1]);
}

fn draw_form(f: &mut Frame, popup: &FormPopup) {
    let title = match popup.mode {
        FormMode::Add => " Add Entry ",
        FormMode::Edit => " Edit Entry ",
    };

    let area = centered_rect(50, 15, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let fields = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // name
            Constraint::Length(2), // host
            Constraint::Length(2), // port
            Constraint::Length(2), // protocol
            Constraint::Length(2), // username
            Constraint::Length(1), // help
        ])
        .split(inner);

    draw_field(
        f,
        "Name:",
        &popup.name,
        popup.focused == PopupField::Name,
        fields[0],
    );
    draw_field(
        f,
        "Host:",
        &popup.host,
        popup.focused == PopupField::Host,
        fields[1],
    );
    draw_field(
        f,
        "Port:",
        &popup.port_str,
        popup.focused == PopupField::Port,
        fields[2],
    );

    // Protocol toggle field
    let proto_active = popup.focused == PopupField::Protocol;
    let proto_style = if proto_active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let proto_value_style = if proto_active {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    } else {
        Style::default()
    };
    let proto_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(LABEL_WIDTH), Constraint::Min(1)])
        .split(fields[3]);
    let proto_label = Paragraph::new("Type:").style(proto_style);
    f.render_widget(
        proto_label,
        Rect::new(
            proto_chunks[0].x,
            proto_chunks[0].y,
            proto_chunks[0].width,
            1,
        ),
    );
    let proto_text = format!("{} (Space to toggle)", popup.protocol);
    let proto_widget = Paragraph::new(proto_text).style(proto_value_style);
    f.render_widget(
        proto_widget,
        Rect::new(
            proto_chunks[1].x,
            proto_chunks[1].y,
            proto_chunks[1].width,
            1,
        ),
    );

    draw_field(
        f,
        "User:",
        &popup.username,
        popup.focused == PopupField::Username,
        fields[4],
    );

    let help = Paragraph::new(Line::from(vec![
        Span::styled("Tab", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" next  "),
        Span::styled("Enter", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" save  "),
        Span::styled("Esc", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" cancel"),
    ]))
    .alignment(Alignment::Center)
    .style(Style::default().fg(Color::DarkGray));
    f.render_widget(help, fields[5]);
}

fn draw_field(f: &mut Frame, label: &str, value: &str, active: bool, area: Rect) {
    let style = if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(LABEL_WIDTH), Constraint::Min(1)])
        .split(area);

    let label_widget = Paragraph::new(label).style(style);
    f.render_widget(
        label_widget,
        Rect::new(chunks[0].x, chunks[0].y, chunks[0].width, 1),
    );

    let input_style = if active {
        Style::default().bg(Color::DarkGray).fg(Color::White)
    } else {
        Style::default()
    };

    let display = if value.is_empty() && !active {
        " ".to_string()
    } else {
        format!("{} ", value)
    };

    let input_widget = Paragraph::new(display).style(input_style);
    let input_area = Rect::new(chunks[1].x, chunks[1].y, chunks[1].width, 1);
    f.render_widget(input_widget, input_area);

    if active {
        f.set_cursor_position((input_area.x + value.len() as u16, input_area.y));
    }
}
