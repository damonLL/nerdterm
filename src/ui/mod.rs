pub mod address_book;
pub mod popup;
pub mod terminal_view;

use ratatui::Frame;

use crate::app::{App, AppState};

pub fn draw(f: &mut Frame, app: &mut App) {
    match app.state {
        AppState::AddressBook | AppState::Connecting => {
            address_book::draw(f, app);
            if let Some(ref popup_state) = app.popup {
                popup::draw(f, popup_state);
            }
        }
        AppState::Connected => {
            terminal_view::draw(f, app);
            if let Some(ref popup_state) = app.popup {
                popup::draw(f, popup_state);
            }
        }
    }
}
