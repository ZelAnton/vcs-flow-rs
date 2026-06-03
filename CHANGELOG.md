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
- `commit`: model selection with persistence. When copilot reports the configured
  model is unavailable, the TUI prompts for another name and saves the working one
  back to the source it came from (the per-repo file, else the per-user file). The
  model resolves as `COMMIT_AI_MODEL` env var → per-repo `.vcs-flow-commit.toml`
  (kept out of version control via `.git/info/exclude`, or `.gitignore` for a
  worktree / pure-jj repo) → per-user config (`<config_dir>/vcs-flow/commit.toml`)
  → built-in default `gpt-5.4-mini`.

### Changed
- Raised MSRV to 1.88 (required by the `processkit` dependency).
- Upgraded to `processkit` 0.5 and `vcs-git`/`vcs-jj` 0.3, and adopted the new
  [`vcs-core`](https://crates.io/crates/vcs-core) facade: `commit`'s `vcs` backend
  now wraps `vcs_core::Repo` (detection via `vcs_core::detect`, dispatch + escape
  hatches) and uses the typed 0.3 client methods (`commit_paths`, `diff_text`,
  `rev_list_count`/`commit_count`, `merge_continue`, `is_merge_in_progress`/
  `is_rebase_in_progress`, `op_head`/`op_restore`, `bookmark_set`/`bookmark_rename`,
  …) in place of hand-built command strings. Behavior is unchanged; the
  conflict-sensitive merge/rebase steps deliberately stay raw for editor safety.

### Fixed
-

[Unreleased]: https://github.com/ZelAnton/vcs-flow-rs/commits/HEAD
