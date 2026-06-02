//! Terminal UI: the file-select, bookmark-menu, and message-editor screens, plus
//! the terminal lifecycle and diff highlighting.

pub mod busy;
pub mod diff;
pub mod editor;
pub mod input;
pub mod menu;
pub mod select;
pub mod terminal;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// True for the commit-confirm chord: `Ctrl+Enter` (where the terminal reports
/// it) or the universal `Ctrl+S` fallback.
pub fn is_confirm(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(
            key.code,
            KeyCode::Enter | KeyCode::Char('s') | KeyCode::Char('S')
        )
}
