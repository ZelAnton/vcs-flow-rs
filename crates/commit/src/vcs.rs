//! Backend abstraction over git and jj.
//!
//! The published `vcs-git`/`vcs-jj` 0.1 APIs are thin, so most work goes through
//! each client's `run` escape hatch with machine-readable porcelain/templates
//! (never human output). All paths are normalised to forward slashes on ingest;
//! commands run with the process CWD set to the repo root (see `main`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use processkit::ProcessResult;
use vcs_git::{Git, GitApi};
use vcs_jj::{Jj, JjApi, Result};

use crate::model::{BackendKind, ChangeKind, FileChange, Target};
use crate::repo::RepoLocation;
use crate::settings::PullStrategy;

/// The remote `commit` pushes to. The tools here assume the conventional `origin`.
const REMOTE: &str = "origin";

/// Outcome of integrating remote commits (merge/rebase) before a push.
pub enum Integration {
    /// Integration completed cleanly — ready to push.
    Clean,
    /// Conflicts remain in these (repo-relative) paths; the user must resolve them.
    Conflicts(Vec<String>),
}

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

    // ----- push flow -------------------------------------------------------

    /// The branch (git) / bookmark (jj) a push would advance, or `None` when there
    /// is nothing pushable (git detached HEAD, or a jj describe-only target).
    pub async fn push_name(&self, target: &Target) -> Result<Option<String>> {
        match self {
            Backend::Git { root, client } => {
                let b = client.current_branch(root).await?.trim().to_string();
                Ok((!b.is_empty() && b != "HEAD").then_some(b))
            }
            Backend::Jj { .. } => {
                let l = target.label.trim();
                Ok((!l.is_empty()).then(|| l.to_string()))
            }
        }
    }

    /// The remote-side branch name `name` already tracks on `origin`, or `None`
    /// when untracked.
    pub async fn upstream(&self, name: &str) -> Result<Option<String>> {
        match self {
            Backend::Git { client, .. } => {
                let r = client
                    .run_raw(&args(&[
                        "rev-parse",
                        "--abbrev-ref",
                        "--symbolic-full-name",
                        "@{u}",
                    ]))
                    .await?;
                // Non-zero exit = no upstream configured.
                Ok(r.is_success()
                    .then(|| parse_git_upstream(r.stdout()))
                    .flatten())
            }
            Backend::Jj { client, .. } => {
                let out = client.run(&args(&["bookmark", "list", "-a"])).await?;
                Ok(jj_upstream(&out, name))
            }
        }
    }

    /// Fetch from `origin` so remote-tracking refs/bookmarks are current.
    pub async fn fetch(&self) -> Result<()> {
        match self {
            Backend::Git { client, .. } => {
                client.run(&args(&["fetch", REMOTE])).await?;
                Ok(())
            }
            Backend::Jj { root, client } => client.git_fetch(root).await,
        }
    }

    /// Existing branch names on `origin`.
    pub async fn remote_branches(&self) -> Result<Vec<String>> {
        match self {
            Backend::Git { client, .. } => {
                let out = client.run(&args(&["ls-remote", "--heads", REMOTE])).await?;
                Ok(parse_ls_remote_heads(&out))
            }
            Backend::Jj { client, .. } => {
                let out = client.run(&args(&["bookmark", "list", "-a"])).await?;
                Ok(jj_remote_branches(&out))
            }
        }
    }

    /// Attach the local branch/bookmark to `origin/<remote_branch>` and track it.
    /// Returns the local name to use afterwards: unchanged for git (which pushes a
    /// `local:remote` refspec), but for jj — which tracks by matching name — the
    /// local bookmark is *renamed* to the remote branch when they differ, so the
    /// returned name equals `remote_branch` in that case. Errors (without mutating)
    /// if that rename would collide with an existing local jj bookmark.
    pub async fn attach(&self, name: &str, remote_branch: &str) -> crate::AppResult<String> {
        match self {
            Backend::Git { client, .. } => {
                let _ = client
                    .run_raw(&args(&[
                        "branch",
                        &format!("--set-upstream-to={REMOTE}/{remote_branch}"),
                        name,
                    ]))
                    .await?;
                Ok(name.to_string())
            }
            Backend::Jj { client, root } => {
                let local = if remote_branch == name {
                    name.to_string()
                } else {
                    // The local work becomes that remote branch (jj has no
                    // differently-named tracking, and this avoids a stray bookmark).
                    // Refuse up front if a local bookmark already owns that name —
                    // jj would reject the rename and leave the push half-done.
                    if client
                        .bookmarks(root)
                        .await?
                        .iter()
                        .any(|b| b.name == remote_branch)
                    {
                        return Err(format!(
                            "a local bookmark '{remote_branch}' already exists; rename or \
                             delete it, or push '{name}' as its own branch (Ctrl+N)"
                        )
                        .into());
                    }
                    client
                        .run(&args(&["bookmark", "rename", name, remote_branch]))
                        .await?;
                    remote_branch.to_string()
                };
                let _ = client
                    .run_raw(&args(&["bookmark", "track", &local, "--remote", REMOTE]))
                    .await?;
                Ok(local)
            }
        }
    }

    /// Whether the working tree has uncommitted changes to tracked files (which
    /// would block a git rebase/merge integration). Always `false` for jj — a jj
    /// commit already moved unselected changes into the new working-copy change.
    pub async fn working_tree_dirty(&self) -> Result<bool> {
        match self {
            Backend::Git { client, .. } => {
                let out = client
                    .run(&args(&["status", "--porcelain", "--untracked-files=no"]))
                    .await?;
                Ok(!out.trim().is_empty())
            }
            Backend::Jj { .. } => Ok(false),
        }
    }

    /// Whether `name` is behind `origin/<remote_branch>` (the remote has commits the
    /// local branch lacks). Caller fetches first.
    pub async fn behind(&self, name: &str, remote_branch: &str) -> Result<bool> {
        match self {
            Backend::Git { client, .. } => {
                let spec = format!("{name}...{REMOTE}/{remote_branch}");
                let r = client
                    .run_raw(&args(&["rev-list", "--left-right", "--count", &spec]))
                    .await?;
                // Failure = the remote ref doesn't exist locally yet → not behind.
                Ok(r.is_success() && parse_behind_count(r.stdout()))
            }
            Backend::Jj { client, .. } => {
                let revset = format!("{remote_branch}@{REMOTE} ~ ::{name}");
                let out = client
                    .run(&args(&[
                        "log",
                        "-r",
                        &revset,
                        "--no-graph",
                        "-T",
                        "commit_id",
                    ]))
                    .await?;
                Ok(!out.trim().is_empty())
            }
        }
    }

    /// Capture the current jj operation id so a failed integration can be rolled
    /// back with `op restore`. `None` for git (which uses `--abort` instead).
    pub async fn pre_integration_op(&self) -> Result<Option<String>> {
        match self {
            Backend::Jj { client, .. } => {
                let id = client
                    .run(&args(&[
                        "op",
                        "log",
                        "--no-graph",
                        "-T",
                        "id.short()",
                        "--limit",
                        "1",
                    ]))
                    .await?;
                Ok(Some(id.trim().to_string()))
            }
            Backend::Git { .. } => Ok(None),
        }
    }

    /// Integrate `origin/<remote_branch>` into `name` (merge or rebase per
    /// `strategy`; jj always rebases). Returns [`Integration::Conflicts`] with the
    /// conflicted paths rather than erroring on a conflict.
    pub async fn integrate(
        &self,
        name: &str,
        remote_branch: &str,
        strategy: PullStrategy,
    ) -> Result<Integration> {
        match self {
            Backend::Git { client, .. } => {
                let target = format!("{REMOTE}/{remote_branch}");
                let r = match strategy {
                    PullStrategy::Merge => {
                        client
                            .run_raw(&args(&["merge", "--no-edit", &target]))
                            .await?
                    }
                    PullStrategy::Rebase => client.run_raw(&git_rebase_args(&target)).await?,
                };
                if !r.is_success() {
                    let files = self.git_conflicted_files().await?;
                    if !files.is_empty() {
                        return Ok(Integration::Conflicts(files));
                    }
                    // A non-conflict failure: surface the real error.
                    r.ensure_success()?;
                }
                Ok(Integration::Clean)
            }
            Backend::Jj { client, .. } => {
                let dest = format!("{remote_branch}@{REMOTE}");
                client
                    .run(&args(&["rebase", "-b", name, "-d", &dest]))
                    .await?;
                self.jj_integration_state(name).await
            }
        }
    }

    /// Re-check (and, for git, advance) an in-progress integration after the user
    /// resolved conflicts.
    pub async fn continue_integration(
        &self,
        name: &str,
        strategy: PullStrategy,
    ) -> Result<Integration> {
        match self {
            Backend::Git { client, root } => {
                let files = self.git_conflicted_files().await?;
                if !files.is_empty() {
                    return Ok(Integration::Conflicts(files));
                }
                match strategy {
                    PullStrategy::Merge => {
                        if git_path_exists(client, root, "MERGE_HEAD").await {
                            client.run(&args(&["commit", "--no-edit"])).await?;
                        }
                        Ok(Integration::Clean)
                    }
                    PullStrategy::Rebase => {
                        // Advance one step (the top-of-fn check already confirmed no
                        // unmerged files), then re-surface state so the caller
                        // re-prompts — never spin without a prompt between steps.
                        let r = client.run_raw(&git_rebase_continue_args()).await?;
                        if r.is_success() && !git_rebase_in_progress(client, root).await {
                            Ok(Integration::Clean)
                        } else {
                            // A later step (fresh conflict) or a refused continue
                            // (e.g. a still-marked file was staged): back to the user.
                            Ok(Integration::Conflicts(self.git_conflicted_files().await?))
                        }
                    }
                }
            }
            Backend::Jj { client, .. } => {
                // The conflict lives in the bookmark commit; the user resolves it in
                // the working copy (its conflicted descendant). Fold that resolution
                // into the bookmark — jj's prescribed `jj squash` step — then re-check.
                // A no-op (nothing to squash) just leaves the bookmark conflicted.
                let _ = client
                    .run_raw(&args(&[
                        "squash",
                        "--into",
                        name,
                        "--use-destination-message",
                    ]))
                    .await?;
                self.jj_integration_state(name).await
            }
        }
    }

    /// Roll back a failed/abandoned integration. Returns whether the rollback
    /// succeeded, so the caller doesn't falsely claim a clean abort.
    pub async fn abort_integration(
        &self,
        strategy: PullStrategy,
        jj_pre_op: Option<&str>,
    ) -> Result<bool> {
        match self {
            Backend::Git { client, .. } => {
                let sub = match strategy {
                    PullStrategy::Merge => "merge",
                    PullStrategy::Rebase => "rebase",
                };
                let r = client.run_raw(&args(&[sub, "--abort"])).await?;
                Ok(r.is_success())
            }
            Backend::Jj { client, .. } => match jj_pre_op {
                Some(op) => Ok(client
                    .run_raw(&args(&["op", "restore", op]))
                    .await?
                    .is_success()),
                None => Ok(true),
            },
        }
    }

    /// Push `name` to `origin/<remote_branch>`, optionally setting upstream. Returns
    /// the raw result so the caller can report a rejection (e.g. non-fast-forward).
    pub async fn push(
        &self,
        name: &str,
        remote_branch: &str,
        set_upstream: bool,
    ) -> Result<ProcessResult<String>> {
        match self {
            Backend::Git { client, .. } => {
                let mut a = vec!["push".to_string()];
                if set_upstream {
                    a.push("-u".into());
                }
                a.push(REMOTE.into());
                a.push(format!("{name}:{remote_branch}"));
                client.run_raw(&a).await
            }
            Backend::Jj { client, .. } => {
                // `name` is already the bookmark to push (attach renames it to match
                // the remote branch). jj auto-creates+tracks a new bookmark on push.
                let _ = (remote_branch, set_upstream);
                client.run_raw(&args(&["git", "push", "-b", name])).await
            }
        }
    }

    /// git-only: the repo-relative paths with unresolved (unmerged) conflicts.
    async fn git_conflicted_files(&self) -> Result<Vec<String>> {
        let Backend::Git { client, .. } = self else {
            return Ok(Vec::new());
        };
        let out = client
            .run(&args(&["diff", "--name-only", "--diff-filter=U"]))
            .await?;
        Ok(out
            .lines()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect())
    }

    /// jj-only: conflicted paths in the bookmark commit `name` (empty when clean).
    async fn jj_conflicted_files(&self, name: &str) -> Result<Vec<String>> {
        let Backend::Jj { client, .. } = self else {
            return Ok(Vec::new());
        };
        let r = client
            .run_raw(&args(&["resolve", "--list", "-r", name]))
            .await?;
        // `resolve --list` exits non-zero when there are no conflicts.
        Ok(if r.is_success() {
            parse_jj_resolve_list(r.stdout())
        } else {
            Vec::new()
        })
    }

    /// jj-only: classify the bookmark commit as clean or conflicted.
    async fn jj_integration_state(&self, name: &str) -> Result<Integration> {
        let Backend::Jj { client, .. } = self else {
            return Ok(Integration::Clean);
        };
        let flag = client
            .run(&args(&[
                "log",
                "-r",
                name,
                "--no-graph",
                "-T",
                "if(conflict, \"1\", \"\")",
            ]))
            .await?;
        if flag.trim().is_empty() {
            Ok(Integration::Clean)
        } else {
            Ok(Integration::Conflicts(
                self.jj_conflicted_files(name).await?,
            ))
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

/// `git ... rebase <target>` with the editor disabled so `--continue` never opens
/// one (`core.editor=true` is a no-op editor that exits 0).
fn git_rebase_args(target: &str) -> Vec<String> {
    args(&["-c", "core.editor=true", "rebase", target])
}

/// `git -c core.editor=true rebase --continue`.
fn git_rebase_continue_args() -> Vec<String> {
    args(&["-c", "core.editor=true", "rebase", "--continue"])
}

/// Strip the remote prefix from a `git @{u}` value (`origin/feat/x` → `feat/x`).
/// `None` if there's no `/` (not a remote-qualified ref).
fn parse_git_upstream(upstream: &str) -> Option<String> {
    let s = upstream.trim();
    s.split_once('/').map(|(_, branch)| branch.to_string())
}

/// Branch names from `git ls-remote --heads origin` (`<sha>\trefs/heads/<name>`).
fn parse_ls_remote_heads(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split_once('\t'))
        .filter_map(|(_, r)| r.trim().strip_prefix("refs/heads/"))
        .map(str::to_string)
        .collect()
}

/// Whether `name` is tracked on `origin`, from `jj bookmark list -a`. A tracked
/// remote shows as an indented `@origin:` line under the local `name:` block.
fn jj_upstream(out: &str, name: &str) -> Option<String> {
    let header = format!("{name}:");
    let mut in_block = false;
    for line in out.lines() {
        let indented = line.starts_with(char::is_whitespace);
        if !indented {
            in_block = line.trim_end().starts_with(&header);
        } else if in_block && line.trim_start().starts_with(&format!("@{REMOTE}:")) {
            return Some(name.to_string());
        }
    }
    None
}

/// All branch names present on `origin`, from `jj bookmark list -a`: a top-level
/// remote-only `name@origin:` line, or an indented `@origin:` under a local block.
fn jj_remote_branches(out: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut local: Option<String> = None;
    let suffix = format!("@{REMOTE}");
    for line in out.lines() {
        if line.starts_with(char::is_whitespace) {
            // Indented remote line: `@origin: ...` belongs to the current local block.
            if line.trim_start().starts_with(&format!("@{REMOTE}:"))
                && let Some(n) = &local
            {
                names.push(n.clone());
            }
        } else if let Some((head, _)) = line.split_once(':') {
            let head = head.trim();
            if let Some(n) = head.strip_suffix(&suffix) {
                names.push(n.to_string()); // top-level remote-only bookmark
                local = None;
            } else {
                local = Some(head.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Parse `git rev-list --left-right --count A...B` (`<ahead>\t<behind>`): behind>0.
fn parse_behind_count(out: &str) -> bool {
    out.split_whitespace()
        .nth(1)
        .and_then(|n| n.parse::<u64>().ok())
        .is_some_and(|behind| behind > 0)
}

/// Conflicted paths from `jj resolve --list` (`<path>   <description>` per line).
fn parse_jj_resolve_list(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// Whether the git path `rel` (e.g. `MERGE_HEAD`) exists, resolved via
/// `git rev-parse --git-path` so it's correct in worktrees too.
async fn git_path_exists(client: &Git, root: &Path, rel: &str) -> bool {
    let Ok(p) = client.run(&args(&["rev-parse", "--git-path", rel])).await else {
        return false;
    };
    let p = p.trim();
    let path = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        root.join(p)
    };
    path.exists()
}

/// Whether a git rebase is mid-flight (either backend leaves a state directory).
async fn git_rebase_in_progress(client: &Git, root: &Path) -> bool {
    git_path_exists(client, root, "rebase-merge").await
        || git_path_exists(client, root, "rebase-apply").await
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
    fn parses_git_upstream_and_behind_count() {
        assert_eq!(parse_git_upstream("origin/main"), Some("main".into()));
        // A branch name containing slashes keeps everything after the remote.
        assert_eq!(parse_git_upstream("origin/feat/x"), Some("feat/x".into()));
        assert_eq!(parse_git_upstream("HEAD"), None);
        // `<ahead>\t<behind>`.
        assert!(parse_behind_count("0\t3"));
        assert!(parse_behind_count("2\t1"));
        assert!(!parse_behind_count("4\t0"));
        assert!(!parse_behind_count(""));
    }

    #[test]
    fn parses_ls_remote_heads() {
        let out = "abc123\trefs/heads/main\ndef456\trefs/heads/feature/x\n\
                   789aaa\trefs/tags/v1\n";
        // Only heads; tags ignored.
        assert_eq!(parse_ls_remote_heads(out), vec!["main", "feature/x"]);
    }

    #[test]
    fn jj_upstream_and_remote_branches_from_bookmark_list() {
        // `main` tracked on origin (indented @origin); `feature` is local-only;
        // `release` exists only on the remote (top-level name@origin).
        let out = "main: qpr 2b70 desc\n  @git: qpr 2b70 desc\n  @origin: qpr 2b70 desc\n\
                   feature: abcd 1234 wip\n\
                   release@origin: zzzz 9999 shipped\n";
        assert_eq!(jj_upstream(out, "main"), Some("main".into()));
        assert_eq!(jj_upstream(out, "feature"), None); // local-only, untracked
        let mut remotes = jj_remote_branches(out);
        remotes.sort();
        assert_eq!(remotes, vec!["main", "release"]);
    }

    #[test]
    fn parses_jj_resolve_list() {
        let out = "src/a.rs    2-sided conflict\nsrc/b.rs    2-sided conflict\n";
        assert_eq!(parse_jj_resolve_list(out), vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn rebase_arg_builders() {
        assert_eq!(
            git_rebase_args("origin/main"),
            ["-c", "core.editor=true", "rebase", "origin/main"]
        );
        assert_eq!(
            git_rebase_continue_args(),
            ["-c", "core.editor=true", "rebase", "--continue"]
        );
    }

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
