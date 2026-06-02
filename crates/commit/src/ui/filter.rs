//! A filterable list picker — used to choose an existing remote branch to push to
//! when the local branch has no same-named remote. Type to narrow the list.
//!
//! `Enter` picks the highlighted branch, `Ctrl+N` pushes as a new same-named branch,
//! `Esc` cancels the push.

use std::io;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::ui::terminal::Tui;

/// What the user chose in the picker.
pub enum Pick {
    /// Attach to / push to this existing remote branch.
    Existing(String),
    /// Push as a new same-named remote branch.
    NewBranch,
    /// Cancel the push.
    Cancel,
}

/// Case-insensitive substring match (the filter rule). Empty filter matches all.
fn matches(item: &str, filter: &str) -> bool {
    filter.is_empty() || item.to_lowercase().contains(&filter.to_lowercase())
}

pub fn run(tui: &mut Tui, title: &str, items: &[String], new_name: &str) -> io::Result<Pick> {
    let mut filter = String::new();
    let mut state = ListState::default();
    state.select(Some(0));

    loop {
        // Recompute the visible subset each frame so it tracks the filter.
        let visible: Vec<&String> = items.iter().filter(|i| matches(i, &filter)).collect();
        // Keep the selection inside the (possibly shrunken) visible range.
        let selected = match state.selected() {
            Some(i) if i < visible.len() => Some(i),
            _ if visible.is_empty() => None,
            _ => Some(visible.len() - 1),
        };
        state.select(selected);

        tui.draw(|frame| {
            let area = frame.area();
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

            frame.render_widget(
                Paragraph::new(title.to_string())
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                rows[0],
            );

            let list_items: Vec<ListItem> = visible
                .iter()
                .map(|b| ListItem::new((*b).clone()))
                .collect();
            let list = List::new(list_items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Remote branches "),
                )
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");
            frame.render_stateful_widget(list, rows[1], &mut state);

            frame.render_widget(
                Paragraph::new(format!("filter: {filter}"))
                    .style(Style::default().fg(Color::Yellow)),
                rows[2],
            );
            frame.render_widget(
                Paragraph::new(format!(
                    "Enter use highlighted   Ctrl+N new '{new_name}'   Esc cancel"
                ))
                .style(Style::default().fg(Color::DarkGray)),
                rows[3],
            );
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('n' | 'N') if ctrl => return Ok(Pick::NewBranch),
            KeyCode::Char('c') if ctrl => return Ok(Pick::Cancel),
            KeyCode::Esc => return Ok(Pick::Cancel),
            KeyCode::Enter => {
                return Ok(match selected.and_then(|i| visible.get(i)) {
                    Some(b) => Pick::Existing((*b).clone()),
                    None => Pick::Cancel, // nothing matches the filter
                });
            }
            KeyCode::Up => {
                let i = state.selected().unwrap_or(0).saturating_sub(1);
                state.select(Some(i));
            }
            KeyCode::Down => {
                let max = visible.len().saturating_sub(1);
                let i = (state.selected().unwrap_or(0) + 1).min(max);
                state.select(Some(i));
            }
            KeyCode::Backspace => {
                filter.pop();
            }
            KeyCode::Char(c) if !ctrl => {
                filter.push(c);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_is_case_insensitive_substring() {
        assert!(matches("origin/Feature-X", "feat"));
        assert!(matches("main", ""));
        assert!(!matches("main", "release"));
        assert!(matches("Release-1.2", "1.2"));
    }
}
