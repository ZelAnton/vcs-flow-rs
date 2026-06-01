//! Backend abstraction over git and jj.
//!
//! The published `vcs-git`/`vcs-jj` 0.1 APIs are thin, so most work goes through
//! each client's `run` escape hatch with machine-readable porcelain/templates
//! (never human output). All paths are normalised to forward slashes on ingest;
//! commands run with the process CWD set to the repo root (see `main`).

use std::collections::HashMap;
use std::path::PathBuf;

use vcs_git::{Git, GitApi};
use vcs_jj::{Jj, JjApi, Result};

use crate::model::{BackendKind, ChangeKind, FileChange, Target};
use crate::repo::RepoLocation;

/// The changed files plus a per-file unified diff, captured once at startup.
pub struct Snapshot {
    pub changes: Vec<FileChange>,
    /// Unified diff text keyed by [`FileChange::path`] — the same path shown in
    /// the tree, so a selected file always resolves to its diff.
    pub diffs: HashMap<String, String>,
}

/// A git or jj repository the tool operates on.
pub enum Backend {
    Git { root: PathBuf, client: Git },
    Jj { root: PathBuf, client: Jj },
}

impl Backend {
    pub fn new(loc: &RepoLocation) -> Self {
        match loc.kind {
            BackendKind::Git => Backend::Git {
                root: loc.root.clone(),
                client: Git::new(),
            },
            BackendKind::Jj => Backend::Jj {
                root: loc.root.clone(),
                client: Jj::new(),
            },
        }
    }

    pub fn kind(&self) -> BackendKind {
        match self {
            Backend::Git { .. } => BackendKind::Git,
            Backend::Jj { .. } => BackendKind::Jj,
        }
    }

    /// Collect the changed tracked files (ignoring the index) and their diffs.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        match self {
            Backend::Git { client, .. } => {
                // Unborn repo (no initial commit): `diff HEAD` would error, but
                // there are no tracked files yet — report nothing to commit.
                let head = client
                    .run_raw(&args(&["rev-parse", "--verify", "-q", "HEAD"]))
                    .await?;
                if !head.is_success() {
                    return Ok(Snapshot {
                        changes: Vec::new(),
                        diffs: HashMap::new(),
                    });
                }
                // `diff HEAD` = working tree vs HEAD: all tracked changes whether
                // staged or not, excluding untracked. Both the file list and the
                // per-file diffs come from this one git-format diff.
                let full = client
                    .run(&args(&[
                        "diff",
                        "HEAD",
                        "--no-color",
                        "--no-ext-diff",
                        "-M",
                    ]))
                    .await?;
                Ok(snapshot_from_diff(&full))
            }
            Backend::Jj { client, .. } => {
                let full = client.run(&args(&["diff", "-r", "@", "--git"])).await?;
                Ok(snapshot_from_diff(&full))
            }
        }
    }

    /// Where the commit can land. git: the current branch (one). jj: the nearest
    /// bookmarks reachable from `@`; if none, every bookmark plus a describe-only
    /// option (empty label).
    pub async fn targets(&self) -> Result<Vec<Target>> {
        match self {
            Backend::Git { root, client } => {
                let branch = client.current_branch(root).await?;
                let branch = branch.trim();
                // `current_branch` returns the literal "HEAD" when detached; make
                // that explicit so the UI doesn't claim a branch named "HEAD".
                let label = if branch == "HEAD" {
                    let short = client
                        .run(&args(&["rev-parse", "--short", "HEAD"]))
                        .await
                        .unwrap_or_default();
                    format!("detached HEAD @ {}", short.trim())
                } else {
                    branch.to_string()
                };
                Ok(vec![Target {
                    label,
                    revision: None,
                }])
            }
            Backend::Jj { root, client } => {
                let template = "local_bookmarks ++ \"\\x1f\" ++ commit_id.short() ++ \"\\n\"";
                let out = client
                    .run(&args(&[
                        "log",
                        "-r",
                        "heads(::@ & bookmarks())",
                        "--no-graph",
                        "-T",
                        template,
                    ]))
                    .await?;
                let mut targets = parse_jj_targets(&out);
                if targets.is_empty() {
                    for b in client.bookmarks(root).await? {
                        // Skip remote-tracking bookmarks (`name@remote`) — only a
                        // local bookmark can be advanced.
                        if b.name.contains('@') {
                            continue;
                        }
                        targets.push(Target {
                            label: b.name,
                            revision: Some(b.target),
                        });
                    }
                    // Always offer "commit without moving a bookmark".
                    targets.push(Target {
                        label: String::new(),
                        revision: None,
                    });
                }
                Ok(targets)
            }
        }
    }

    /// The message to pre-fill the editor with for `target`.
    pub async fn message_for(&self, target: &Target, amend: bool) -> Result<String> {
        match self {
            Backend::Git { client, .. } => {
                if amend {
                    Ok(client
                        .run(&args(&["log", "-1", "--format=%B"]))
                        .await?
                        .trim_end()
                        .to_string())
                } else {
                    Ok(String::new())
                }
            }
            Backend::Jj { client, .. } => {
                let revset = if amend && !target.label.is_empty() {
                    target.label.as_str()
                } else {
                    "@"
                };
                let out = client
                    .run(&args(&[
                        "log",
                        "-r",
                        revset,
                        "--no-graph",
                        "-T",
                        "description",
                    ]))
                    .await?;
                Ok(out.trim_end().to_string())
            }
        }
    }

    /// Perform the commit of `paths` (repo-relative, forward slashes) with
    /// `message`, optionally amending, onto `target`.
    pub async fn commit(
        &self,
        paths: &[String],
        message: &str,
        amend: bool,
        target: &Target,
    ) -> Result<()> {
        match self {
            Backend::Git { client, .. } => {
                client.run(&git_commit_args(paths, message, amend)).await?;
                Ok(())
            }
            Backend::Jj { client, .. } => {
                // Amend needs an existing bookmark commit to fold into; with the
                // describe-only target (no bookmark) fall back to a normal commit.
                if amend && !target.label.is_empty() {
                    // Squash the selected paths from @ into the bookmark's commit.
                    let revset = target.label.as_str();
                    client.run(&jj_squash_args(paths, revset)).await?;
                    // Update the commit's description only if the user changed it.
                    let current = self.message_for(target, true).await?;
                    if current.trim() != message.trim() {
                        client.run(&jj_describe_args(revset, message)).await?;
                    }
                } else {
                    // `jj commit <paths>` finalises a commit with the selected
                    // paths; the rest move into the new working copy `@`.
                    client.run(&jj_commit_args(paths, message)).await?;
                    // Advance the chosen bookmark onto the finalised commit (`@-`).
                    if !target.label.is_empty() {
                        client.run(&jj_bookmark_set_args(&target.label)).await?;
                    }
                }
                Ok(())
            }
        }
    }
}

/// `git commit [--amend] -m <msg> --only -- <paths>` — commit exactly these
/// paths' working-tree content, regardless of the index.
fn git_commit_args(paths: &[String], message: &str, amend: bool) -> Vec<String> {
    let mut a: Vec<String> = vec!["commit".into()];
    if amend {
        a.push("--amend".into());
    }
    a.push("-m".into());
    a.push(message.into());
    a.push("--only".into());
    a.push("--".into());
    a.extend(paths.iter().cloned());
    a
}

/// `jj commit -m <msg> <filesets>` — finalise a commit with just these paths.
fn jj_commit_args(paths: &[String], message: &str) -> Vec<String> {
    let mut a: Vec<String> = vec!["commit".into(), "-m".into(), message.into()];
    a.extend(paths.iter().map(|p| jj_fileset(p)));
    a
}

/// `jj bookmark set <name> -r @-` — advance a bookmark onto the new commit.
fn jj_bookmark_set_args(label: &str) -> Vec<String> {
    args(&["bookmark", "set", label, "-r", "@-"])
}

/// `jj squash --from @ --into <revset> <filesets>` — fold these paths into a commit.
fn jj_squash_args(paths: &[String], revset: &str) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "squash".into(),
        "--from".into(),
        "@".into(),
        "--into".into(),
        revset.into(),
    ];
    a.extend(paths.iter().map(|p| jj_fileset(p)));
    a
}

/// Wrap a path as an exact-path jj fileset (`file:"<path>"`) so metacharacters
/// like `(`, `)`, `|`, `*` in the path are treated literally, not as fileset
/// operators. Paths are repo-root-relative and the tool runs from the root.
fn jj_fileset(path: &str) -> String {
    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("file:\"{escaped}\"")
}

/// `jj describe -r <revset> -m <msg>`.
fn jj_describe_args(revset: &str, message: &str) -> Vec<String> {
    args(&["describe", "-r", revset, "-m", message])
}

/// Build an owned-arg vector from string literals.
fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

/// Parse the nearest-bookmark template output: `name [name...]\x1f<commit>` per
/// line. A commit can carry several bookmarks; each becomes its own target.
fn parse_jj_targets(out: &str) -> Vec<Target> {
    let mut res = Vec::new();
    for line in out.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '\u{1f}');
        let names = parts.next().unwrap_or("").trim();
        let rev = parts.next().unwrap_or("").trim();
        for name in names.split_whitespace() {
            res.push(Target {
                label: name.to_string(),
                revision: (!rev.is_empty()).then(|| rev.to_string()),
            });
        }
    }
    res
}

/// Build a [`Snapshot`] from a single git-format diff (git's `diff HEAD` or jj's
/// `diff --git`). The change list and per-file diffs both come from here, so the
/// tree path and the diff key always agree. Paths are read from the unambiguous
/// single-path lines (`+++ b/…`, `--- a/…`, `rename to …`) rather than the
/// space-ambiguous `diff --git a/… b/…` header.
fn snapshot_from_diff(full: &str) -> Snapshot {
    let mut changes = Vec::new();
    let mut diffs = HashMap::new();
    for section in diff_sections(full) {
        if let Some(change) = section_change(section) {
            diffs.insert(change.path.clone(), section.to_string());
            changes.push(change);
        }
    }
    Snapshot { changes, diffs }
}

/// Slice a git-format diff into per-file sections (each starts at `diff --git`).
fn diff_sections(full: &str) -> Vec<&str> {
    let mut sections = Vec::new();
    let mut start = None;
    let mut idx = 0;
    for line in full.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            if let Some(s) = start {
                sections.push(&full[s..idx]);
            }
            start = Some(idx);
        }
        idx += line.len();
    }
    if let Some(s) = start {
        sections.push(&full[s..]);
    }
    sections
}

/// Determine the [`FileChange`] for one diff section. The path comes from a
/// single-path line so a directory containing spaces parses correctly; the
/// `diff --git` header is only a binary-file fallback.
fn section_change(section: &str) -> Option<FileChange> {
    let mut kind = ChangeKind::Modified;
    let mut new_path = None;
    let mut minus_path = None;
    let mut rename_to = None;
    let mut rename_from = None;

    for line in section.lines() {
        if line.starts_with("new file") {
            kind = ChangeKind::Added;
        } else if line.starts_with("deleted file") {
            kind = ChangeKind::Deleted;
        } else if let Some(p) = line.strip_prefix("rename to ") {
            rename_to = Some(p.trim_end().to_string());
        } else if let Some(p) = line.strip_prefix("rename from ") {
            rename_from = Some(p.trim_end().to_string());
        } else if let Some(p) = line.strip_prefix("+++ b/") {
            new_path = Some(p.trim_end().to_string());
        } else if let Some(p) = line.strip_prefix("--- a/") {
            minus_path = Some(p.trim_end().to_string());
        }
    }
    let normalize = |p: String| p.replace('\\', "/");

    // A rename: keep the old path so the commit records the deletion too.
    let old_path = if rename_to.is_some() {
        kind = ChangeKind::Renamed;
        rename_from.map(normalize)
    } else {
        None
    };

    let path = rename_to
        .or(new_path)
        .or(minus_path)
        .or_else(|| header_b_path(section))?;
    Some(FileChange {
        path: normalize(path),
        old_path,
        kind,
    })
}

/// Fallback path extraction for sections with no `+++`/`---`/`rename` lines
/// (e.g. binary files): the `b/<new>` of the `diff --git` header. Ambiguous only
/// when a path contains the literal `" b/"`, which binary-with-spaces makes rare.
fn header_b_path(section: &str) -> Option<String> {
    let first = section.lines().next()?;
    let s = first.strip_prefix("diff --git ")?;
    let idx = s.find(" b/")?;
    Some(s[idx + 1..].strip_prefix("b/").unwrap_or("").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_parser_covers_add_modify_delete_rename() {
        // Add (new), modify (mod), delete (gone), and a directory-changing rename
        // (old/f -> new/f) — the case that broke the old jj `--summary` parser.
        let full = concat!(
            "diff --git a/new b/new\n",
            "new file mode 100644\n--- /dev/null\n+++ b/new\n@@ -0,0 +1 @@\n+n\n",
            "diff --git a/mod b/mod\n",
            "--- a/mod\n+++ b/mod\n@@ -1 +1 @@\n-a\n+b\n",
            "diff --git a/gone b/gone\n",
            "deleted file mode 100644\n--- a/gone\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n",
            "diff --git a/old/f.txt b/new/f.txt\n",
            "similarity index 100%\nrename from old/f.txt\nrename to new/f.txt\n",
        );
        let snap = snapshot_from_diff(full);
        let kinds: Vec<_> = snap
            .changes
            .iter()
            .map(|c| (c.path.as_str(), c.kind))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("new", ChangeKind::Added),
                ("mod", ChangeKind::Modified),
                ("gone", ChangeKind::Deleted),
                ("new/f.txt", ChangeKind::Renamed),
            ]
        );
        // The diff for the rename is keyed under the new path it lists in the tree.
        assert!(
            snap.diffs
                .get("new/f.txt")
                .unwrap()
                .contains("rename to new/f.txt")
        );
        // The rename carries its old path so the deletion is committed too.
        let rename = snap
            .changes
            .iter()
            .find(|c| c.kind == ChangeKind::Renamed)
            .unwrap();
        assert_eq!(rename.old_path.as_deref(), Some("old/f.txt"));
    }

    #[test]
    fn section_parser_handles_space_paths() {
        // git appends a trailing tab to `+++`/`---` paths containing spaces; the
        // path must survive intact (the `diff --git` header is ambiguous here).
        let full = "diff --git a/a b/c.txt b/a b/c.txt\n--- a/a b/c.txt\t\n+++ b/a b/c.txt\t\n@@ -1 +1 @@\n-x\n+y\n";
        let snap = snapshot_from_diff(full);
        assert_eq!(snap.changes.len(), 1);
        assert_eq!(snap.changes[0].path, "a b/c.txt");
        assert!(snap.diffs.contains_key("a b/c.txt"));
    }

    #[test]
    fn jj_fileset_quotes_metacharacters() {
        assert_eq!(jj_fileset("src/a(b).rs"), "file:\"src/a(b).rs\"");
        let args = jj_commit_args(&["x|y.rs".to_string()], "m");
        assert_eq!(args, ["commit", "-m", "m", "file:\"x|y.rs\""]);
    }

    #[test]
    fn parses_targets_multiple_names() {
        let out = "feat alt\u{1f}abc123\nmain\u{1f}def456\n";
        let ts = parse_jj_targets(out);
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[0].label, "feat");
        assert_eq!(ts[0].revision.as_deref(), Some("abc123"));
        assert_eq!(ts[1].label, "alt");
        assert_eq!(ts[2].label, "main");
    }

    #[test]
    fn git_commit_args_partial_and_amend() {
        let paths = vec!["src/a.rs".to_string(), "top.txt".to_string()];
        let normal = git_commit_args(&paths, "msg", false);
        assert_eq!(
            normal,
            ["commit", "-m", "msg", "--only", "--", "src/a.rs", "top.txt"]
        );
        let amend = git_commit_args(&paths, "msg", true);
        assert_eq!(amend[..2], ["commit", "--amend"]);
        assert!(amend.ends_with(&["--".to_string(), "src/a.rs".into(), "top.txt".into()]));
    }

    #[test]
    fn jj_command_args() {
        let paths = vec!["src/a.rs".to_string()];
        assert_eq!(
            jj_commit_args(&paths, "msg"),
            ["commit", "-m", "msg", "file:\"src/a.rs\""]
        );
        assert_eq!(
            jj_bookmark_set_args("feat"),
            ["bookmark", "set", "feat", "-r", "@-"]
        );
        assert_eq!(
            jj_squash_args(&paths, "feat"),
            [
                "squash",
                "--from",
                "@",
                "--into",
                "feat",
                "file:\"src/a.rs\""
            ]
        );
        assert_eq!(
            jj_describe_args("feat", "msg"),
            ["describe", "-r", "feat", "-m", "msg"]
        );
    }
}
