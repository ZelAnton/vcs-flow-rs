//! Commit-message editor screen (multi-line, `tui-textarea`). Pre-filled with the
//! target's current message. Returns the edited message, or `None` if cancelled.

use std::io;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph};
use tui_textarea::TextArea;

use crate::ui::is_confirm;
use crate::ui::terminal::Tui;

pub fn run(tui: &mut Tui, initial: &str, header: &str) -> io::Result<Option<String>> {
    let mut textarea = TextArea::new(initial.split('\n').map(str::to_string).collect());
    textarea.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Commit message "),
    );
    textarea.set_cursor_line_style(Style::default());
    textarea.set_placeholder_text("Describe the change…");

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
                Paragraph::new(header.to_string())
                    .style(Style::default().add_modifier(Modifier::BOLD)),
                rows[0],
            );
            frame.render_widget(&textarea, rows[1]);
            frame.render_widget(
                Paragraph::new("Ctrl+Enter / Ctrl+S commit    Esc cancel")
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
        if is_confirm(&key) {
            return Ok(Some(textarea.lines().join("\n").trim_end().to_string()));
        }
        if key.code == KeyCode::Esc {
            return Ok(None);
        }
        textarea.input(key);
    }
}
