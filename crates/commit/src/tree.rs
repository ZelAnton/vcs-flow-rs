//! The file picker's data model: a path-compressed tree over the changed files
//! plus tri-state selection.
//!
//! Compression rule: a directory node whose only child is itself a directory is
//! merged with that child (`aa` + `bb` → `aa/bb`). Files never merge. So a set of
//! files all under `aa/bb/cc` collapses to a single `aa/bb/cc` node, while files
//! split between `aa/bb` and `aa/bb/cc` yield an `aa/bb` node containing a `cc`
//! node plus the loose files.

use std::collections::BTreeMap;

use crate::model::{ChangeKind, FileChange};

/// Selection status of a node, derived from its descendant files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectState {
    All,
    None,
    Partial,
}

/// Whether a tree node is a directory or a concrete changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File { index: usize, kind: ChangeKind },
}

/// One node of the compressed tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Display label: the (possibly compressed) directory segment, or file name.
    pub label: String,
    /// Repo-relative path of this node (forward slashes). Unique — used as the
    /// tree-widget identifier and for lookups.
    pub path: String,
    pub kind: NodeKind,
    pub children: Vec<Node>,
    /// Indices (into the original change list) of every file under this node.
    /// For a file node this is just its own index.
    pub files: Vec<usize>,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, NodeKind::Dir)
    }

    /// Tree-widget identifier — must be unique even when a tracked file and a
    /// directory share a path (a file replaced by a same-named directory, with
    /// the new entry staged). Directories get a trailing slash; files don't.
    pub fn id(&self) -> String {
        if self.is_dir() {
            format!("{}/", self.path)
        } else {
            self.path.clone()
        }
    }
}

/// The full picker model: the compressed forest plus per-file selection.
#[derive(Debug, Clone)]
pub struct TreeModel {
    pub roots: Vec<Node>,
    /// Selected flag per file, indexed by the original change index.
    selected: Vec<bool>,
}

impl TreeModel {
    /// Build the compressed tree from the change list. All files start selected.
    pub fn build(changes: &[FileChange]) -> Self {
        let mut model = TreeModel {
            roots: Vec::new(),
            selected: vec![true; changes.len()],
        };
        let all: Vec<usize> = (0..changes.len()).collect();
        model.rebuild_view(changes, &all);
        model
    }

    /// Rebuild the visible forest from the subset of `changes` whose original
    /// indices are in `keep`, leaving the selection state untouched. File nodes
    /// keep their *original* change indices, so marks survive any narrowing —
    /// this is what the select screen's filter uses (pass every index to
    /// restore the full view).
    pub fn rebuild_view(&mut self, changes: &[FileChange], keep: &[usize]) {
        let mut indexed: Vec<(Vec<&str>, usize)> = keep
            .iter()
            .map(|&i| {
                (
                    changes[i]
                        .path
                        .split('/')
                        .filter(|s| !s.is_empty())
                        .collect(),
                    i,
                )
            })
            .collect();
        indexed.sort_by(|a, b| a.0.cmp(&b.0));

        let mut roots = build_nodes("", indexed, changes);
        for node in &mut roots {
            compress(node);
        }
        self.roots = roots;
    }

    /// Selection status of a node from its descendant files.
    pub fn state_of(&self, node: &Node) -> SelectState {
        let total = node.files.len();
        if total == 0 {
            return SelectState::None;
        }
        let sel = node.files.iter().filter(|&&i| self.selected[i]).count();
        if sel == 0 {
            SelectState::None
        } else if sel == total {
            SelectState::All
        } else {
            SelectState::Partial
        }
    }

    /// Toggle the node with identifier `id` (see [`Node::id`]): a fully-selected
    /// node clears; anything else (partial or empty) selects all its files.
    pub fn toggle(&mut self, id: &str) {
        let Some(files) = self.find(id).map(|n| n.files.clone()) else {
            return;
        };
        let fully = files.iter().all(|&i| self.selected[i]);
        for i in files {
            self.selected[i] = !fully;
        }
    }

    /// Select (`true`) or clear (`false`) every file.
    pub fn set_all(&mut self, value: bool) {
        for s in &mut self.selected {
            *s = value;
        }
    }

    /// Select (`true`) or clear (`false`) only the files in the current view —
    /// with a filter active, `+`/`-` shouldn't reach what the user can't see.
    /// Identical to [`set_all`](Self::set_all) when the view is unfiltered.
    pub fn set_view(&mut self, value: bool) {
        let mut files = Vec::new();
        collect_files(&self.roots, &mut files);
        for i in files {
            self.selected[i] = value;
        }
    }

    /// Find a node by its identifier (see [`Node::id`]).
    pub fn find(&self, id: &str) -> Option<&Node> {
        fn walk<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Node> {
            for n in nodes {
                let nid = n.id();
                if nid == id {
                    return Some(n);
                }
                // A dir id ends with `/`, so `id` under it starts with `nid`.
                if n.is_dir()
                    && id.starts_with(&nid)
                    && let Some(found) = walk(&n.children, id)
                {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.roots, id)
    }

    /// Paths to hand to the commit command for the selected files, repo-relative,
    /// forward slashes. A renamed file contributes both its new and old path so
    /// the deletion of the old path is committed alongside the addition.
    pub fn selected_paths(&self, changes: &[FileChange]) -> Vec<String> {
        let mut out = Vec::new();
        for (i, c) in changes.iter().enumerate() {
            if self.selected[i] {
                out.push(c.path.clone());
                if let Some(old) = &c.old_path {
                    out.push(old.clone());
                }
            }
        }
        out
    }

    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|&&s| s).count()
    }
}

/// Recursively group `(components, file_index)` entries into raw nodes under
/// `prefix`. Directories (sorted) come before loose files (sorted).
fn build_nodes(
    prefix: &str,
    entries: Vec<(Vec<&str>, usize)>,
    changes: &[FileChange],
) -> Vec<Node> {
    let mut files_here: Vec<(String, usize)> = Vec::new();
    let mut dirs: BTreeMap<String, Vec<(Vec<&str>, usize)>> = BTreeMap::new();
    for (comps, idx) in entries {
        if comps.len() <= 1 {
            let name = comps.first().copied().unwrap_or_default().to_string();
            files_here.push((name, idx));
        } else {
            let head = comps[0].to_string();
            dirs.entry(head)
                .or_default()
                .push((comps[1..].to_vec(), idx));
        }
    }

    let join = |name: &str| -> String {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        }
    };

    let mut nodes = Vec::new();
    for (name, sub) in dirs {
        let path = join(&name);
        let children = build_nodes(&path, sub, changes);
        let mut files = Vec::new();
        collect_files(&children, &mut files);
        nodes.push(Node {
            label: name,
            path,
            kind: NodeKind::Dir,
            children,
            files,
        });
    }
    files_here.sort();
    for (name, idx) in files_here {
        nodes.push(Node {
            label: name.clone(),
            path: join(&name),
            kind: NodeKind::File {
                index: idx,
                kind: changes[idx].kind,
            },
            children: Vec::new(),
            files: vec![idx],
        });
    }
    nodes
}

fn collect_files(nodes: &[Node], out: &mut Vec<usize>) {
    for n in nodes {
        match n.kind {
            NodeKind::File { index, .. } => out.push(index),
            NodeKind::Dir => collect_files(&n.children, out),
        }
    }
}

/// Collapse single-directory-child chains. Depth-first so inner chains compress
/// before their parents absorb them.
fn compress(node: &mut Node) {
    for child in &mut node.children {
        compress(child);
    }
    while node.is_dir() && node.children.len() == 1 && node.children[0].is_dir() {
        let child = node.children.remove(0);
        node.label = format!("{}/{}", node.label, child.label);
        node.path = child.path;
        node.children = child.children;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(paths: &[&str]) -> Vec<FileChange> {
        paths
            .iter()
            .map(|p| FileChange {
                path: (*p).to_string(),
                old_path: None,
                kind: ChangeKind::Modified,
            })
            .collect()
    }

    #[test]
    fn collapses_full_single_chain() {
        let c = changes(&["aa/bb/cc/d1.txt", "aa/bb/cc/d2.txt"]);
        let t = TreeModel::build(&c);
        assert_eq!(t.roots.len(), 1);
        assert_eq!(t.roots[0].label, "aa/bb/cc");
        assert!(t.roots[0].is_dir());
        assert_eq!(t.roots[0].children.len(), 2);
        assert!(t.roots[0].children.iter().all(|n| !n.is_dir()));
    }

    #[test]
    fn branches_when_files_split_across_levels() {
        let c = changes(&["aa/bb/x.txt", "aa/bb/cc/y.txt"]);
        let t = TreeModel::build(&c);
        assert_eq!(t.roots.len(), 1);
        let bb = &t.roots[0];
        assert_eq!(bb.label, "aa/bb");
        // children: a `cc` dir plus the loose `x.txt` file.
        let labels: Vec<&str> = bb.children.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, vec!["cc", "x.txt"]);
        let cc = &bb.children[0];
        assert_eq!(cc.path, "aa/bb/cc");
        assert_eq!(cc.children.len(), 1);
        assert_eq!(cc.children[0].label, "y.txt");
    }

    #[test]
    fn single_file_dir_collapses_to_its_folder() {
        let c = changes(&["a/b/c.txt"]);
        let t = TreeModel::build(&c);
        assert_eq!(t.roots[0].label, "a/b");
        assert_eq!(t.roots[0].children[0].label, "c.txt");
    }

    #[test]
    fn tristate_reflects_selection() {
        let c = changes(&["aa/bb/x.txt", "aa/bb/cc/y.txt"]);
        let mut t = TreeModel::build(&c);
        let root_id = t.roots[0].id();
        // all selected by default
        assert_eq!(t.state_of(&t.roots[0].clone()), SelectState::All);
        // clear one leaf -> root becomes partial
        t.toggle("aa/bb/x.txt");
        assert_eq!(t.state_of(&t.roots[0].clone()), SelectState::Partial);
        assert_eq!(t.selected_count(), 1);
        // toggling the partial root selects everything
        t.toggle(&root_id);
        assert_eq!(t.state_of(&t.roots[0].clone()), SelectState::All);
        // toggling a full node clears it
        t.toggle(&root_id);
        assert_eq!(t.state_of(&t.roots[0].clone()), SelectState::None);
        assert_eq!(t.selected_count(), 0);
    }

    #[test]
    fn set_all_and_selected_paths() {
        let c = changes(&["src/a.rs", "src/b.rs", "top.txt"]);
        let mut t = TreeModel::build(&c);
        t.set_all(false);
        assert!(t.selected_paths(&c).is_empty());
        t.toggle("src/b.rs");
        assert_eq!(t.selected_paths(&c), vec!["src/b.rs".to_string()]);
    }

    #[test]
    fn rename_commits_both_old_and_new_paths() {
        let c = vec![FileChange {
            path: "new.rs".into(),
            old_path: Some("old.rs".into()),
            kind: ChangeKind::Renamed,
        }];
        let t = TreeModel::build(&c);
        let mut paths = t.selected_paths(&c);
        paths.sort();
        assert_eq!(paths, vec!["new.rs".to_string(), "old.rs".to_string()]);
    }

    #[test]
    fn rebuild_view_keeps_original_indices_and_marks() {
        let c = changes(&["src/a.rs", "src/b.rs", "docs/x.md"]);
        let mut t = TreeModel::build(&c);
        // Clear b.rs, then narrow the view to docs/ only.
        t.toggle("src/b.rs");
        t.rebuild_view(&c, &[2]);
        // The view shows only docs/x.md, carrying its ORIGINAL index (2).
        assert_eq!(t.roots.len(), 1);
        let file = &t.roots[0].children[0];
        assert!(matches!(file.kind, NodeKind::File { index: 2, .. }));
        // Marks of hidden files survive: a.rs still on, b.rs still off.
        t.rebuild_view(&c, &[0, 1, 2]);
        let paths = t.selected_paths(&c);
        assert_eq!(paths, vec!["src/a.rs".to_string(), "docs/x.md".to_string()]);
    }

    #[test]
    fn set_view_only_touches_visible_files() {
        let c = changes(&["src/a.rs", "src/b.rs", "docs/x.md"]);
        let mut t = TreeModel::build(&c);
        t.rebuild_view(&c, &[0, 1]); // filter to src/
        t.set_view(false); // "- none" under the filter
        t.rebuild_view(&c, &[0, 1, 2]); // back to full view
        // docs/x.md was invisible and keeps its mark.
        assert_eq!(t.selected_paths(&c), vec!["docs/x.md".to_string()]);
    }

    #[test]
    fn file_and_dir_sharing_a_path_have_distinct_ids() {
        // A tracked file `aa` deleted while `aa/bb.txt` is added — file node and
        // dir node both at path "aa". Identifiers must stay unique (else the tree
        // widget panics on duplicate ids).
        let c = vec![
            FileChange {
                path: "aa".into(),
                old_path: None,
                kind: ChangeKind::Deleted,
            },
            FileChange {
                path: "aa/bb.txt".into(),
                old_path: None,
                kind: ChangeKind::Added,
            },
        ];
        let t = TreeModel::build(&c);

        fn collect(nodes: &[Node], ids: &mut Vec<String>) {
            for n in nodes {
                ids.push(n.id());
                collect(&n.children, ids);
            }
        }
        let mut ids = Vec::new();
        collect(&t.roots, &mut ids);
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "duplicate identifiers: {ids:?}");

        // Both the file and the directory are independently resolvable by id.
        assert!(matches!(t.find("aa").unwrap().kind, NodeKind::File { .. }));
        assert!(t.find("aa/").unwrap().is_dir());
    }
}
