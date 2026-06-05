# commit

An interactive terminal UI for committing. Pick exactly which changed files go
in — ignoring git's staging area entirely — preview each file's
syntax-highlighted diff, write the message (optionally AI-drafted), and commit:
to **git** when the repo is git-only, or to **jj** when it's a jj or colocated
repo. After a fresh commit it can also push for you — and, on a GitHub repo,
show the branch's open pull requests or help you open a new one.

Part of [vcs-flow-rs](https://github.com/ZelAnton/vcs-flow-rs). The crate is
published as `vcs-flow-commit`; the binary it installs is `commit`.

```text
 Target: branch feature/login    selected 3/4
┌ Changes ───────────────────┐┌ src/auth/login.rs ─────────────┐
│   [~] src/                 ││ @@ -12,7 +12,9 @@ fn sign_in(   │
│     [~] auth/              ││  use crate::session;           │
│ »     [x] M login.rs       ││ +use crate::ratelimit;         │
│       [ ] M session.rs     ││                                │
│   [x] A README.md          ││ -fn sign_in(u: &User) {        │
│   [x] R new.rs             ││ +fn sign_in(u: &User, t: Tok) {│
└────────────────────────────┘└────────────────────────────────┘
 ↑↓ move  ←→ fold  Space toggle  +/- all/none  a amend  Ctrl+S commit  Esc cancel
```

## Contents

- [Why](#why)
- [Requirements](#requirements)
- [Install](#install)
- [Usage](#usage)
- [The interactive flow](#the-interactive-flow)
  - [1. Choose the target](#1-choose-the-target)
  - [2. Pick files & preview diffs](#2-pick-files--preview-diffs)
  - [3. Write the message](#3-write-the-message)
- [Keybindings](#keybindings)
- [AI commit messages](#ai-commit-messages)
- [Configuration](#configuration)
- [What "commit" means per backend](#what-commit-means-per-backend)
- [Pushing](#pushing)
- [GitHub pull requests](#github-pull-requests)
  - [Reviewing the branch diff (and reverting)](#reviewing-the-branch-diff-and-reverting)
- [Behavior & edge cases](#behavior--edge-cases)
- [How it's built](#how-its-built)
- [License](#license)

## Why

Committing a focused changeset usually means staging hunks by hand
(`git add -p`), squinting at `git diff --cached`, then writing a message — every
time. `commit` collapses that into one screen: a checkbox tree of changed files
with live diff previews, message editing, and an optional AI draft, working the
same way whether the repo is **git** or **jj**. There is no staging step — you
select files in the UI and `commit` records exactly those.

## Requirements

- A **git** or **jj** repository (run `commit` anywhere inside it; it operates
  from the repo root). `.jj` present → jj backend (including colocated git+jj);
  otherwise `.git` → git backend.
- The matching CLI on `PATH`: `git` and/or `jj`.
- *Optional* — the [GitHub Copilot CLI] (`copilot`) on `PATH` for AI-drafted
  messages. Without it, the editor opens on the existing message instead.
- *Optional* — an `origin` remote, for the post-commit push flow.
- *Optional* — the [GitHub CLI] (`gh`), authenticated (`gh auth login`), for the
  [post-push PR step](#github-pull-requests). Without it the step is skipped.

## Install

```bash
cargo install vcs-flow-commit       # from crates.io
cargo install --path crates/commit  # from a checkout
```

Both install a binary named `commit` (`commit.exe` on Windows).

## Usage

Run it inside a repository:

```bash
commit            # interactive commit
commit --amend    # start in amend mode (also toggle in-app with `a`)
commit -a         # short form of --amend
commit -C path    # operate on another repo directory
commit --help     # full flag list
```

`commit` is interactive: if stdin/stdout isn't a terminal it exits with
`commit is interactive; run it in a terminal`. If there are no changed tracked
files it prints `Nothing to commit` and exits 0.

## The interactive flow

### 1. Choose the target

Where the commit will land:

- **git** — the current branch (a detached `HEAD` is shown as
  `detached HEAD @ <short>`).
- **jj** — the nearest bookmark(s) reachable from `@`. If several are equally
  near, a picker opens (`↑`/`↓` to move, `Enter` to choose, `Esc` to cancel).
  If none are near, you can pick any bookmark or
  *"commit without moving a bookmark"* (describe-only).

When there's only one target the picker is skipped.

### 2. Pick files & preview diffs

A two-pane screen: the changed tracked files on the left, the highlighted diff of
the selected file on the right (a folder shows its children instead).

- The tree is **path-compressed** — deep single-child folders collapse, so
  `aa/bb/cc/deep.txt` shows as one `aa/bb/cc` node. It starts fully expanded with
  everything selected; uncheck what you don't want.
- **Checkboxes** are tri-state: `[x]` all (green), `[ ]` none (gray),
  `[~]` partial (yellow, folders only — toggling a folder cascades to its files).
- **File glyphs** mark the change kind: `A` added (green), `M` modified (yellow),
  `D` deleted (red), `R` renamed (cyan). A rename commits both the new path and
  the removal of the old one.
- The **header** shows the target, an `[AMEND]` badge when amend is on, and the
  `selected / total` count.

`Ctrl+Enter` / `Ctrl+S` confirms and opens the message editor. Confirming with
nothing selected is refused (no empty commits).

### 3. Write the message

A multi-line editor, pre-filled:

- On **amend**, with the target commit's current message.
- Otherwise, with an [AI-drafted message](#ai-commit-messages) (a "Generating…"
  screen shows while it runs; `Esc` skips to the fallback) — or, if Copilot is
  unavailable, with the change's current description (jj) or empty (git).

`Ctrl+Enter` / `Ctrl+S` commits; `Esc` cancels. An empty message (after trimming)
aborts with `empty commit message — nothing committed`.

## Keybindings

**File-selection screen**

| Key | Action |
|---|---|
| `↑` `↓` | Move the cursor |
| `←` `→` | Collapse / expand a folder |
| `Space` | Toggle the selected file/folder (folders cascade, tri-state) |
| `+` / `-` | Select all / none |
| `a` | Toggle amend |
| `PgUp` `PgDn` | Scroll the diff pane (±10 lines, clamped) |
| `Home` | Jump the diff pane back to the top |
| `Ctrl+Enter` or `Ctrl+S` | Confirm → message editor |
| `Esc` / `q` / `Ctrl+C` | Cancel (nothing committed) |

**Message editor**

| Key | Action |
|---|---|
| *(typing)* | Edit the multi-line message |
| `Ctrl+Enter` or `Ctrl+S` | Commit |
| `Esc` | Cancel |

**Pickers** (bookmark target / remote branch / PR base branch)

| Key | Action |
|---|---|
| `↑` `↓` | Move |
| *(typing)* | Filter the list (branch pickers only) |
| `Enter` | Choose the highlighted entry |
| `Ctrl+N` | Push as a new same-named branch (push-target picker only) |
| `Esc` | Cancel |

**Diff review** ([PR step](#reviewing-the-branch-diff-and-reverting))

| Key | Action |
|---|---|
| `↑` `↓` | Move the cursor |
| `←` `→` | Collapse / expand a folder |
| `Space` | Mark / unmark the selected file/folder (folders cascade, tri-state) |
| `+` / `-` | Mark all / none |
| `PgUp` `PgDn` | Scroll the diff pane (±10 lines, clamped) |
| `Home` | Jump the diff pane back to the top |
| `r` | Revert the marked files (asks `y/N` first) |
| `Esc` / `q` / `Ctrl+C` | Back to the create-PR question |

> `Ctrl+Enter` needs a terminal that reports it (most modern ones do); `Ctrl+S`
> is the universal fallback.

## AI commit messages

For a non-amend commit, `commit` drafts a message from the **selected** diff
using the [GitHub Copilot CLI]. It runs `copilot` non-interactively (no file or
tool access — pure text generation), seeded with the existing jj change
description as context, and asks for an imperative subject (≤72 chars) with an
optional body. The draft is cleaned of any `Co-authored-by:` trailer before it
lands in the editor, where you can edit it freely.

It's strictly **best-effort** — a commit is never blocked by the AI:

- A "Generating commit message…" spinner shows while Copilot runs; `Esc` skips it
  (and kills the subprocess).
- Generation is capped at 45 s; on timeout, failure, missing CLI, or empty output
  it silently falls back to the existing description (jj) / empty message (git).
- The diff sent is capped (~8 KB) so it stays well under the OS command-line
  limit; the seeded description is capped too.

If Copilot reports the configured **model** is unavailable, `commit` prompts for
another model name and retries. The working name is saved back to the source that
supplied the failing one (the per-repo file if a repo override was in effect,
otherwise your user config) so later runs use it.

## Configuration

Two settings, each resolved **highest-precedence first**; a blank/unrecognized
value falls through to the next source.

| Setting | Env var | TOML key | Default | Notes |
|---|---|---|---|---|
| AI model | `COMMIT_AI_MODEL` | `model` | `gpt-5.4-mini` | Passed to `copilot --model=…` |
| Pull strategy | `COMMIT_PULL_STRATEGY` | `pull` | `merge` | `merge` or `rebase`; governs **git** integration (jj always rebases) |

**Resolution order** (first match wins):

1. the environment variable;
2. a **per-repo** override file `.vcs-flow-commit.toml` in the repo root;
3. the **per-user** config file;
4. the built-in default.

The per-user file lives at the platform config dir:

- Windows: `%APPDATA%\vcs-flow\commit.toml`
- Linux: `~/.config/vcs-flow/commit.toml`
- macOS: `~/Library/Application Support/vcs-flow/commit.toml`

Example file (either location):

```toml
model = "gpt-5.4"
pull  = "rebase"
```

The per-repo `.vcs-flow-commit.toml` is **kept out of version control
automatically** so it's never committed or pushed: a `.git/info/exclude` entry
for a colocated git repo, or a `.gitignore` entry for a worktree / pure-jj repo.

## What "commit" means per backend

- **git** — commits exactly the selected paths' working-tree content to the
  current branch (`git commit --only <paths>`), regardless of what's staged.
  `--amend` amends the branch tip.
- **jj** — finalizes a commit containing the selected paths and advances the
  nearest bookmark onto it; deselected changes stay in the working copy. If
  several bookmarks are equally near, you pick one first. **Amend** squashes the
  selected paths into the nearest bookmark's existing commit, keeping that
  commit's description (so jj never opens an editor to merge two messages).

## Pushing

After a (non-amend) commit, `commit` offers to push to `origin`. On agreement it:

1. **Resolves the remote branch.** If your branch/bookmark already tracks a remote
   branch, it uses that. Otherwise it looks for a **same-named** remote branch and
   tracks it. If there's no match, a **filterable picker** of existing remote
   branches opens — type to narrow, `Enter` attaches to the highlighted branch,
   `Ctrl+N` pushes as a new same-named branch, `Esc` cancels.
2. **Pulls if behind.** It fetches `origin` and, if the local branch is behind the
   remote branch, integrates the remote commits first. The strategy is **merge**
   by default; set `pull = "rebase"` (or `COMMIT_PULL_STRATEGY=rebase`) to rebase
   instead. (The setting governs git; jj always rebases the bookmark onto the
   remote.) On a **git** repo with uncommitted changes to tracked files it stops
   instead of risking a half-integration. If integration **conflicts**, `commit`
   lists the conflicted files and waits — resolve them in your own editor, press
   `Enter` to re-check and continue, or `a` to abort and roll back.
3. **Pushes**, setting upstream when the branch was untracked. On a GitHub repo
   the [PR step](#github-pull-requests) follows.

Amended commits are **not** auto-pushed (rewriting an already-pushed tip needs a
manual force push, e.g. `git push --force-with-lease`).

## GitHub pull requests

After a **successful push**, `commit` closes the loop with GitHub. The step is
strictly **best-effort** — it never affects the push result — and runs only when
`origin` points at `github.com` and the authenticated [GitHub CLI] (`gh`) is on
`PATH`; otherwise it's skipped (with at most one dim notice).

**If the pushed branch already has open PRs**, they're listed — number, title,
base branch, and a *clickable* URL (an OSC 8 hyperlink; Windows Terminal and
most modern emulators make it a real link, others still show the address):

```text
Open pull request for 'feature/login':
  #42 Add rate-limited sign-in  → main
      https://github.com/you/repo/pull/42
```

**If there are none**, `commit` offers to create one:

```text
No open pull request for 'feature/login'.
Create a pull request 'feature/login' → 'main'?  [Y]es / [n]o / [b]ase / [d]iff:
```

- The **base** defaults to the repository's default branch. `b` opens the
  filterable branch picker to target another existing branch on `origin`.
- `d` opens the [diff review](#reviewing-the-branch-diff-and-reverting) of the
  branch against the current base, then re-asks.
- `Y`/`Enter` proceeds: the PR **title + markdown description** are
  [AI-drafted](#ai-commit-messages) from the branch-vs-base diff (same Copilot
  machinery, spinner, `Esc` to skip, model retry); you edit the result in the
  multi-line editor — **first line = title**, the rest (after a blank line) =
  description. Confirming opens the GitHub **PR-creation page in your browser**
  with both prefilled (`gh pr create --web`), so nothing is published until you
  press the button there. An empty title aborts.

### Reviewing the branch diff (and reverting)

The `d` review mode shows what the PR would contain — the diff of
`origin/<branch>` against `origin/<base>` (merge-base, exactly GitHub's view) —
in the familiar two-pane screen: the path-compressed checkbox tree with `A`/`M`/
`D`/`R` glyphs on the left, the selected file's highlighted diff (or a folder's
children) on the right.

Marks start **cleared** here: check files or folders you want to *undo*, then
press `r` and confirm. `commit` then:

1. **Backs up first** — the exact patch being undone is written to
   `<temp>/vcs-flow-commit/revert-<stamp>.patch` (e.g. `%TEMP%\vcs-flow-commit\`
   on Windows). If you reverted by mistake, re-apply it from the repo root with
   `git apply <file>`.
2. **Reverts in the working copy only** — the marked files' content returns to
   the base-branch (merge-base) state: files the branch added are deleted, ones
   it deleted come back, edits and renames are undone. The pushed branch itself
   is **never rewritten**; commit and push the revert (with `commit`, naturally)
   to update the branch and its future PR.

Reverted files disappear from the review list for the session, and on the way
out the step prints a reminder with the backup path(s).

## Behavior & edge cases

- **No staging area.** git selection ignores the index entirely; only the files
  you check are recorded.
- **Untracked files** aren't shown for git (the diff is against `HEAD`); `git add`
  them first if you want them in. jj tracks new files automatically, so they
  appear.
- **Empty / unborn repo.** With no changed tracked files, `commit` prints
  `Nothing to commit` and exits without entering the UI.
- **Renames** show as a single `R old → new` entry and commit both sides.
- **Non-interactive** invocations (piped stdin/stdout) are refused up front.
- **Exit codes:** `0` on a successful commit, a clean cancel, or "nothing to
  commit"; non-zero only on an actual error.

## How it's built

`commit` composes the [vcs-flow-rs](https://github.com/ZelAnton/vcs-flow-rs)
toolkit: the [`vcs-core`](https://crates.io/crates/vcs-core) facade for
git/jj detection and dispatch, the typed
[`vcs-git`](https://crates.io/crates/vcs-git) /
[`vcs-jj`](https://crates.io/crates/vcs-jj) clients for the backend operations,
the [`vcs-github`](https://crates.io/crates/vcs-github) client driving `gh` for
the PR step, and the job-backed launcher
[`processkit`](https://crates.io/crates/processkit) (so every
`git`/`jj`/`gh`/`copilot` subprocess tree dies with the tool). The UI is built
on [`ratatui`](https://crates.io/crates/ratatui) with `syntect` for diff
highlighting.

[GitHub Copilot CLI]: https://github.com/github/copilot-cli
[GitHub CLI]: https://cli.github.com/

## License

MIT — see [LICENSE](LICENSE).
