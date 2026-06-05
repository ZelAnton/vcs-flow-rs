//! The PR diff-review screen: the branch-vs-base changed files as a
//! path-compressed tree with tri-state checkboxes (revert marks) on the left,
//! and the selected file's highlighted diff (or a folder's children) on the
//! right — the same layout as the file-select screen, reusing its row/detail
//! builders.
//!
//! `r` asks to bulk-revert the marked paths; the screen itself never mutates
//! the repo — it returns the request and the caller (`pr.rs`) performs the
//! backup + revert, then re-enters with a refreshed snapshot.

use std::collections::HashMap;
use std::io;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use tui_tree_widget::{Tree, TreeState};

use crate::tree::TreeModel;
use crate::ui::diff::Highlighter;
use crate::ui::select::{build_detail, items_of, open_chains};
use crate::ui::terminal::Tui;
use crate::vcs::Snapshot;

/// What the user asked for on exit.
pub enum ReviewOutcome {
    /// Back to the create-PR question; nothing to do.
    Back,
    /// Revert these (repo-relative) paths to the base-branch state. Carries the
    /// marked files' new + old (rename) paths, as `TreeModel::selected_paths`
    /// emits them.
    RevertMarked { paths: Vec<String> },
}

/// Run the screen until the user confirms a revert (`r`, then `y`) or goes back
/// (`Esc`/`q`/`Ctrl+C`).
pub fn run(
    tui: &mut Tui,
    snapshot: &Snapshot,
    tree: &mut TreeModel,
    branch: &str,
    base: &str,
    highlighter: &Highlighter,
) -> io::Result<ReviewOutcome> {
    let mut state: TreeState<String> = TreeState::default();
    for chain in open_chains(&tree.roots) {
        state.open(chain);
    }
    if let Some(first) = tree.roots.first() {
        state.select(vec![first.id()]);
    }
    // Marks start cleared — reverting is opt-in, unlike committing where the
    // build-time default (everything selected) is the common case.
    tree.set_all(false);

    let total = snapshot.changes.len();
    let mut diff_cache: HashMap<String, Text<'static>> = HashMap::new();
    let mut detail_scroll: u16 = 0;
    // Visible line count of the diff pane (inner height), captured each draw so
    // PageDown can clamp to "last line at the bottom" instead of overscrolling.
    let mut detail_view_height: u16 = 0;
    let mut last_selected: Option<String> = None;
    // `r` arms this; the footer turns into a y/N confirmation until answered.
    let mut confirming = false;

    loop {
        let items = items_of(tree, &tree.roots);
        let selected_path = state.selected().last().cloned();
        if selected_path != last_selected {
            detail_scroll = 0;
            last_selected = selected_path.clone();
        }

        let (detail_title, detail) = build_detail(
            tree,
            snapshot,
            highlighter,
            &mut diff_cache,
            selected_path.as_deref(),
        );
        let marked = tree.selected_count();
        let header = header_line(branch, base, marked, total);
        let footer = if confirming {
            Line::styled(
                format!(
                    "Revert {marked} marked file(s) to '{base}'? Working copy only — \
                     no commit.  [y/N]"
                ),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Line::styled(FOOTER, Style::default().fg(Color::DarkGray))
        };

        tui.draw(|frame| {
            let area = frame.area();
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

            frame.render_widget(
                Paragraph::new(header.clone()).style(Style::default().add_modifier(Modifier::BOLD)),
                rows[0],
            );

            let cols = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(rows[1]);
            detail_view_height = cols[1].height.saturating_sub(2);

            let widget = Tree::new(&items)
                .expect("node paths are unique")
                .block(Block::default().borders(Borders::ALL).title(" Changes "))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
                .highlight_symbol("» ");
            frame.render_stateful_widget(widget, cols[0], &mut state);

            frame.render_widget(
                Paragraph::new(detail.clone())
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(detail_title.clone()),
                    )
                    .scroll((detail_scroll, 0)),
                cols[1],
            );

            frame.render_widget(Paragraph::new(footer.clone()), rows[2]);
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if confirming {
            // Only an explicit `y` reverts; anything else dismisses the question.
            if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                return Ok(ReviewOutcome::RevertMarked {
                    paths: tree.selected_paths(&snapshot.changes),
                });
            }
            confirming = false;
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => return Ok(ReviewOutcome::Back),
            KeyCode::Esc | KeyCode::Char('q') => return Ok(ReviewOutcome::Back),
            KeyCode::Char('r' | 'R') => {
                if tree.selected_count() > 0 {
                    confirming = true;
                }
            }
            KeyCode::Up => {
                state.key_up();
            }
            KeyCode::Down => {
                state.key_down();
            }
            KeyCode::Left => {
                state.key_left();
            }
            KeyCode::Right => {
                state.key_right();
            }
            KeyCode::Char(' ') => {
                if let Some(id) = state.selected().last() {
                    tree.toggle(&id.clone());
                }
            }
            KeyCode::Char('+') => tree.set_all(true),
            KeyCode::Char('-') => tree.set_all(false),
            KeyCode::PageDown => {
                let hidden = detail
                    .lines
                    .len()
                    .saturating_sub(detail_view_height as usize);
                let max = u16::try_from(hidden).unwrap_or(u16::MAX);
                detail_scroll = detail_scroll.saturating_add(10).min(max);
            }
            KeyCode::PageUp => detail_scroll = detail_scroll.saturating_sub(10),
            KeyCode::Home => detail_scroll = 0,
            _ => {}
        }
    }
}

const FOOTER: &str =
    "↑↓ move  ←→ fold  Space mark  +/- all/none  PgUp/PgDn/Home scroll  r revert marked  Esc back";

/// Header naming the compared branches plus the file and mark counts.
fn header_line(branch: &str, base: &str, marked: usize, total: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(branch.to_string(), Style::default().fg(Color::Cyan)),
        Span::styled(" vs ", Style::default().fg(Color::DarkGray)),
        Span::styled(base.to_string(), Style::default().fg(Color::Cyan)),
        Span::styled(
            format!("    {total} file(s)    marked {marked}"),
            Style::default().fg(Color::DarkGray),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::model::{ChangeKind, FileChange};

    fn buffer_text(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    /// Renders the header + cleared-marks tree off-screen: the review screen's
    /// specific bits (vs-header, A/M glyphs, unmarked checkboxes) show up.
    #[test]
    fn renders_vs_header_and_unmarked_tree() {
        let changes = vec![
            FileChange {
                path: "src/lib.rs".into(),
                old_path: None,
                kind: ChangeKind::Modified,
            },
            FileChange {
                path: "docs/new.md".into(),
                old_path: None,
                kind: ChangeKind::Added,
            },
        ];
        let mut tree = TreeModel::build(&changes);
        tree.set_all(false); // review starts with marks cleared

        let items = items_of(&tree, &tree.roots);
        let mut state: TreeState<String> = TreeState::default();
        for chain in open_chains(&tree.roots) {
            state.open(chain);
        }
        let header = header_line("feat/x", "main", tree.selected_count(), changes.len());

        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|frame| {
            let rows =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(frame.area());
            frame.render_widget(Paragraph::new(header.clone()), rows[0]);
            let widget = Tree::new(&items).unwrap();
            frame.render_stateful_widget(widget, rows[1], &mut state);
        })
        .unwrap();

        let text = buffer_text(&term);
        assert!(text.contains("feat/x vs main"), "header missing:\n{text}");
        assert!(text.contains("marked 0"), "mark count missing:\n{text}");
        assert!(text.contains("[ ] M lib.rs"), "file row missing:\n{text}");
        assert!(text.contains("[ ] A new.md"), "added row missing:\n{text}");
    }
}
