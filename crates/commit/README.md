# commit

An interactive terminal UI for committing. Pick exactly which changed files go
in (ignoring git's staging area), preview each file's syntax-highlighted diff,
write the message, and commit — to **git** when the repo is git-only, or to
**jj** when it's a jj or colocated repo.

Part of [vcs-flow-rs](https://github.com/ZelAnton/vcs-flow-rs). The crate is
published as `vcs-flow-commit`; the binary it installs is `commit`.

## Install

```bash
cargo install vcs-flow-commit       # from crates.io
cargo install --path crates/commit  # from a checkout
```

## Usage

Run it inside a repository:

```bash
commit            # interactive commit
commit --amend    # start in amend mode (also toggle with `a`)
commit -C path    # operate on another repo directory
```

You get a file tree of the changed tracked files, path-compressed so deep
single-child folders collapse (`aa/bb/cc` shown as one node). The right pane
shows the selected file's diff, or a folder's contents. Everything starts
selected; uncheck what you don't want. The header shows where the commit will
land (git branch / jj bookmark).

### Keys

| Key | Action |
|---|---|
| `↑` `↓` | move | 
| `←` `→` | collapse / expand a folder |
| `Space` | toggle the selected file/folder (folders are tri-state) |
| `+` / `-` | select all / none |
| `a` | toggle amend |
| `PgUp` `PgDn` | scroll the diff pane |
| `Ctrl+Enter` or `Ctrl+S` | confirm → opens the message editor |
| `Esc` / `q` | cancel |

In the message editor, `Ctrl+Enter` / `Ctrl+S` commits and `Esc` cancels. The
editor is pre-filled with an AI-drafted message: if the [GitHub Copilot CLI]
(`copilot`) is on `PATH`, the message is generated from the selected diff (plus
the existing jj change description as context) — a "Generating…" screen shows
while it runs, and `Esc` skips it. Without copilot (or if it fails) the editor
falls back to the change's current description (jj) or an empty message (git).

### Choosing the AI model

If copilot reports the configured model is unavailable, `commit` asks you to type
another model name and retries; the working name is saved back to whichever source
supplied the failing one (the per-repo file if a repo override was in effect,
otherwise your user settings) so later runs use it. The model is resolved in this
order (first wins):

1. the `COMMIT_AI_MODEL` environment variable;
2. a per-repo override file `.vcs-flow-commit.toml` in the repo root
   (`model = "…"`) — kept out of version control so it is never committed or
   pushed: a `.git/info/exclude` entry for a colocated git repo, or a `.gitignore`
   entry for a worktree / pure-jj repo;
3. the per-user config file (`model = "…"`) at the platform config dir,
   e.g. `%APPDATA%\vcs-flow\commit.toml` (Windows) or
   `~/.config/vcs-flow/commit.toml` (Linux);
4. the built-in default `gpt-5.4-mini`.

[GitHub Copilot CLI]: https://github.com/github/copilot-cli

> `Ctrl+Enter` needs a terminal that reports it (most modern ones do); `Ctrl+S`
> is the universal fallback.

## What "commit" means per backend

- **git** — commits exactly the selected paths' working-tree content to the
  current branch (`git commit --only`), regardless of what's staged. `--amend`
  amends the branch tip.
- **jj** — finalises a commit containing the selected paths and advances the
  nearest bookmark onto it; deselected changes stay in the working copy. If
  several bookmarks are equally near, you pick one first. Amend squashes the
  selected paths into the nearest bookmark's existing commit instead.

## Pushing

After a (non-amend) commit, `commit` offers to push to `origin`. On agreement it:

1. **Resolves the remote branch.** If your branch/bookmark already tracks a remote
   branch, it uses that. Otherwise it looks for a **same-named** remote branch and
   tracks it. If there's no match, a **filterable picker** of existing remote
   branches opens — type to narrow, **Enter** attaches to the highlighted branch,
   **Ctrl+N** pushes as a new same-named branch, **Esc** cancels.
2. **Pulls if behind.** It fetches and, if the local branch is behind the remote,
   integrates the remote commits first. The strategy is **merge** by default; set
   `pull = "rebase"` in a settings file (or `COMMIT_PULL_STRATEGY=rebase`) to rebase
   instead. (The setting governs git; jj always rebases the bookmark onto the
   remote.) If integration conflicts, `commit` lists the conflicted files and waits
   — resolve and stage them in your own editor, press Enter, and it re-checks and
   pushes once clean (or type `a` to abort).
3. **Pushes**, setting upstream when the branch was untracked.

Amended commits are not auto-pushed (rewriting an already-pushed tip needs a manual
force push).

## License

MIT — see [LICENSE](LICENSE).
