//! Backend operations over a [`vcs_core::Repo`] handle.
//!
//! `Repo` (from the `vcs-core` facade) detects git/jj and dispatches the common
//! surface; for everything it doesn't model we reach the typed `vcs_git::Git` /
//! `vcs_jj::Jj` clients via the `repo.git()`/`repo.jj()` escape hatches and call
//! the 0.3 typed methods (`commit_paths`, `diff_text`, `merge_continue`, `op_head`,
//! …). A handful of operations with no typed equivalent (the `@{u}` upstream,
//! `ls-remote`, the `bookmark list -a` parse, `resolve --list`, the refspec push,
//! and the editor-suppressed git rebase) still go through the clients' `run`
//! escape hatch. All operations run at `repo.cwd()`, which `Backend::open` binds to
//! the repo root; `main` also sets the process cwd to the root for the raw runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use processkit::ProcessResult;
use vcs_git::{DiffSpec as GitDiff, Git, GitApi};
use vcs_jj::{DiffSpec as JjDiff, Jj, JjApi, JjFileset};

use crate::AppResult;
use crate::model::{BackendKind, ChangeKind, FileChange, Target};
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

/// A git or jj repository the tool operates on, wrapping the `vcs-core` facade.
pub struct Backend {
    repo: vcs_core::Repo,
}

impl Backend {
    /// Detect the repo at or above `start` and bind the handle to its root.
    pub fn open(start: &Path) -> AppResult<Self> {
        let repo = vcs_core::Repo::open(start).map_err(|_| "not inside a git or jj repository")?;
        // Bind cwd to the root so facade/typed methods and the root-relative paths
        // we pass them agree (`open` binds cwd to `start`, which may be a subdir).
        let root = repo.root().to_path_buf();
        Ok(Backend {
            repo: repo.at(root),
        })
    }

    pub fn root(&self) -> &Path {
        self.repo.root()
    }

    fn cwd(&self) -> &Path {
        self.repo.cwd()
    }

    fn git(&self) -> Option<&Git> {
        self.repo.git()
    }

    fn jj(&self) -> Option<&Jj> {
        self.repo.jj()
    }

    pub fn kind(&self) -> BackendKind {
        match self.repo.kind() {
            vcs_core::BackendKind::Git => BackendKind::Git,
            vcs_core::BackendKind::Jj => BackendKind::Jj,
            _ => unreachable!("vcs_core::BackendKind is Git | Jj"),
        }
    }

    /// Collect the changed tracked files (ignoring the index) and their diffs.
    pub async fn snapshot(&self) -> AppResult<Snapshot> {
        let text = if let Some(g) = self.git() {
            // Unborn repo (no initial commit): nothing tracked yet.
            if g.is_unborn(self.cwd()).await? {
                return Ok(Snapshot {
                    changes: Vec::new(),
                    diffs: HashMap::new(),
                });
            }
            // `diff_text(WorkingTree)` is `diff HEAD --no-color --no-ext-diff -M` —
            // the git-format text `snapshot_from_diff` parses.
            g.diff_text(self.cwd(), GitDiff::WorkingTree).await?
        } else if let Some(j) = self.jj() {
            // jj `diff_text(WorkingTree)` is `diff -r @ --git`.
            j.diff_text(self.cwd(), JjDiff::WorkingTree).await?
        } else {
            unreachable!()
        };
        Ok(snapshot_from_diff(&text))
    }

    /// Where the commit can land. git: the current branch (one). jj: the nearest
    /// bookmarks reachable from `@`; if none, every bookmark plus a describe-only
    /// option (empty label).
    pub async fn targets(&self) -> AppResult<Vec<Target>> {
        if let Some(g) = self.git() {
            // Facade `current_branch` returns `None` when detached; surface that
            // explicitly rather than claiming a branch named "HEAD".
            let label = match self.repo.current_branch().await? {
                Some(b) => b,
                None => {
                    let short = g
                        .run(&args(&["rev-parse", "--short", "HEAD"]))
                        .await
                        .unwrap_or_default();
                    format!("detached HEAD @ {}", short.trim())
                }
            };
            Ok(vec![Target {
                label,
                revision: None,
            }])
        } else if let Some(j) = self.jj() {
            let template = "local_bookmarks ++ \"\\x1f\" ++ commit_id.short() ++ \"\\n\"";
            let out = j
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
                for b in j.bookmarks(self.cwd()).await? {
                    // Skip remote-tracking bookmarks (`name@remote`) — only a local
                    // bookmark can be advanced.
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
        } else {
            unreachable!()
        }
    }

    /// The message to pre-fill the editor with for `target`.
    pub async fn message_for(&self, target: &Target, amend: bool) -> AppResult<String> {
        if let Some(g) = self.git() {
            if amend {
                Ok(g.last_commit_message(self.cwd())
                    .await?
                    .trim_end()
                    .to_string())
            } else {
                Ok(String::new())
            }
        } else if let Some(j) = self.jj() {
            let revset = if amend && !target.label.is_empty() {
                target.label.as_str()
            } else {
                "@"
            };
            let out = j
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
        } else {
            unreachable!()
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
    ) -> AppResult<()> {
        if let Some(g) = self.git() {
            let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
            g.commit_paths(self.cwd(), &paths, message, amend).await?;
        } else if let Some(j) = self.jj() {
            let filesets: Vec<JjFileset> = paths.iter().map(JjFileset::path).collect();
            // Amend needs an existing bookmark commit to fold into; the describe-only
            // target (no bookmark) falls back to a normal commit.
            if amend && !target.label.is_empty() {
                // `--use-destination-message` keeps the bookmark commit's own
                // description so jj never opens an editor to *combine* it with `@`'s
                // (which `squash_paths`/`jj squash` does when both are described, and
                // would hang on the now-restored normal terminal). We set the final
                // message explicitly below.
                let mut squash = args(&[
                    "squash",
                    "--from",
                    "@",
                    "--into",
                    &target.label,
                    "--use-destination-message",
                ]);
                squash.extend(filesets.iter().map(|f| f.as_str().to_string()));
                j.run(&squash).await?;
                // Apply the chosen message only if the user changed it (the squash
                // kept the bookmark's prior message).
                let current = self.message_for(target, true).await?;
                if current.trim() != message.trim() {
                    j.run(&args(&["describe", "-r", &target.label, "-m", message]))
                        .await?;
                }
            } else {
                j.commit_paths(self.cwd(), &filesets, message).await?;
                // Advance the chosen bookmark onto the finalised commit (`@-`).
                if !target.label.is_empty() {
                    j.bookmark_set(self.cwd(), &target.label, "@-").await?;
                }
            }
        } else {
            unreachable!()
        }
        Ok(())
    }

    // ----- push flow -------------------------------------------------------

    /// The branch (git) / bookmark (jj) a push would advance, or `None` when there
    /// is nothing pushable (git detached HEAD, or a jj describe-only target).
    pub async fn push_name(&self, target: &Target) -> AppResult<Option<String>> {
        if self.git().is_some() {
            // `None` = detached HEAD.
            Ok(self.repo.current_branch().await?)
        } else {
            let l = target.label.trim();
            Ok((!l.is_empty()).then(|| l.to_string()))
        }
    }

    /// The remote-side branch name `name` already tracks on `origin`, or `None`
    /// when untracked.
    pub async fn upstream(&self, name: &str) -> AppResult<Option<String>> {
        if let Some(g) = self.git() {
            let r = g
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
        } else if let Some(j) = self.jj() {
            let out = j.run(&args(&["bookmark", "list", "-a"])).await?;
            Ok(jj_upstream(&out, name))
        } else {
            unreachable!()
        }
    }

    /// Fetch from `origin` so remote-tracking refs/bookmarks are current.
    pub async fn fetch(&self) -> AppResult<()> {
        if let Some(g) = self.git() {
            // Explicitly `origin`: the push flow reasons about `origin/<rb>`. The
            // facade `repo.fetch()` is bare `git fetch`, which would fetch the
            // branch's *configured* remote (not necessarily origin).
            g.run(&args(&["fetch", REMOTE])).await?;
        } else {
            // jj: `repo.fetch()` is `jj git fetch` — already the right thing.
            self.repo.fetch().await?;
        }
        Ok(())
    }

    /// Existing branch names on `origin`.
    pub async fn remote_branches(&self) -> AppResult<Vec<String>> {
        if let Some(g) = self.git() {
            let out = g.run(&args(&["ls-remote", "--heads", REMOTE])).await?;
            Ok(parse_ls_remote_heads(&out))
        } else if let Some(j) = self.jj() {
            let out = j.run(&args(&["bookmark", "list", "-a"])).await?;
            Ok(jj_remote_branches(&out))
        } else {
            unreachable!()
        }
    }

    /// Attach the local branch/bookmark to `origin/<remote_branch>` and track it.
    /// Returns the local name to use afterwards: unchanged for git (which pushes a
    /// `local:remote` refspec), but for jj — which tracks by matching name — the
    /// local bookmark is *renamed* to the remote branch when they differ, so the
    /// returned name equals `remote_branch` in that case. Errors (without mutating)
    /// if that rename would collide with an existing local jj bookmark.
    pub async fn attach(&self, name: &str, remote_branch: &str) -> AppResult<String> {
        if let Some(g) = self.git() {
            let _ = g
                .run_raw(&args(&[
                    "branch",
                    &format!("--set-upstream-to={REMOTE}/{remote_branch}"),
                    name,
                ]))
                .await?;
            Ok(name.to_string())
        } else if let Some(j) = self.jj() {
            let local = if remote_branch == name {
                name.to_string()
            } else {
                // The local work becomes that remote branch (jj has no
                // differently-named tracking, and this avoids a stray bookmark).
                // Refuse up front if a local bookmark already owns that name — jj
                // would reject the rename and leave the push half-done.
                if j.bookmarks(self.cwd())
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
                j.bookmark_rename(self.cwd(), name, remote_branch).await?;
                remote_branch.to_string()
            };
            let _ = j
                .run_raw(&args(&["bookmark", "track", &local, "--remote", REMOTE]))
                .await?;
            Ok(local)
        } else {
            unreachable!()
        }
    }

    /// Whether the working tree has uncommitted changes to tracked files (which
    /// would block a git rebase/merge integration). Always `false` for jj — a jj
    /// commit already moved unselected changes into the new working-copy change.
    pub async fn working_tree_dirty(&self) -> AppResult<bool> {
        if let Some(g) = self.git() {
            let out = g
                .run(&args(&["status", "--porcelain", "--untracked-files=no"]))
                .await?;
            Ok(!out.trim().is_empty())
        } else {
            Ok(false)
        }
    }

    /// Whether `name` is behind `origin/<remote_branch>` (the remote has commits the
    /// local branch lacks). Caller fetches first.
    pub async fn behind(&self, name: &str, remote_branch: &str) -> AppResult<bool> {
        if let Some(g) = self.git() {
            // Commits reachable from the remote branch but not the local one.
            match g
                .rev_list_count(self.cwd(), &git_behind_range(name, remote_branch))
                .await
            {
                Ok(n) => Ok(n > 0),
                Err(_) => Ok(false), // the remote ref isn't present locally → not behind
            }
        } else if let Some(j) = self.jj() {
            Ok(
                j.commit_count(self.cwd(), &jj_behind_revset(name, remote_branch))
                    .await?
                    > 0,
            )
        } else {
            unreachable!()
        }
    }

    /// Capture the current jj operation id so a failed integration can be rolled
    /// back with `op restore`. `None` for git (which uses `--abort` instead).
    pub async fn pre_integration_op(&self) -> AppResult<Option<String>> {
        if let Some(j) = self.jj() {
            Ok(Some(j.op_head(self.cwd()).await?))
        } else {
            Ok(None)
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
    ) -> AppResult<Integration> {
        if let Some(g) = self.git() {
            let target = format!("{REMOTE}/{remote_branch}");
            // Raw to preserve the reviewed, editor-safe behaviour (`--no-edit` /
            // `core.editor=true`); typed merge/rebase don't suppress the editor.
            let r = match strategy {
                PullStrategy::Merge => g.run_raw(&args(&["merge", "--no-edit", &target])).await?,
                PullStrategy::Rebase => g.run_raw(&git_rebase_args(&target)).await?,
            };
            if !r.is_success() {
                let files = self.git_conflicted_files().await?;
                if !files.is_empty() {
                    return Ok(Integration::Conflicts(files));
                }
                r.ensure_success()?; // a non-conflict failure: surface the real error
            }
            Ok(Integration::Clean)
        } else if let Some(j) = self.jj() {
            // jj rebase records conflicts in the commit rather than erroring.
            let dest = format!("{remote_branch}@{REMOTE}");
            j.run(&args(&["rebase", "-b", name, "-d", &dest])).await?;
            self.jj_integration_state(name).await
        } else {
            unreachable!()
        }
    }

    /// Re-check (and, for git, advance) an in-progress integration after the user
    /// resolved conflicts.
    pub async fn continue_integration(
        &self,
        name: &str,
        strategy: PullStrategy,
    ) -> AppResult<Integration> {
        if let Some(g) = self.git() {
            let files = self.git_conflicted_files().await?;
            if !files.is_empty() {
                return Ok(Integration::Conflicts(files));
            }
            match strategy {
                PullStrategy::Merge => {
                    if g.is_merge_in_progress(self.cwd()).await? {
                        g.merge_continue(self.cwd()).await?; // `commit --no-edit`
                    }
                    Ok(Integration::Clean)
                }
                PullStrategy::Rebase => {
                    // Advance one step (the check above confirmed no unmerged files),
                    // then re-surface state so the caller re-prompts — never spin.
                    let r = g.run_raw(&git_rebase_continue_args()).await?;
                    if r.is_success() && !g.is_rebase_in_progress(self.cwd()).await? {
                        Ok(Integration::Clean)
                    } else {
                        Ok(Integration::Conflicts(self.git_conflicted_files().await?))
                    }
                }
            }
        } else if let Some(j) = self.jj() {
            // The conflict lives in the bookmark commit; the user resolves it in the
            // working copy (its conflicted descendant). Fold that resolution into the
            // bookmark — jj's prescribed `jj squash` step — then re-check. A no-op
            // (nothing to squash) just leaves the bookmark conflicted.
            let _ = j
                .run_raw(&args(&[
                    "squash",
                    "--into",
                    name,
                    "--use-destination-message",
                ]))
                .await?;
            self.jj_integration_state(name).await
        } else {
            unreachable!()
        }
    }

    /// Roll back a failed/abandoned integration. Returns whether the rollback
    /// succeeded, so the caller doesn't falsely claim a clean abort.
    pub async fn abort_integration(
        &self,
        strategy: PullStrategy,
        jj_pre_op: Option<&str>,
    ) -> AppResult<bool> {
        if let Some(g) = self.git() {
            let res = match strategy {
                PullStrategy::Merge => g.merge_abort(self.cwd()).await,
                PullStrategy::Rebase => g.rebase_abort(self.cwd()).await,
            };
            Ok(res.is_ok())
        } else if let Some(j) = self.jj() {
            match jj_pre_op {
                Some(op) => Ok(j.op_restore(self.cwd(), op).await.is_ok()),
                None => Ok(true),
            }
        } else {
            unreachable!()
        }
    }

    /// Push `name` to `origin/<remote_branch>`, optionally setting upstream. Returns
    /// the raw result so the caller can report a rejection (e.g. non-fast-forward).
    pub async fn push(
        &self,
        name: &str,
        remote_branch: &str,
        set_upstream: bool,
    ) -> AppResult<ProcessResult<String>> {
        if let Some(g) = self.git() {
            let mut a = vec!["push".to_string()];
            if set_upstream {
                a.push("-u".into());
            }
            a.push(REMOTE.into());
            a.push(format!("{name}:{remote_branch}"));
            Ok(g.run_raw(&a).await?)
        } else if let Some(j) = self.jj() {
            // `name` is already the bookmark to push (attach renames it to match the
            // remote branch). jj auto-creates+tracks a new bookmark on push.
            let _ = (remote_branch, set_upstream);
            Ok(j.run_raw(&args(&["git", "push", "-b", name])).await?)
        } else {
            unreachable!()
        }
    }

    /// git-only: the repo-relative paths with unresolved (unmerged) conflicts.
    async fn git_conflicted_files(&self) -> AppResult<Vec<String>> {
        let Some(g) = self.git() else {
            return Ok(Vec::new());
        };
        let out = g
            .run(&args(&["diff", "--name-only", "--diff-filter=U"]))
            .await?;
        Ok(out
            .lines()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect())
    }

    /// jj-only: conflicted paths in the bookmark commit `name` (empty when clean).
    async fn jj_conflicted_files(&self, name: &str) -> AppResult<Vec<String>> {
        let Some(j) = self.jj() else {
            return Ok(Vec::new());
        };
        let r = j.run_raw(&args(&["resolve", "--list", "-r", name])).await?;
        // `resolve --list` exits non-zero when there are no conflicts.
        Ok(if r.is_success() {
            parse_jj_resolve_list(r.stdout())
        } else {
            Vec::new()
        })
    }

    /// jj-only: classify the bookmark commit as clean or conflicted.
    async fn jj_integration_state(&self, name: &str) -> AppResult<Integration> {
        let Some(j) = self.jj() else {
            return Ok(Integration::Clean);
        };
        if j.is_conflicted(self.cwd(), name).await? {
            Ok(Integration::Conflicts(
                self.jj_conflicted_files(name).await?,
            ))
        } else {
            Ok(Integration::Clean)
        }
    }
}

/// Build an owned-arg vector from string literals.
fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

/// `git -c core.editor=true rebase <target>` — the no-op editor keeps a later
/// `--continue` from opening one (typed `rebase` doesn't suppress it).
fn git_rebase_args(target: &str) -> Vec<String> {
    args(&["-c", "core.editor=true", "rebase", target])
}

/// `git -c core.editor=true rebase --continue`.
fn git_rebase_continue_args() -> Vec<String> {
    args(&["-c", "core.editor=true", "rebase", "--continue"])
}

/// git `rev-list --count` range whose count is how many commits `origin/<rb>` has
/// that local `<name>` lacks — i.e. the "behind" count (`A..B` = in B, not in A).
fn git_behind_range(name: &str, remote_branch: &str) -> String {
    format!("{name}..{REMOTE}/{remote_branch}")
}

/// jj revset of commits on `<rb>@origin` not in the ancestry of local `<name>` —
/// non-empty means the local bookmark is behind the remote.
fn jj_behind_revset(name: &str, remote_branch: &str) -> String {
    format!("{remote_branch}@{REMOTE} ~ ::{name}")
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

/// Conflicted paths from `jj resolve --list` (`<path>   <description>` per line).
fn parse_jj_resolve_list(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect()
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
    fn parses_git_upstream() {
        assert_eq!(parse_git_upstream("origin/main"), Some("main".into()));
        // A branch name containing slashes keeps everything after the remote.
        assert_eq!(parse_git_upstream("origin/feat/x"), Some("feat/x".into()));
        assert_eq!(parse_git_upstream("HEAD"), None);
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
    fn behind_range_and_revset() {
        // git: `name..origin/rb` counts commits the remote has that we lack.
        assert_eq!(git_behind_range("feat", "main"), "feat..origin/main");
        // jj: remote-only commits relative to the local bookmark's ancestry.
        assert_eq!(jj_behind_revset("feat", "main"), "main@origin ~ ::feat");
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
    fn parses_targets_multiple_names() {
        let out = "feat alt\u{1f}abc123\nmain\u{1f}def456\n";
        let ts = parse_jj_targets(out);
        assert_eq!(ts.len(), 3);
        assert_eq!(ts[0].label, "feat");
        assert_eq!(ts[0].revision.as_deref(), Some("abc123"));
        assert_eq!(ts[1].label, "alt");
        assert_eq!(ts[2].label, "main");
    }
}
