# Changelog

All notable changes to this workspace are documented in this file. Every crate
shares one version and releases together, so this is the single changelog for
all of them.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Add entries to `[Unreleased]` as you work — manual bullets always win over the
git-cliff auto-fill (config: `cliff.toml`). On release, promote `[Unreleased]`
to a dated version section.

## [Unreleased]

### Added
- Workspace skeleton (shared-version Cargo workspace, strict CI, cargo-deny, MSRV check).
- `commit` binary (`vcs-flow-commit`): interactive TUI to pick changed files
  (ignoring the index) as a path-compressed tree with tri-state selection,
  preview syntax-highlighted diffs, edit the message, optionally amend, and
  commit to the current git branch or the nearest jj bookmark.
- `commit`: AI-drafted commit messages via the GitHub Copilot CLI (`copilot`).
  After file selection the message editor is pre-filled with a draft generated
  from the selected diff (seeded by the existing jj change description); `Esc`
  skips it. Best-effort — falls back to the existing description if `copilot` is
  missing or fails.
- `commit`: after a (non-amend) commit, offers to push to `origin`. Resolves the
  remote branch (existing upstream → same-named remote → a filterable picker of
  remote branches, with `Ctrl+N` to push as a new same-named branch); fetches and, if
  the local branch is behind, integrates remote commits first — `merge` by default,
  `rebase` via the `pull` setting / `COMMIT_PULL_STRATEGY` (jj always rebases) —
  pausing for the user to resolve any conflicts before pushing.
- `commit`: post-push GitHub PR step (best-effort; GitHub remote + authenticated
  `gh` CLI required, silently skipped otherwise). After a successful push it lists
  the open PRs whose head is the pushed branch — title, base, and a clickable
  (OSC 8) URL — or, when there are none, offers to create one: the base branch
  defaults to the repo's default branch and can be re-picked from a filterable
  list; the title + markdown description are AI-drafted from the branch-vs-base
  diff and edited in the TUI editor (first line = title); the PR page then opens
  in the browser with both prefilled (`gh pr create --web`). While the question
  is pending, a diff-review mode shows the branch-vs-base changes as the familiar
  checkbox tree with per-file diffs, and can bulk-revert the marked files in the
  working copy — the undone patch is backed up to `%TEMP%/vcs-flow-commit/`
  first, and the pushed branch itself is never rewritten.
- `commit`: model selection with persistence. When copilot reports the configured
  model is unavailable, the TUI prompts for another name and saves the working one
  back to the source it came from (the per-repo file, else the per-user file). The
  model resolves as `COMMIT_AI_MODEL` env var → per-repo `.vcs-flow-commit.toml`
  (kept out of version control via `.git/info/exclude`, or `.gitignore` for a
  worktree / pure-jj repo) → per-user config (`<config_dir>/vcs-flow/commit.toml`)
  → built-in default `gpt-5.4-mini`.

### Changed
- Raised MSRV to 1.88 (required by the `processkit` dependency).
- Upgraded to `processkit` 0.6, `vcs-core` 0.2, and `vcs-git`/`vcs-jj`/`vcs-github`
  0.4, and adopted the new toolkit surface: `commit`'s `vcs` backend wraps
  `vcs_core::Repo` (detection via `vcs_core::detect`) and drives it through the
  cwd-bound `repo.git_at()`/`repo.jj_at()` typed views. The hand-rolled unified-diff
  parser is replaced by typed `diff()` → `FileDiff` (inheriting the toolkit's rename
  and forward-slash path fixes); the jj target/bookmark and git ls-remote/upstream
  text parsing by typed `reachable_bookmarks`/`bookmarks_all`/`remote_branches`/
  `upstream`/`resolve_list`; and the git merge/rebase integration by the
  editor-suppressed typed `merge_commit`/`rebase`/`rebase_continue` (dropping the
  `-c core.editor=true` workaround). Conflicts are still detected from the index
  (unmerged entries), and tracking/upstream calls stay best-effort. Behavior is
  unchanged for the user.

### Fixed
-

[Unreleased]: https://github.com/ZelAnton/vcs-flow-rs/commits/HEAD
