# commit

Create a commit from the current working-copy change. A thin front-end over
`jj commit`: it finalises the working-copy change with a message and starts a
fresh empty change on top.

Part of [vcs-flow-rs](https://github.com/ZelAnton/vcs-flow-rs). The crate is
published as `vcs-flow-commit`; the binary it installs is `commit`.

## Install

```bash
cargo install vcs-flow-commit       # from crates.io
cargo install --path crates/commit  # from a checkout
```

## Usage

Run it inside a (jj-colocated) repository:

```bash
commit -m "fix(parser): handle empty input"   # commit with a message
commit                                          # no -m: jj opens your editor
```

Exit code is `0` on success; on failure it prints `commit: <error>` to stderr
and exits non-zero.

## License

MIT — see [LICENSE](LICENSE).
