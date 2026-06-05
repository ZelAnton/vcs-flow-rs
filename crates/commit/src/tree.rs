//! The file picker's data model: a path-compressed tree over the changed files
//! plus tri-state selection.
//!
//! Compression rule: a directory node whose only child is itself a directory is
//! merged with that child (`aa` + `bb` → `aa/bb`). Files never merge. So a set of
//! files all under `aa/bb/cc` collapses to a single `aa/bb/cc` node, while files
//! split between `aa/bb` and `aa/bb/cc` yield an `aa/bb` node containing a `cc`
//! node plus the loose files.

use std::collections::{BTreeMap, HashMap};

use crate::model::{ChangeKind, FileChange, HunkInfo};

/// Selection status of a node, derived from its descendant files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectState {
    All,
    None,
    Partial,
}

/// Whether a tree node is a directory, a concrete changed file, or one hunk
/// of a modified file (git only — a leaf child of its file node).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File {
        index: usize,
        kind: ChangeKind,
    },
    Hunk {
        file_index: usize,
        hunk_index: usize,
    },
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
    /// the new entry staged). Directories get a trailing slash; files don't;
    /// hunks append `#<index>` to their file's id (`#` can't collide with the
    /// other shapes — a path named `x#1` yields a *file* id `x#1`, but its
    /// hypothetical hunks would be `x#1#0`, still unique).
    pub fn id(&self) -> String {
        match &self.kind {
            NodeKind::Dir => format!("{}/", self.path),
            NodeKind::File { .. } => self.path.clone(),
            NodeKind::Hunk { hunk_index, .. } => format!("{}#{hunk_index}", self.path),
        }
    }
}

/// Selection state of one file: whole-file by default, or per-hunk once the
/// file offers hunk granularity (then `whole` is derived: all hunks on).
#[derive(Debug, Clone)]
struct FileSel {
    whole: bool,
    hunks: Option<Vec<bool>>,
}

impl FileSel {
    /// `(selected, total)` selection units: hunks when split, else the file (1).
    fn units(&self) -> (usize, usize) {
        match &self.hunks {
            Some(h) => (h.iter().filter(|&&b| b).count(), h.len()),
            None => (usize::from(self.whole), 1),
        }
    }

    fn any(&self) -> bool {
        self.units().0 > 0
    }

    fn all(&self) -> bool {
        let (sel, total) = self.units();
        sel == total
    }

    fn set(&mut self, value: bool) {
        self.whole = value;
        if let Some(h) = &mut self.hunks {
            for b in h {
                *b = value;
            }
        }
    }
}

/// The full picker model: the compressed forest plus per-file (and, for
/// hunk-split files, per-hunk) selection.
#[derive(Debug, Clone)]
pub struct TreeModel {
    pub roots: Vec<Node>,
    /// Selection per file, indexed by the original change index.
    selected: Vec<FileSel>,
    /// Hunk row labels per change index (empty → the file has no hunk children).
    hunk_labels: Vec<Vec<String>>,
}

impl TreeModel {
    /// Build the compressed tree from the change list. All files start selected.
    pub fn build(changes: &[FileChange]) -> Self {
        let mut model = TreeModel {
            roots: Vec::new(),
            selected: vec![
                FileSel {
                    whole: true,
                    hunks: None,
                };
                changes.len()
            ],
            hunk_labels: vec![Vec::new(); changes.len()],
        };
        let all: Vec<usize> = (0..changes.len()).collect();
        model.rebuild_view(changes, &all);
        model
    }

    /// Enable hunk-level selection (git only): files present in `hunks` with at
    /// least two hunks gain hunk children (a single hunk is the whole change —
    /// nothing to subdivide). Everything starts selected, like files do.
    pub fn with_hunks(&mut self, changes: &[FileChange], hunks: &HashMap<String, Vec<HunkInfo>>) {
        for (i, c) in changes.iter().enumerate() {
            if let Some(hs) = hunks.get(&c.path)
                && hs.len() >= 2
            {
                self.hunk_labels[i] = hs.iter().map(|h| h.header.clone()).collect();
                self.selected[i].hunks = Some(vec![true; hs.len()]);
            }
        }
        let all: Vec<usize> = (0..changes.len()).collect();
        self.rebuild_view(changes, &all);
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
        attach_hunks(&mut roots, &self.hunk_labels);
        self.roots = roots;
    }

    /// Selection status of a node from its descendant selection units (hunks
    /// for hunk-split files, the file itself otherwise).
    pub fn state_of(&self, node: &Node) -> SelectState {
        if let NodeKind::Hunk {
            file_index,
            hunk_index,
        } = &node.kind
        {
            let on = self.selected[*file_index]
                .hunks
                .as_ref()
                .is_some_and(|h| h.get(*hunk_index).copied().unwrap_or(false));
            return if on {
                SelectState::All
            } else {
                SelectState::None
            };
        }
        let (mut sel, mut total) = (0usize, 0usize);
        for &i in &node.files {
            let (s, t) = self.selected[i].units();
            sel += s;
            total += t;
        }
        if total == 0 || sel == 0 {
            SelectState::None
        } else if sel == total {
            SelectState::All
        } else {
            SelectState::Partial
        }
    }

    /// Toggle the node with identifier `id` (see [`Node::id`]). A hunk flips
    /// alone; for a file or directory, a fully-selected node clears and
    /// anything else (partial or empty) selects everything under it.
    pub fn toggle(&mut self, id: &str) {
        let Some(node) = self.find(id) else {
            return;
        };
        let kind = node.kind.clone();
        let files = node.files.clone();
        if let NodeKind::Hunk {
            file_index,
            hunk_index,
        } = kind
        {
            if let Some(h) = &mut self.selected[file_index].hunks
                && let Some(b) = h.get_mut(hunk_index)
            {
                *b = !*b;
            }
            return;
        }
        let fully = files.iter().all(|&i| self.selected[i].all());
        for i in files {
            self.selected[i].set(!fully);
        }
    }

    /// Select (`true`) or clear (`false`) every file.
    pub fn set_all(&mut self, value: bool) {
        for s in &mut self.selected {
            s.set(value);
        }
    }

    /// Select (`true`) or clear (`false`) only the files in the current view —
    /// with a filter active, `+`/`-` shouldn't reach what the user can't see.
    /// Identical to [`set_all`](Self::set_all) when the view is unfiltered.
    pub fn set_view(&mut self, value: bool) {
        let mut files = Vec::new();
        collect_files(&self.roots, &mut files);
        for i in files {
            self.selected[i].set(value);
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
                // A dir id ends with `/`, so `id` under it starts with `nid`;
                // a hunk id is its file's id plus `#<k>`.
                let descend = match &n.kind {
                    NodeKind::Dir => id.starts_with(&nid),
                    NodeKind::File { .. } => {
                        !n.children.is_empty()
                            && id.starts_with(&nid)
                            && id[nid.len()..].starts_with('#')
                    }
                    NodeKind::Hunk { .. } => false,
                };
                if descend && let Some(found) = walk(&n.children, id) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.roots, id)
    }

    /// Paths of every file participating in the commit (whole or via a hunk
    /// subset), repo-relative, forward slashes. A renamed file contributes both
    /// its new and old path so the deletion of the old path is committed
    /// alongside the addition.
    pub fn selected_paths(&self, changes: &[FileChange]) -> Vec<String> {
        let mut out = Vec::new();
        for (i, c) in changes.iter().enumerate() {
            if self.selected[i].any() {
                out.push(c.path.clone());
                if let Some(old) = &c.old_path {
                    out.push(old.clone());
                }
            }
        }
        out
    }

    /// Paths committed *whole*: fully-selected files (a hunk-split file counts
    /// once every hunk is on). New + old (rename) paths, like
    /// [`selected_paths`](Self::selected_paths).
    pub fn selected_whole_paths(&self, changes: &[FileChange]) -> Vec<String> {
        let mut out = Vec::new();
        for (i, c) in changes.iter().enumerate() {
            let s = &self.selected[i];
            if s.any() && s.all() {
                out.push(c.path.clone());
                if let Some(old) = &c.old_path {
                    out.push(old.clone());
                }
            }
        }
        out
    }

    /// Files with only a *subset* of hunks selected: `(path, hunk indices)`.
    pub fn selected_partial(&self, changes: &[FileChange]) -> Vec<(String, Vec<usize>)> {
        let mut out = Vec::new();
        for (i, c) in changes.iter().enumerate() {
            let Some(h) = &self.selected[i].hunks else {
                continue;
            };
            let on: Vec<usize> = h
                .iter()
                .enumerate()
                .filter(|&(_, &b)| b)
                .map(|(k, _)| k)
                .collect();
            if !on.is_empty() && on.len() < h.len() {
                out.push((c.path.clone(), on));
            }
        }
        out
    }

    /// Count of files with at least one selection unit on.
    pub fn selected_count(&self) -> usize {
        self.selected.iter().filter(|s| s.any()).count()
    }
}

/// Give every hunk-split file its hunk children (leaves labelled by the `@@`
/// headers). Runs after compression on every view rebuild, so a filtered view
/// keeps the same hunk rows.
fn attach_hunks(nodes: &mut [Node], labels: &[Vec<String>]) {
    for n in nodes {
        match &n.kind {
            NodeKind::Dir => attach_hunks(&mut n.children, labels),
            NodeKind::File { index, .. } => {
                let i = *index;
                if !labels[i].is_empty() {
                    n.children = labels[i]
                        .iter()
                        .enumerate()
                        .map(|(k, label)| Node {
                            label: label.clone(),
                            path: n.path.clone(),
                            kind: NodeKind::Hunk {
                                file_index: i,
                                hunk_index: k,
                            },
                            children: Vec::new(),
                            files: vec![i],
                        })
                        .collect();
                }
            }
            NodeKind::Hunk { .. } => {}
        }
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
            NodeKind::Hunk { .. } => {} // its file node already contributed
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

    fn hunk_map(path: &str, n: usize) -> HashMap<String, Vec<HunkInfo>> {
        let mut m = HashMap::new();
        m.insert(
            path.to_string(),
            (0..n)
                .map(|k| HunkInfo {
                    header: format!("@@ -{k} +{k} @@"),
                    text: format!("@@ -{k} +{k} @@\n+line{k}\n"),
                })
                .collect(),
        );
        m
    }

    #[test]
    fn with_hunks_adds_children_only_for_multi_hunk_files() {
        let c = changes(&["src/a.rs", "src/b.rs"]);
        let mut t = TreeModel::build(&c);
        let mut hunks = hunk_map("src/a.rs", 3);
        hunks.extend(hunk_map("src/b.rs", 1)); // single hunk → no children
        t.with_hunks(&c, &hunks);
        let dir = &t.roots[0];
        let a = dir.children.iter().find(|n| n.label == "a.rs").unwrap();
        let b = dir.children.iter().find(|n| n.label == "b.rs").unwrap();
        assert_eq!(a.children.len(), 3);
        assert!(b.children.is_empty());
        assert!(matches!(
            a.children[1].kind,
            NodeKind::Hunk {
                file_index: 0,
                hunk_index: 1
            }
        ));
        assert_eq!(a.children[1].id(), "src/a.rs#1");
    }

    #[test]
    fn hunk_toggle_drives_tristate_and_selectors() {
        let c = changes(&["src/a.rs", "src/b.rs"]);
        let mut t = TreeModel::build(&c);
        t.with_hunks(&c, &hunk_map("src/a.rs", 3));

        // Everything starts fully selected → no partial set.
        assert!(t.selected_partial(&c).is_empty());
        assert_eq!(t.selected_whole_paths(&c).len(), 2);

        // Drop one hunk: the file (and the dir above) turn partial.
        t.toggle("src/a.rs#1");
        let a = t.find("src/a.rs").unwrap().clone();
        assert_eq!(t.state_of(&a), SelectState::Partial);
        assert_eq!(t.state_of(&t.roots[0].clone()), SelectState::Partial);
        assert_eq!(
            t.selected_partial(&c),
            vec![("src/a.rs".into(), vec![0, 2])]
        );
        // The partial file leaves the whole list but stays in selected_paths.
        assert_eq!(t.selected_whole_paths(&c), vec!["src/b.rs".to_string()]);
        assert_eq!(t.selected_paths(&c).len(), 2);
        assert_eq!(t.selected_count(), 2);

        // Toggling the file node from partial selects every hunk again…
        t.toggle("src/a.rs");
        assert!(t.selected_partial(&c).is_empty());
        assert_eq!(t.selected_whole_paths(&c).len(), 2);
        // …and from full it clears them all.
        t.toggle("src/a.rs");
        let a = t.find("src/a.rs").unwrap().clone();
        assert_eq!(t.state_of(&a), SelectState::None);
        assert_eq!(t.selected_count(), 1);

        // A single selected hunk: partial again; hunk node states differ.
        t.toggle("src/a.rs#2");
        let on = t.find("src/a.rs#2").unwrap().clone();
        let off = t.find("src/a.rs#0").unwrap().clone();
        assert_eq!(t.state_of(&on), SelectState::All);
        assert_eq!(t.state_of(&off), SelectState::None);
        assert_eq!(t.selected_partial(&c), vec![("src/a.rs".into(), vec![2])]);
    }

    #[test]
    fn hunk_children_survive_filtered_rebuilds() {
        let c = changes(&["src/a.rs", "docs/x.md"]);
        let mut t = TreeModel::build(&c);
        t.with_hunks(&c, &hunk_map("src/a.rs", 2));
        t.toggle("src/a.rs#0");
        // Narrow to src/ and back — children and hunk marks survive.
        t.rebuild_view(&c, &[0]);
        assert_eq!(t.find("src/a.rs").unwrap().children.len(), 2);
        t.rebuild_view(&c, &[0, 1]);
        assert_eq!(t.selected_partial(&c), vec![("src/a.rs".into(), vec![1])]);
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
