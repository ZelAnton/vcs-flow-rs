# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

## Project

`vcs-flow-rs` is a **Cargo workspace of small CLI binaries**, one per `crates/`
member. Each tool composes a useful *sequence* of version-control operations
(Git, jj, GitHub CLI) into a single command. The tools are built on the typed
clients [`vcs-git`](https://crates.io/crates/vcs-git) /
[`vcs-jj`](https://crates.io/crates/vcs-jj) /
[`vcs-github`](https://crates.io/crates/vcs-github), all of which drive their CLI
through the job-backed launcher [`processkit`](https://crates.io/crates/processkit)
so subprocess trees die with the tool. The client traits are `async`, so every
binary runs under `#[tokio::main]` and parses args with `clap`.

Key facts:

- **Binaries are self-contained.** There is no shared library crate (by design):
  each member has its own `main.rs`. If real duplication appears later, factor a
  `crates/<name>` lib out then — don't pre-abstract.
- **Crate name ≠ binary name when crates.io is taken.** `commit` is squatted on
  crates.io, so the first member's package is `vcs-flow-commit` with
  `[[bin]] name = "commit"` — it still installs/builds as `commit` (`commit.exe`).
  Reuse this pattern for any future tool whose ideal name is unavailable.
- **Shared version.** All crates inherit `version` from `[workspace.package]` and
  release together under one `v<version>` tag.
- **Distribution: crates.io + npm + standalone exe.** crates.io publishing is
  wired (`.github/workflows/release.yml`). The npm wrapper packages are a planned
  follow-up (model them on the sibling `agent-workspace` / `DevKit` `npm/*/bin`
  layout).
- **TUI tools use ratatui.** The `commit` binary is a full-screen TUI built on
  `ratatui` 0.29 + `tui-tree-widget` 0.23 + `tui-textarea` 0.7 (version-locked —
  see the `[workspace.dependencies]` comment) and `syntect` for diff highlighting.
  Its modules split cleanly: `repo`/`vcs` (backend + git/jj command sequences via
  the clients' `run` escape hatch), `tree` (path-compressed tree + tri-state
  selection — pure, unit-tested), `model`, `ai` (Copilot-CLI commit-message
  drafting via `processkit`), `settings` (per-user / per-repo AI model config,
  TOML), and `ui/*` (ratatui screens). The
  pure logic and command-arg builders are unit-tested; the live event loop needs
  a real terminal, so it's verified manually (`cargo run -p vcs-flow-commit`).

### Adding a new tool

1. `crates/<name>/` with its own `Cargo.toml` (inherit workspace fields with
   `field.workspace = true`), `src/main.rs`, `README.md`, `CHANGELOG.md` *(or note
   it's covered by the root changelog)*, and **its own `LICENSE`** (cargo packages
   only files inside the crate dir).
2. Add `"crates/<name>"` to the root `members` list.
3. If the crate.io name is taken, name the package `vcs-flow-<name>` and set
   `[[bin]] name = "<name>"`.
4. Reference workspace deps with `<dep>.workspace = true`.

## Build, test, run

```bash
cargo build                       # build the whole workspace
cargo build -p <crate>            # one crate
cargo run -p vcs-flow-commit               # run a tool (commit is interactive)
cargo test                        # all unit + integration tests
cargo test -p <crate>             # one crate's tests
cargo test -- --ignored           # the real-binary tests (need git/jj/gh installed)
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
cargo deny check advisories bans  # supply-chain scan (matches CI)
```

Integration tests live in each crate's `tests/` — each file is compiled as its
own crate. Tests that spawn a real `git`/`jj`/`gh` are `#[ignore]`d so the
default `cargo test` stays hermetic and green.

## Code style

- **Comment the *why*, not the *what*.** Explain the non-obvious reason — a wire
  contract, a workaround, a CLI quirk — not what the line plainly does.
- **Match the surrounding code.** Follow the existing module's naming, idioms,
  error handling, and comment density. New code should read like it was always there.
- **Reuse before you add.** Search for an existing helper before writing a new one.
- **Conventional-commit subjects** (`type(scope): summary` — `feat`, `fix`,
  `refactor`, `perf`, `docs`, `test`, `chore`, `ci`). These feed the changelog
  (`cliff.toml`); see "Releasing and the changelog".
- **Keep it formatted and lint-clean.** `cargo fmt` + `cargo clippy --all-targets`
  before considering work done.

## Dependency management

This workspace fixes **no** allow-list of crates — add whatever a tool genuinely
needs. The convention is about *how*:

- **Document every dependency.** Each entry in `[workspace.dependencies]` (or a
  crate's `Cargo.toml`) gets an inline comment explaining *why* it's there.
- **Declare shared deps once** in `[workspace.dependencies]`; reference them with
  `<dep>.workspace = true`. Pin major versions, enable only the features used.
- **Commit `Cargo.lock`.** It's tracked, not ignored.
- **Platform-specific deps** go under a cfg target table, e.g.
  `[target.'cfg(windows)'.dependencies]`, with the same "why" comment.

## Local-only files

`.gitignore` carves out `*.local.md`, `task_plan.md`, `findings.md`,
`progress.md` — use those names freely for scratch notes; they won't be committed.

## Releasing and the changelog

- **`[workspace.package]` `version` is the single source of truth** (all crates
  inherit it). Bump it with the release, tag `v<version>`, never let manifest,
  tag, and published artifacts drift.
- **`CHANGELOG.md` (one file for the whole workspace) follows
  [Keep a Changelog](https://keepachangelog.com/).** Curate `[Unreleased]` as you
  work. **Manual bullets always win.** If `[Unreleased]` is empty at release time,
  `git-cliff` (`cliff.toml`) auto-fills from commit subjects
  (`feat`→Added, `fix`→Fixed, `remove`→Removed, `perf`/`refactor`/`ci`→Changed,
  `docs`/`chore`/`test`→skipped).
- **`.github/workflows/release.yml`** is a manual (`workflow_dispatch`) Action:
  pick `patch`/`minor`/`major`, the next version is **computed** from the manifest
  (never typed), it auto-fills + promotes the changelog, then for **each
  publishable workspace member** runs a `cargo publish --dry-run` gate and
  publishes to crates.io, and finally tags `v<version>` + cuts a GitHub Release.
  Needs the `CRATES_IO_TOKEN` repo secret. Because the binaries are self-contained
  (no intra-workspace deps), members publish in any order.

## Supply chain and MSRV

- **`cargo deny`.** `deny.toml` configures cargo-deny; the CI job fails on RustSec
  advisories, yanked crates, and wildcard versions. Run
  `cargo deny check advisories bans` before adding/bumping a dependency.
- **MSRV.** `[workspace.package]` `rust-version` is the floor; the `msrv` CI job
  verifies it. Raise it and the job's pinned toolchain together.
  `rust-toolchain.toml` pins everyday builds to `stable` + rustfmt/clippy.

## Version control workflow

This repo uses [jujutsu (`jj`)](https://jj-vcs.github.io/jj/) colocated with git.
Use `jj`, not raw git. A `UserPromptSubmit` hook
(`.claude/hooks/jj-prompt-reminder.sh`) injects the per-prompt checklist each turn.

- **Per-prompt evaluation (mandatory).** Before edits, run `jj st` and classify
  the prompt against the current change description:

	| Signal in prompt | Category | Action |
	|---|---|---|
	| Same topic, refinement, follow-up of in-progress work | **Continuation** | Just work. jj folds edits into the current change. |
	| Same change but goal refined/expanded | **Scope shift** | `jj describe -m "<refined summary>"`. Don't start a new change. |
	| Orthogonal topic, "теперь сделай X" | **New work** | Finished → `jj new -m "<summary>"`; still in progress → `jj new @- -m "..."` (sibling). |

	Words like "теперь" / "now" / "next" / "also" usually mean new work or scope
	shift; imperative follow-ups in scope ("fix this", "продолжи") mean
	continuation. When in doubt, ask.

- **Describe early.** `jj describe -m "..."` when starting work; keep extending the
  same change for follow-ups rather than spawning one per edit.
- **Sync only on the user's explicit `pull`/`push`/`sync`:** `jj git fetch`;
  rebase if `main@origin` advanced (`jj rebase -r @- -d main@origin`);
  `jj bookmark set main -r <rev>`; `jj git push --bookmark main`. **Never push
  without an explicit signal.**
- **Undo via jj's safety net:** `jj undo` (repeatable), `jj abandon <rev>`,
  `jj restore`, `jj op log` + `jj op restore <op-id>`.
- **No new bookmarks** unless asked. Work lands on `main` (the publish target).

## Windows / line endings

`.gitattributes` (`* text=auto eol=lf`) keeps git and jj agreeing on the working
copy. A pre-attributes checkout may leave CRLF stat-cache state so colocated
`jj st` shows phantom modifications — the committed blobs are LF and pushed
commits are clean.
