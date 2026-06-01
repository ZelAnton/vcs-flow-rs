//! Bookmark picker, shown for a jj repo when several bookmarks are equally near
//! `@`. Returns the chosen index, or `None` if cancelled.

use std::io;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::model::Target;
use crate::ui::terminal::Tui;

pub fn pick(tui: &mut Tui, targets: &[Target]) -> io::Result<Option<usize>> {
    let mut state = ListState::default();
    state.select(Some(0));

    loop {
        tui.draw(|frame| {
            let area = frame.area();
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

            frame.render_widget(
                Paragraph::new("Several bookmarks reach this change — pick the commit target:")
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                rows[0],
            );

            let items: Vec<ListItem> = targets
                .iter()
                .map(|t| ListItem::new(label_for(t)))
                .collect();
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(" Bookmarks "))
                .highlight_style(
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");
            frame.render_stateful_widget(list, rows[1], &mut state);

            frame.render_widget(
                Paragraph::new("↑↓ move   Enter select   Esc cancel")
                    .style(Style::default().fg(Color::DarkGray)),
                rows[2],
            );
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Up => {
                let i = state.selected().unwrap_or(0).saturating_sub(1);
                state.select(Some(i));
            }
            KeyCode::Down => {
                let i = (state.selected().unwrap_or(0) + 1).min(targets.len().saturating_sub(1));
                state.select(Some(i));
            }
            KeyCode::Enter => return Ok(state.selected()),
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            _ => {}
        }
    }
}

fn label_for(t: &Target) -> String {
    if t.label.is_empty() {
        "(commit without moving a bookmark)".to_string()
    } else if let Some(rev) = &t.revision {
        format!("{}  ({rev})", t.label)
    } else {
        t.label.clone()
    }
}
