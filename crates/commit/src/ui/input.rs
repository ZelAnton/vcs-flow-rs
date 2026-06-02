//! A single-line text prompt (e.g. "enter another model"). Built on the same
//! `tui-textarea` widget as the message editor, but one line: `Enter` confirms,
//! `Esc` cancels. Returns the trimmed entry, or `None` if cancelled or empty.

use std::io;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use tui_textarea::TextArea;

use crate::ui::terminal::Tui;

pub fn run(tui: &mut Tui, title: &str, initial: &str) -> io::Result<Option<String>> {
    let mut textarea = TextArea::new(vec![initial.to_string()]);
    // The caller's question is the heading above; keep the box label generic so it
    // doesn't duplicate it.
    textarea.set_block(Block::default().borders(Borders::ALL).title(" Input "));
    textarea.set_cursor_line_style(Style::default());
    // Start the cursor at the end of any seeded text.
    textarea.move_cursor(tui_textarea::CursorMove::End);

    loop {
        tui.draw(|frame| {
            let area = frame.area();
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

            frame.render_widget(
                Paragraph::new(title.to_string())
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                rows[0],
            );
            frame.render_widget(&textarea, rows[1]);
            frame.render_widget(
                Paragraph::new("Enter confirm    Esc cancel")
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
            KeyCode::Enter => {
                let text = textarea.lines().join(" ");
                let trimmed = text.trim();
                return Ok((!trimmed.is_empty()).then(|| trimmed.to_string()));
            }
            KeyCode::Esc => return Ok(None),
            // Swallow newlines so the field stays single-line; pass the rest on.
            _ => {
                textarea.input(key);
            }
        }
    }
}
