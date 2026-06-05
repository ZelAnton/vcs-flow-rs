//! The file-selection screen: a path-compressed tree with tri-state checkboxes
//! on the left, and the selected file's highlighted diff (or a folder's
//! children) on the right.

use std::collections::HashMap;
use std::io;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use tui_tree_widget::{Tree, TreeItem, TreeState};

use crate::model::{BackendKind, ChangeKind, Target};
use crate::tree::{Node, NodeKind, SelectState, TreeModel};
use crate::ui::diff::Highlighter;
use crate::ui::is_confirm;
use crate::ui::terminal::Tui;
use crate::vcs::Snapshot;

/// Outcome of the select screen. The chosen files live in the (mutated) tree.
pub struct SelectResult {
    pub confirmed: bool,
    pub amend: bool,
}

/// Run the screen until the user confirms (`Ctrl+Enter`/`Ctrl+S`) or cancels
/// (`Esc`/`q`/`Ctrl+C`).
pub fn run(
    tui: &mut Tui,
    snapshot: &Snapshot,
    tree: &mut TreeModel,
    target: &Target,
    kind: BackendKind,
    initial_amend: bool,
    highlighter: &Highlighter,
) -> io::Result<SelectResult> {
    let mut state: TreeState<String> = TreeState::default();
    // Start fully expanded (the spec wants an expanded tree) with the first node
    // selected.
    for chain in open_chains(&tree.roots) {
        state.open(chain);
    }
    if let Some(first) = tree.roots.first() {
        state.select(vec![first.id()]);
    }

    let total = snapshot.changes.len();
    let mut amend = initial_amend;
    let mut diff_cache: HashMap<String, Text<'static>> = HashMap::new();
    let mut detail_scroll: u16 = 0;
    // Visible line count of the diff pane (inner height), captured each draw so
    // PageDown can clamp to "last line at the bottom" instead of overscrolling.
    let mut detail_view_height: u16 = 0;
    let mut last_selected: Option<String> = None;
    // `/` filter over the tree: while `filtering`, typed characters edit the
    // needle and the view narrows live; marks of hidden files are untouched.
    let mut filter = String::new();
    let mut filtering = false;

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
        let header = header_line(kind, target, amend, tree.selected_count(), total);

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
            // Inner height of the bordered diff pane (the visible diff-line count).
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

            let footer: Paragraph = if filtering {
                Paragraph::new(format!("filter: {filter}_   Enter keep   Esc clear"))
                    .style(Style::default().fg(Color::Yellow))
            } else if !filter.is_empty() {
                Paragraph::new(format!("{FOOTER}  [filter: {filter}]"))
                    .style(Style::default().fg(Color::DarkGray))
            } else {
                Paragraph::new(FOOTER).style(Style::default().fg(Color::DarkGray))
            };
            frame.render_widget(footer, rows[2]);
        })?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if is_confirm(&key) {
            if tree.selected_count() > 0 {
                return Ok(SelectResult {
                    confirmed: true,
                    amend,
                });
            }
            continue; // refuse an empty commit
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if filtering {
            // Modal filter input: printable keys edit the needle (so `q`/`a`/
            // Space lose their command meaning until Enter); navigation and the
            // confirm chord stay live.
            match key.code {
                KeyCode::Char('c') if ctrl => {
                    return Ok(SelectResult {
                        confirmed: false,
                        amend,
                    });
                }
                KeyCode::Esc => {
                    filtering = false;
                    filter.clear();
                    apply_filter(tree, snapshot, &filter, &mut state);
                }
                KeyCode::Enter => filtering = false,
                KeyCode::Backspace => {
                    filter.pop();
                    apply_filter(tree, snapshot, &filter, &mut state);
                }
                KeyCode::Char(c) if !ctrl => {
                    filter.push(c);
                    apply_filter(tree, snapshot, &filter, &mut state);
                }
                KeyCode::Up => {
                    state.key_up();
                }
                KeyCode::Down => {
                    state.key_down();
                }
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('c') if ctrl => {
                return Ok(SelectResult {
                    confirmed: false,
                    amend,
                });
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                // With a filter active, cancel first widens back to the full
                // tree (same for Esc and q — no surprise aborts); a second
                // press cancels the screen.
                if !filter.is_empty() {
                    filter.clear();
                    apply_filter(tree, snapshot, &filter, &mut state);
                    continue;
                }
                return Ok(SelectResult {
                    confirmed: false,
                    amend,
                });
            }
            KeyCode::Char('/') => filtering = true,
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
            KeyCode::Char('+') => tree.set_view(true),
            KeyCode::Char('-') => tree.set_view(false),
            KeyCode::Char('a') | KeyCode::Char('A') => amend = !amend,
            KeyCode::PageDown => {
                // Clamp so the last line lands at the bottom of the pane rather than
                // scrolling into a blank gap (and don't truncate huge diffs to u16).
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

const FOOTER: &str = "↑↓ move  ←→ fold  Space toggle  +/- all/none  / filter  a amend  PgUp/PgDn scroll  Ctrl+Enter/Ctrl+S commit  Esc cancel";

/// Narrow the visible tree to the files whose path matches `filter`
/// (case-insensitive substring; empty matches all) and reset the cursor/fold
/// state for the rebuilt forest. Marks are per original index and survive.
fn apply_filter(
    tree: &mut TreeModel,
    snapshot: &Snapshot,
    filter: &str,
    state: &mut TreeState<String>,
) {
    let keep: Vec<usize> = snapshot
        .changes
        .iter()
        .enumerate()
        .filter(|(_, c)| crate::ui::filter::matches(&c.path, filter))
        .map(|(i, _)| i)
        .collect();
    tree.rebuild_view(&snapshot.changes, &keep);
    for chain in open_chains(&tree.roots) {
        state.open(chain);
    }
    if let Some(first) = tree.roots.first() {
        state.select(vec![first.id()]);
    } else {
        state.select(Vec::new());
    }
}

/// Header showing where the commit will land plus the amend flag and counts.
fn header_line(
    kind: BackendKind,
    target: &Target,
    amend: bool,
    selected: usize,
    total: usize,
) -> Line<'static> {
    let where_to = if target.label.is_empty() {
        "(no bookmark — describe only)".to_string()
    } else {
        match kind {
            BackendKind::Git => format!("branch {}", target.label),
            BackendKind::Jj => match &target.revision {
                Some(rev) => format!("bookmark {} ({rev})", target.label),
                None => format!("bookmark {}", target.label),
            },
        }
    };
    let mut spans = vec![
        Span::styled("Target: ", Style::default().fg(Color::DarkGray)),
        Span::styled(where_to, Style::default().fg(Color::Cyan)),
    ];
    if amend {
        spans.push(Span::styled(
            "  [AMEND]",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(
        format!("    selected {selected}/{total}"),
        Style::default().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

/// Build tree-widget items from the model, styling checkboxes and file glyphs.
/// (`pub(crate)`: the PR diff-review screen renders the same tree rows.)
pub(crate) fn items_of(tree: &TreeModel, nodes: &[Node]) -> Vec<TreeItem<'static, String>> {
    nodes
        .iter()
        .map(|node| {
            let line = node_line(tree, node);
            if node.children.is_empty() {
                TreeItem::new_leaf(node.id(), line)
            } else {
                // Dirs — and files with hunk children (collapsed by default;
                // `→` folds them open for hunk-level marking).
                TreeItem::new(node.id(), line, items_of(tree, &node.children))
                    .expect("node identifiers are unique")
            }
        })
        .collect()
}

/// A single tree row: `[x] dir/` or `[x] M file.rs`, coloured by state/kind.
fn node_line(tree: &TreeModel, node: &Node) -> Line<'static> {
    let (mark, mark_color) = match tree.state_of(node) {
        SelectState::All => ("[x]", Color::Green),
        SelectState::None => ("[ ]", Color::DarkGray),
        SelectState::Partial => ("[~]", Color::Yellow),
    };
    let mut spans = vec![
        Span::styled(mark.to_string(), Style::default().fg(mark_color)),
        Span::raw(" "),
    ];
    match &node.kind {
        NodeKind::Dir => spans.push(Span::styled(
            format!("{}/", node.label),
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )),
        NodeKind::File { kind, .. } => {
            let (glyph, color) = glyph_style(*kind);
            spans.push(Span::styled(
                format!("{glyph} "),
                Style::default().fg(color),
            ));
            spans.push(Span::raw(node.label.clone()));
        }
        NodeKind::Hunk { .. } => {
            // The label is the `@@ -a,b +c,d @@ section` header.
            spans.push(Span::styled(
                node.label.clone(),
                Style::default().fg(Color::Cyan),
            ));
        }
    }
    Line::from(spans)
}

fn glyph_style(kind: ChangeKind) -> (char, Color) {
    match kind {
        ChangeKind::Added => ('A', Color::Green),
        ChangeKind::Modified => ('M', Color::Yellow),
        ChangeKind::Deleted => ('D', Color::Red),
        ChangeKind::Renamed => ('R', Color::Cyan),
        ChangeKind::Untracked => ('?', Color::Magenta),
    }
}

/// Right-pane content: a file's highlighted diff, or a folder's child listing.
pub(crate) fn build_detail(
    tree: &TreeModel,
    snapshot: &Snapshot,
    highlighter: &Highlighter,
    cache: &mut HashMap<String, Text<'static>>,
    id: Option<&str>,
) -> (String, Text<'static>) {
    let Some(id) = id else {
        return (" detail ".to_string(), Text::raw(""));
    };
    let Some(node) = tree.find(id) else {
        return (" detail ".to_string(), Text::raw(""));
    };
    match &node.kind {
        NodeKind::File { .. } => {
            // The real repo path (the diff key) — for files this equals the id.
            let path = node.path.as_str();
            if !cache.contains_key(path) {
                let diff = snapshot.diffs.get(path).map(String::as_str).unwrap_or("");
                cache.insert(path.to_string(), highlighter.render(path, diff));
            }
            (
                format!(" {path} "),
                cache.get(path).cloned().unwrap_or_default(),
            )
        }
        NodeKind::Dir => {
            let lines: Vec<Line<'static>> =
                node.children.iter().map(|c| node_line(tree, c)).collect();
            (format!(" {}/ ", node.path), Text::from(lines))
        }
        NodeKind::Hunk { hunk_index, .. } => {
            let path = node.path.as_str();
            let total = snapshot.hunks.get(path).map_or(0, Vec::len);
            let key = node.id(); // "path#k" — distinct from the file's cache entry
            if !cache.contains_key(&key) {
                let text = snapshot
                    .hunks
                    .get(path)
                    .and_then(|hs| hs.get(*hunk_index))
                    .map(|h| h.text.as_str())
                    .unwrap_or("");
                cache.insert(key.clone(), highlighter.render(path, text));
            }
            (
                format!(" {path} — hunk {}/{total} ", hunk_index + 1),
                cache.get(&key).cloned().unwrap_or_default(),
            )
        }
    }
}

/// All directory identifier-chains, used to expand the whole tree at startup.
pub(crate) fn open_chains(nodes: &[Node]) -> Vec<Vec<String>> {
    fn walk(nodes: &[Node], prefix: &[String], out: &mut Vec<Vec<String>>) {
        for node in nodes {
            let mut chain = prefix.to_vec();
            chain.push(node.id());
            if node.is_dir() {
                out.push(chain.clone());
                walk(&node.children, &chain, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(nodes, &[], &mut out);
    out
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

    /// Renders the expanded tree into an off-screen terminal: exercises the real
    /// widget pipeline (catches panics, width issues) and asserts content shows.
    #[test]
    fn renders_compressed_tree_with_checkboxes() {
        let changes = vec![
            FileChange {
                path: "aa/bb/cc/deep.txt".into(),
                old_path: None,
                kind: ChangeKind::Added,
            },
            FileChange {
                path: "src/main.rs".into(),
                old_path: None,
                kind: ChangeKind::Modified,
            },
        ];
        let tree = TreeModel::build(&changes);
        let items = items_of(&tree, &tree.roots);
        let mut state: TreeState<String> = TreeState::default();
        for chain in open_chains(&tree.roots) {
            state.open(chain);
        }
        state.select(vec![tree.roots[0].id()]);

        let mut term = Terminal::new(TestBackend::new(48, 12)).unwrap();
        term.draw(|frame| {
            let widget = Tree::new(&items).unwrap();
            frame.render_stateful_widget(widget, frame.area(), &mut state);
        })
        .unwrap();

        let text = buffer_text(&term);
        assert!(text.contains("aa/bb/cc"), "compressed dir missing:\n{text}");
        assert!(text.contains("deep.txt"), "file missing:\n{text}");
        assert!(text.contains("[x]"), "checkbox missing:\n{text}");
    }
}
