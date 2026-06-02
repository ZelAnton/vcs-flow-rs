//! A single full-screen "working…" frame, drawn while an async step (AI message
//! generation) runs. The caller owns the spinner state and the wait loop; this
//! just renders one frame with the supplied glyph and message.

use std::io;

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Paragraph;

use crate::ui::terminal::Tui;

/// Spinner frames cycled by the caller (`SPINNER[tick % SPINNER.len()]`).
pub const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

/// Draw a centered `<glyph> <msg>` line with a dim hint below it.
pub fn frame(tui: &mut Tui, glyph: char, msg: &str) -> io::Result<()> {
    tui.draw(|f| {
        let area = f.area();
        // Push the message roughly to the vertical middle.
        let rows = Layout::vertical([
            Constraint::Percentage(45),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

        f.render_widget(
            Paragraph::new(format!("{glyph} {msg}"))
                .centered()
                .style(Style::default().add_modifier(Modifier::BOLD)),
            rows[1],
        );
        f.render_widget(
            Paragraph::new("Esc to skip")
                .centered()
                .style(Style::default().fg(Color::DarkGray)),
            rows[2],
        );
    })?;
    Ok(())
}
