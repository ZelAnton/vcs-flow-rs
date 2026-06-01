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

In the message editor, `Ctrl+Enter` / `Ctrl+S` commits and `Esc` cancels. For a
jj repo the editor is pre-filled with the change's current description.

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

## License

MIT — see [LICENSE](LICENSE).
