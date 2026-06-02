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
- `commit`: model selection with persistence. When copilot reports the configured
  model is unavailable, the TUI prompts for another name and saves the working one
  back to the source it came from (the per-repo file, else the per-user file). The
  model resolves as `COMMIT_AI_MODEL` env var → per-repo `.vcs-flow-commit.toml`
  (kept out of version control via `.git/info/exclude`, or `.gitignore` for a
  worktree / pure-jj repo) → per-user config (`<config_dir>/vcs-flow/commit.toml`)
  → built-in default `gpt-5.4-mini`.

### Changed
- Raised MSRV to 1.88 (required by the `processkit` dependency).

### Fixed
-

[Unreleased]: https://github.com/ZelAnton/vcs-flow-rs/commits/HEAD
