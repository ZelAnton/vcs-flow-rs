//! `commit` — create a commit from the current working-copy change.
//!
//! A thin front-end over `jj commit`: it finalises the working-copy change with
//! a message and starts a fresh empty change on top — the everyday "save my work
//! and move on" step. Runs in the current directory (the repo you invoke it from).

use std::process::ExitCode;

use clap::Parser;
use vcs_jj::{Jj, JjApi};

/// Create a commit from the current working-copy change.
#[derive(Debug, Parser)]
#[command(name = "commit", version, about)]
struct Args {
    /// Commit message. When omitted, `jj` opens your editor.
    #[arg(short, long)]
    message: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    // Compose `jj commit [-m <message>]`. `JjApi::run` executes in the current
    // directory, which is the repo the user invoked the tool from.
    let mut jj_args = vec!["commit".to_string()];
    if let Some(message) = args.message {
        jj_args.push("-m".to_string());
        jj_args.push(message);
    }

    let jj = Jj::new();
    match jj.run(&jj_args).await {
        Ok(output) => {
            let output = output.trim();
            if !output.is_empty() {
                println!("{output}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("commit: {error}");
            ExitCode::FAILURE
        }
    }
}
