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
- `commit` binary (`vcs-flow-commit`): create a commit from the working-copy change via `jj`.

### Changed
-

### Fixed
-

[Unreleased]: https://github.com/ZelAnton/vcs-flow-rs/commits/HEAD
