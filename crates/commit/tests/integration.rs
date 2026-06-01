//! Integration tests for the `commit` binary.
//!
//! The version/help checks run the built binary directly and need no VCS tool.
//! The real commit flow needs a `jj` binary and a repo, so it is `#[ignore]`d —
//! run it with `cargo test -p vcs-flow-commit -- --ignored`.

use std::process::Command;

/// Path to the freshly built `commit` binary — Cargo sets `CARGO_BIN_EXE_<bin>`
/// for integration tests.
const COMMIT_BIN: &str = env!("CARGO_BIN_EXE_commit");

#[test]
fn prints_version() {
    let output = Command::new(COMMIT_BIN)
        .arg("--version")
        .output()
        .expect("run commit --version");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("commit"), "version output: {stdout}");
}

#[test]
fn help_mentions_message_flag() {
    let output = Command::new(COMMIT_BIN)
        .arg("--help")
        .output()
        .expect("run commit --help");
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--message"), "help output: {stdout}");
}
