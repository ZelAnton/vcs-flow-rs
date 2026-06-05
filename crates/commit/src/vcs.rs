//! Backend operations over a [`vcs_core::Repo`] handle.
//!
//! `Repo` (from the `vcs-core` facade) detects git/jj and dispatches the common
//! surface (`current_branch`, `fetch`, …). For everything it doesn't model we use
//! the cwd-bound typed views `repo.git_at()` / `repo.jj_at()` (`GitAt` / `JjAt`),
//! which expose the `vcs_git::Git` / `vcs_jj::Jj` operations without threading a
//! `dir` argument — they're bound to `repo.cwd()`. Most operations are typed
//! (`diff`, `commit_paths`, `merge_commit`, `reachable_bookmarks`, `resolve_list`,
//! `bookmarks_all`, …); a handful with no typed equivalent (the explicit `fetch
//! origin`, the amend-time jj `squash` with filesets, and the refspec push whose
//! `ProcessResult` we report on) still go through the views' `run` / `run_raw`
//! escape hatch. `repo.cwd()` is bound to the repo root by [`Backend::open`]; `main`
//! also sets the process cwd to the root for those raw runs.
//!
//! The post-push PR step adds git-only branch-vs-base operations (review diff,
//! revert). For a jj repo the facade has no git client, so a colocated `.git`
//! gets its own standalone `vcs_git::Git` bound at the root (`review_git`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use processkit::ProcessResult;
use vcs_git::DiffSpec as GitDiff;
use vcs_jj::{DiffSpec as JjDiff, JjFileset};

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
    /// Standalone git client for the colocated `.git` of a jj repo: for a
    /// jj-kind `Repo` the facade's `git_at()` is `None` even when colocated,
    /// but the branch-vs-base PR review/revert are git-only operations. `None`
    /// for git repos (the facade's own client serves) and pure-jj repos.
    colo_git: Option<vcs_git::Git>,
}

impl Backend {
    /// Detect the repo at or above `start` and bind the handle to its root.
    pub fn open(start: &Path) -> AppResult<Self> {
        let repo = vcs_core::Repo::open(start).map_err(|_| "not inside a git or jj repository")?;
        // Bind cwd to the root so the typed views and the root-relative paths we
        // pass them agree (`open` binds cwd to `start`, which may be a subdir).
        let root = repo.root().to_path_buf();
        let repo = repo.at(root);
        // `.git` may be a directory or a worktree's gitdir pointer file.
        let colo_git = (matches!(repo.kind(), vcs_core::BackendKind::Jj)
            && repo.root().join(".git").exists())
        .then(vcs_git::Git::new);
        Ok(Backend { repo, colo_git })
    }

    pub fn root(&self) -> &Path {
        self.repo.root()
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
        if let Some(g) = self.repo.git_at() {
            // Typed `diff(WorkingTree)` is `diff HEAD … -M`, parsed per file. It
            // diffs the empty tree on an unborn repo, so no special-case is needed.
            Ok(snapshot_from_git(g.diff(GitDiff::WorkingTree).await?))
        } else if let Some(j) = self.repo.jj_at() {
            // jj `diff(WorkingTree)` is `diff -r @ --git`, parsed per file.
            Ok(snapshot_from_jj(j.diff(JjDiff::WorkingTree).await?))
        } else {
            unreachable!()
        }
    }

    /// Where the commit can land. git: the current branch (one). jj: the nearest
    /// bookmarks reachable from `@`; if none, every bookmark plus a describe-only
    /// option (empty label).
    pub async fn targets(&self) -> AppResult<Vec<Target>> {
        if let Some(g) = self.repo.git_at() {
            // Facade `current_branch` returns `None` when detached; surface that
            // explicitly rather than claiming a branch named "HEAD".
            let label = match self.repo.current_branch().await? {
                Some(b) => b,
                None => {
                    let short = g.rev_parse_short("HEAD").await.unwrap_or_default();
                    format!("detached HEAD @ {}", short.trim())
                }
            };
            Ok(vec![Target {
                label,
                revision: None,
            }])
        } else if let Some(j) = self.repo.jj_at() {
            // A commit carrying several bookmarks yields one target per name.
            let mut targets: Vec<Target> = j
                .reachable_bookmarks()
                .await?
                .into_iter()
                .map(|b| Target {
                    label: b.name,
                    revision: Some(b.target),
                })
                .collect();
            if targets.is_empty() {
                for b in j.bookmarks().await? {
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
        if let Some(g) = self.repo.git_at() {
            if amend {
                Ok(g.last_commit_message().await?.trim_end().to_string())
            } else {
                Ok(String::new())
            }
        } else if let Some(j) = self.repo.jj_at() {
            let revset = if amend && !target.label.is_empty() {
                target.label.as_str()
            } else {
                "@"
            };
            // No typed equivalent for "read a revision's description"; a raw
            // template read is the simplest exact answer.
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
        if let Some(g) = self.repo.git_at() {
            let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
            g.commit_paths(&paths, message, amend).await?;
        } else if let Some(j) = self.repo.jj_at() {
            let filesets: Vec<JjFileset> = paths.iter().map(JjFileset::path).collect();
            // Amend needs an existing bookmark commit to fold into; the describe-only
            // target (no bookmark) falls back to a normal commit.
            if amend && !target.label.is_empty() {
                // `--use-destination-message` keeps the bookmark commit's own
                // description so jj never opens an editor to *combine* it with `@`'s
                // (which `squash_paths`/`jj squash` does when both are described, and
                // would hang on the now-restored normal terminal). We set the final
                // message explicitly below. Raw because the typed `squash_into` takes
                // neither `--from @` nor filesets.
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
                j.commit_paths(&filesets, message).await?;
                // Advance the chosen bookmark onto the finalised commit (`@-`).
                if !target.label.is_empty() {
                    j.bookmark_set(&target.label, "@-").await?;
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
        if self.repo.git_at().is_some() {
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
        if let Some(g) = self.repo.git_at() {
            // `upstream()` is `Some("origin/<branch>")` or `None`; strip the remote.
            Ok(g.upstream().await?.as_deref().and_then(parse_git_upstream))
        } else if let Some(j) = self.repo.jj_at() {
            // Tracked iff a remote-tracking entry on `origin` carries this name.
            let tracked = j
                .bookmarks_all()
                .await?
                .into_iter()
                .any(|b| b.name == name && b.remote.as_deref() == Some(REMOTE) && b.tracked);
            Ok(tracked.then(|| name.to_string()))
        } else {
            unreachable!()
        }
    }

    /// Fetch from `origin` so remote-tracking refs/bookmarks are current.
    pub async fn fetch(&self) -> AppResult<()> {
        if let Some(g) = self.repo.git_at() {
            // Explicitly `origin`: the push flow reasons about `origin/<rb>`. The
            // facade `repo.fetch()` / typed `g.fetch()` is bare `git fetch`, which
            // would fetch the branch's *configured* remote (not necessarily origin).
            g.run(&args(&["fetch", REMOTE])).await?;
        } else {
            // jj: `repo.fetch()` is `jj git fetch` — already the right thing.
            self.repo.fetch().await?;
        }
        Ok(())
    }

    /// Existing branch names on `origin`.
    pub async fn remote_branches(&self) -> AppResult<Vec<String>> {
        if let Some(g) = self.repo.git_at() {
            Ok(g.remote_branches(REMOTE).await?)
        } else if let Some(j) = self.repo.jj_at() {
            let mut names: Vec<String> = j
                .bookmarks_all()
                .await?
                .into_iter()
                .filter(|b| b.remote.as_deref() == Some(REMOTE))
                .map(|b| b.name)
                .collect();
            names.sort();
            names.dedup();
            Ok(names)
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
        if let Some(g) = self.repo.git_at() {
            // Best-effort: the push runs with `-u` and sets the upstream anyway, so a
            // set-upstream hiccup here must not abort an otherwise-valid push.
            let _ = g
                .set_upstream(name, &format!("{REMOTE}/{remote_branch}"))
                .await;
            Ok(name.to_string())
        } else if let Some(j) = self.repo.jj_at() {
            let local = if remote_branch == name {
                name.to_string()
            } else {
                // The local work becomes that remote branch (jj has no
                // differently-named tracking, and this avoids a stray bookmark).
                // Refuse up front if a local bookmark already owns that name — jj
                // would reject the rename and leave the push half-done.
                if j.bookmarks().await?.iter().any(|b| b.name == remote_branch) {
                    return Err(format!(
                        "a local bookmark '{remote_branch}' already exists; rename or \
                         delete it, or push '{name}' as its own branch (Ctrl+N)"
                    )
                    .into());
                }
                j.bookmark_rename(name, remote_branch).await?;
                remote_branch.to_string()
            };
            // Best-effort: `jj git push` auto-creates and tracks the bookmark, so a
            // tracking hiccup here must not abort the push.
            let _ = j.bookmark_track(&local, REMOTE).await;
            Ok(local)
        } else {
            unreachable!()
        }
    }

    /// Whether the working tree has uncommitted changes to tracked files (which
    /// would block a git rebase/merge integration). Always `false` for jj — a jj
    /// commit already moved unselected changes into the new working-copy change.
    pub async fn working_tree_dirty(&self) -> AppResult<bool> {
        if let Some(g) = self.repo.git_at() {
            // `--untracked-files=no`: untracked files don't block a rebase/merge, so
            // they mustn't count as "dirty" here (no typed predicate excludes them).
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
        if let Some(g) = self.repo.git_at() {
            // Commits reachable from the remote branch but not the local one.
            match g
                .rev_list_count(&git_behind_range(name, remote_branch))
                .await
            {
                Ok(n) => Ok(n > 0),
                Err(_) => Ok(false), // the remote ref isn't present locally → not behind
            }
        } else if let Some(j) = self.repo.jj_at() {
            Ok(j.commit_count(&jj_behind_revset(name, remote_branch))
                .await?
                > 0)
        } else {
            unreachable!()
        }
    }

    /// Capture the current jj operation id so a failed integration can be rolled
    /// back with `op restore`. `None` for git (which uses `--abort` instead).
    pub async fn pre_integration_op(&self) -> AppResult<Option<String>> {
        if let Some(j) = self.repo.jj_at() {
            Ok(Some(j.op_head().await?))
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
        if let Some(g) = self.repo.git_at() {
            let target = format!("{REMOTE}/{remote_branch}");
            // Typed merge/rebase suppress the editor (GIT_EDITOR/GIT_SEQUENCE_EDITOR),
            // so a later `--continue` can't hang headless. `merge_commit(_, false, _)`
            // allows a fast-forward and uses `--no-edit`.
            let res = match strategy {
                PullStrategy::Merge => g.merge_commit(&target, false, None).await,
                PullStrategy::Rebase => g.rebase(&target).await,
            };
            if let Err(e) = res {
                // A conflict leaves unmerged entries in the index — the ground truth,
                // regardless of how git worded (or localised) the failure. No unmerged
                // files → a genuine error, so surface it.
                let files = self.git_conflicted_files().await?;
                if files.is_empty() {
                    return Err(e.into());
                }
                return Ok(Integration::Conflicts(files));
            }
            Ok(Integration::Clean)
        } else if let Some(j) = self.repo.jj_at() {
            // jj rebase records conflicts in the commit rather than erroring.
            let dest = format!("{remote_branch}@{REMOTE}");
            j.rebase_branch(name, &dest).await?;
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
        if let Some(g) = self.repo.git_at() {
            let files = self.git_conflicted_files().await?;
            if !files.is_empty() {
                return Ok(Integration::Conflicts(files));
            }
            match strategy {
                PullStrategy::Merge => {
                    if g.is_merge_in_progress().await? {
                        g.merge_continue().await?; // `commit --no-edit`
                    }
                    Ok(Integration::Clean)
                }
                PullStrategy::Rebase => {
                    // Advance one step (the check above confirmed no unmerged files),
                    // then re-surface state so the caller re-prompts — never spin.
                    // `rebase_continue` suppresses the editor.
                    let advanced = g.rebase_continue().await.is_ok();
                    if advanced && !g.is_rebase_in_progress().await? {
                        Ok(Integration::Clean)
                    } else {
                        Ok(Integration::Conflicts(self.git_conflicted_files().await?))
                    }
                }
            }
        } else if let Some(j) = self.repo.jj_at() {
            // The conflict lives in the bookmark commit; the user resolves it in the
            // working copy (its conflicted descendant). Fold that resolution into the
            // bookmark — jj's prescribed `jj squash` step — then re-check. A no-op
            // (nothing to squash) is ignored; we re-classify the bookmark afterwards.
            let _ = j.squash_into(name, true).await;
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
        if let Some(g) = self.repo.git_at() {
            let res = match strategy {
                PullStrategy::Merge => g.merge_abort().await,
                PullStrategy::Rebase => g.rebase_abort().await,
            };
            Ok(res.is_ok())
        } else if let Some(j) = self.repo.jj_at() {
            match jj_pre_op {
                Some(op) => Ok(j.op_restore(op).await.is_ok()),
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
        if let Some(g) = self.repo.git_at() {
            let mut a = vec!["push".to_string()];
            if set_upstream {
                a.push("-u".into());
            }
            a.push(REMOTE.into());
            a.push(format!("{name}:{remote_branch}"));
            Ok(g.run_raw(&a).await?)
        } else if let Some(j) = self.repo.jj_at() {
            // `name` is already the bookmark to push (attach renames it to match the
            // remote branch). jj auto-creates+tracks a new bookmark on push.
            let _ = (remote_branch, set_upstream);
            Ok(j.run_raw(&args(&["git", "push", "-b", name])).await?)
        } else {
            unreachable!()
        }
    }

    // ----- branch-vs-base review (post-push PR step) -------------------------

    /// Git view for the branch-vs-base (PR) review: the repo's own client for a
    /// git repo, or the standalone colocated client for a jj repo. `None` means
    /// pure jj (no `.git`) — where the GitHub PR step doesn't apply either,
    /// because `gh` needs a git directory too.
    fn review_git(&self) -> Option<vcs_git::GitAt<'_>> {
        self.repo
            .git_at()
            .or_else(|| self.colo_git.as_ref().map(|g| g.at(self.repo.root())))
    }

    /// The URL of `origin`, used to recognise a GitHub remote. `None` when the
    /// remote (or a git working copy) is missing.
    pub async fn remote_url(&self) -> Option<String> {
        match self.review_git() {
            Some(g) => g.remote_url(REMOTE).await.ok(),
            None => None,
        }
    }

    /// The changed files + diffs of `origin/<head>` against `origin/<base>`
    /// (merge-base), i.e. exactly what a PR from `head` into `base` would show.
    pub async fn review_snapshot(&self, base: &str, head: &str) -> AppResult<Snapshot> {
        let g = self
            .review_git()
            .ok_or("branch review needs a git working copy")?;
        Ok(snapshot_from_git(
            g.diff(GitDiff::Rev(review_spec(base, head))).await?,
        ))
    }

    /// The same branch-vs-base diff as one raw text block, for AI drafting.
    pub async fn review_diff_text(&self, base: &str, head: &str) -> AppResult<String> {
        let g = self
            .review_git()
            .ok_or("branch review needs a git working copy")?;
        Ok(g.diff_text(GitDiff::Rev(review_spec(base, head))).await?)
    }

    /// Restore the working-copy content of `paths` to the `origin/<base>` side of
    /// the branch-vs-base diff — no commit is made. The combined patch being
    /// undone is written to a backup file in the temp dir *before* anything is
    /// touched; reverse-applying it (`apply -R`) then deletes files the branch
    /// added, recreates ones it deleted, and undoes edits/renames. Returns the
    /// backup path (re-apply it with `git apply <file>` to take the revert back).
    pub async fn revert_paths(
        &self,
        base: &str,
        head: &str,
        paths: &[String],
    ) -> AppResult<PathBuf> {
        let g = self.review_git().ok_or("revert needs a git working copy")?;
        let dir = std::env::temp_dir().join("vcs-flow-commit");
        std::fs::create_dir_all(&dir)?;
        let backup = dir.join(format!("revert-{}.patch", backup_stamp()));
        // A bespoke diff rather than the typed `diff()` raw blocks, written by
        // git itself via `--output=<file>` so the patch bytes never round-trip
        // through our process (the line-based String capture lossy-decodes
        // non-UTF-8 bytes and rejoins CRLF as LF — breaking `apply` on entirely
        // normal Windows files). `--binary` makes changed binaries revertible
        // too (their parsed `raw` is only the "Binary files differ" stub, which
        // `apply` rejects — and `apply` is atomic, so one binary would fail the
        // whole revert). `--no-textconv` keeps configured textconv drivers from
        // producing a non-applicable patch. `-M` keeps renames as renames; the
        // pathspec limits the patch to the marked files (both sides of a rename
        // are passed, so the pair stays detectable). Runs in the process cwd —
        // `main` sets it to the repo root.
        let mut diff_args = args(&[
            "diff",
            &review_spec(base, head),
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "-M",
            &format!("--output={}", backup.display()),
            "--",
        ]);
        diff_args.extend(paths.iter().cloned());
        if let Err(e) = g.run(&diff_args).await {
            // git may have created/truncated the file before failing — don't
            // leave a stale, never-applied patch behind.
            let _ = std::fs::remove_file(&backup);
            return Err(e.into());
        }
        // With `--output` the patch lands in the file; stdout is empty.
        if std::fs::metadata(&backup).map(|m| m.len()).unwrap_or(0) == 0 {
            let _ = std::fs::remove_file(&backup);
            return Err("nothing to revert for the selected paths".into());
        }
        g.run(&args(&["apply", "-R", &backup.to_string_lossy()]))
            .await?;
        Ok(backup)
    }

    /// git-only: the repo-relative paths with unresolved (unmerged) conflicts.
    async fn git_conflicted_files(&self) -> AppResult<Vec<String>> {
        let Some(g) = self.repo.git_at() else {
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
        let Some(j) = self.repo.jj_at() else {
            return Ok(Vec::new());
        };
        Ok(j.resolve_list(name).await?)
    }

    /// jj-only: classify the bookmark commit as clean or conflicted.
    async fn jj_integration_state(&self, name: &str) -> AppResult<Integration> {
        let Some(j) = self.repo.jj_at() else {
            return Ok(Integration::Clean);
        };
        if j.is_conflicted(name).await? {
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

/// Three-dot range `origin/<base>...origin/<head>`: the merge-base diff git (and
/// GitHub) shows for a PR. Both sides are remote-tracking refs — fresh after the
/// push — so local HEAD and working-copy state don't affect the result.
fn review_spec(base: &str, head: &str) -> String {
    format!("{REMOTE}/{base}...{REMOTE}/{head}")
}

/// Backup-file stamp: epoch milliseconds plus a per-process counter, so two
/// reverts landing in the same millisecond can't overwrite each other's backup.
fn backup_stamp() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}-{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// Strip the remote prefix from a `git upstream()` value (`origin/feat/x` → `feat/x`).
/// `None` if there's no `/` (not a remote-qualified ref).
fn parse_git_upstream(upstream: &str) -> Option<String> {
    let s = upstream.trim();
    s.split_once('/').map(|(_, branch)| branch.to_string())
}

/// Map a [`vcs_git::FileDiff`] list into a [`Snapshot`]: the file change list and
/// the per-file raw diff text both come from the typed parse, so the tree path and
/// the diff key always agree. Paths are already forward-slash normalised, and a
/// rename carries the new path with the original on `old_path`.
fn snapshot_from_git(files: Vec<vcs_git::FileDiff>) -> Snapshot {
    let mut changes = Vec::with_capacity(files.len());
    let mut diffs = HashMap::new();
    for f in files {
        changes.push(FileChange {
            path: f.path.clone(),
            old_path: f.old_path,
            kind: git_kind(f.change),
        });
        diffs.insert(f.path, f.raw);
    }
    Snapshot { changes, diffs }
}

/// jj counterpart of [`snapshot_from_git`] — the two `FileDiff` types are
/// structurally identical but distinct per crate.
fn snapshot_from_jj(files: Vec<vcs_jj::FileDiff>) -> Snapshot {
    let mut changes = Vec::with_capacity(files.len());
    let mut diffs = HashMap::new();
    for f in files {
        changes.push(FileChange {
            path: f.path.clone(),
            old_path: f.old_path,
            kind: jj_kind(f.change),
        });
        diffs.insert(f.path, f.raw);
    }
    Snapshot { changes, diffs }
}

/// Map the toolkit git `ChangeKind` onto the local model kind. (`#[non_exhaustive]`,
/// so an unknown future variant falls back to `Modified`.)
fn git_kind(k: vcs_git::ChangeKind) -> ChangeKind {
    match k {
        vcs_git::ChangeKind::Added => ChangeKind::Added,
        vcs_git::ChangeKind::Deleted => ChangeKind::Deleted,
        vcs_git::ChangeKind::Renamed => ChangeKind::Renamed,
        _ => ChangeKind::Modified,
    }
}

/// jj counterpart of [`git_kind`].
fn jj_kind(k: vcs_jj::ChangeKind) -> ChangeKind {
    match k {
        vcs_jj::ChangeKind::Added => ChangeKind::Added,
        vcs_jj::ChangeKind::Deleted => ChangeKind::Deleted,
        vcs_jj::ChangeKind::Renamed => ChangeKind::Renamed,
        _ => ChangeKind::Modified,
    }
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
    fn behind_range_and_revset() {
        // git: `name..origin/rb` counts commits the remote has that we lack.
        assert_eq!(git_behind_range("feat", "main"), "feat..origin/main");
        // jj: remote-only commits relative to the local bookmark's ancestry.
        assert_eq!(jj_behind_revset("feat", "main"), "main@origin ~ ::feat");
    }

    #[test]
    fn review_spec_is_three_dot_remote_range() {
        // Merge-base diff between the remote-tracking refs — the PR view.
        assert_eq!(review_spec("main", "feat/x"), "origin/main...origin/feat/x");
    }
}
