//! Render a unified diff to a styled ratatui `Text`: syntect syntax highlighting
//! for the code, layered with green/red line tints for added/removed lines and a
//! cyan hunk header — the right-pane view of a selected file.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Owns the syntax + theme sets (loaded once) and renders diffs.
pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl Highlighter {
    pub fn new() -> Self {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let mut themes = ThemeSet::load_defaults();
        // base16-ocean.dark ships with syntect's defaults; fall back to any theme.
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .or_else(|| themes.themes.values().next().cloned())
            .unwrap_or_default();
        Highlighter { syntaxes, theme }
    }

    /// Render `diff` (a single file's unified diff) for `path`. Returns an owned
    /// `Text` so it can be cached.
    pub fn render(&self, path: &str, diff: &str) -> Text<'static> {
        let syntax = path
            .rsplit('.')
            .next()
            .and_then(|ext| self.syntaxes.find_syntax_by_extension(ext))
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());

        let added_bg = Color::Rgb(20, 42, 20);
        let removed_bg = Color::Rgb(48, 20, 20);

        let mut lines: Vec<Line<'static>> = Vec::new();
        for raw in diff.lines() {
            // Headers: render verbatim with a distinct style, no syntax pass.
            if let Some(style) = header_style(raw) {
                lines.push(Line::from(Span::styled(raw.to_string(), style)));
                continue;
            }

            let (sign, bg) = match raw.as_bytes().first() {
                Some(b'+') => ('+', Some(added_bg)),
                Some(b'-') => ('-', Some(removed_bg)),
                _ => (' ', None),
            };
            let code = raw.get(1..).unwrap_or("");

            // Highlight each line independently: robust against +/- interleaving
            // corrupting a stateful highlighter, and diffs are small.
            let mut hl = HighlightLines::new(syntax, &self.theme);
            let ranges = hl.highlight_line(code, &self.syntaxes).unwrap_or_default();

            let sign_style = match sign {
                '+' => Style::default().fg(Color::Green).bg(added_bg),
                '-' => Style::default().fg(Color::Red).bg(removed_bg),
                _ => Style::default().fg(Color::DarkGray),
            };
            let mut spans = vec![Span::styled(sign.to_string(), sign_style)];
            for (syn, text) in ranges {
                let fg = Color::Rgb(syn.foreground.r, syn.foreground.g, syn.foreground.b);
                let mut style = Style::default().fg(fg);
                if let Some(bg) = bg {
                    style = style.bg(bg);
                }
                spans.push(Span::styled(text.to_string(), style));
            }
            lines.push(Line::from(spans));
        }

        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "(no diff — binary or empty change)",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
        Text::from(lines)
    }
}

/// Style for a diff metadata line, or `None` if it's content to be highlighted.
fn header_style(line: &str) -> Option<Style> {
    if line.starts_with("@@") {
        return Some(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    }
    const META: [&str; 8] = [
        "diff --git ",
        "index ",
        "--- ",
        "+++ ",
        "new file",
        "deleted file",
        "rename ",
        "similarity ",
    ];
    if META.iter().any(|p| line.starts_with(p)) {
        return Some(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    }
    None
}
