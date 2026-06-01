//! `commit` — an interactive TUI to pick changed files (ignoring the index),
//! preview their diffs, write a message, and commit to git or jj.
//!
//! Flow: detect the backend → resolve the commit target (git branch / nearest jj
//! bookmark, with a picker if several) → file-select screen → message editor →
//! commit. See `AGENTS.md`/`README.md` for the keybindings.

mod model;
mod repo;
mod tree;
mod ui;
mod vcs;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use crate::model::{BackendKind, Target};
use crate::tree::TreeModel;
use crate::ui::diff::Highlighter;
use crate::ui::terminal::{TerminalGuard, Tui};
use crate::vcs::{Backend, Snapshot};

/// Interactive commit: pick changed files, preview diffs, write a message, commit.
#[derive(Debug, Parser)]
#[command(name = "commit", version, about)]
struct Args {
    /// Repository directory to operate on (defaults to the current directory).
    #[arg(short = 'C', long = "dir")]
    dir: Option<PathBuf>,
    /// Start with amend enabled (also toggleable in-app with `a`).
    #[arg(short, long)]
    amend: bool,
}

type AppResult<T> = Result<T, Box<dyn std::error::Error>>;

/// A confirmed commit, ready to execute once the terminal is restored.
struct Plan {
    target: Target,
    amend: bool,
    message: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("commit: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> AppResult<ExitCode> {
    let args = Args::parse();

    let start = match &args.dir {
        Some(dir) => dir.clone(),
        None => std::env::current_dir()?,
    };
    // Make it absolute so `locate` can walk up parents — a relative path like `.`
    // or `../x` can't be `pop()`ed past its own components.
    let start = std::path::absolute(&start).unwrap_or(start);
    let Some(loc) = repo::locate(&start) else {
        return Err("not inside a git or jj repository".into());
    };
    // Operate from the repo root so jj filesets and diff paths are root-relative.
    std::env::set_current_dir(&loc.root)?;

    let backend = Backend::new(&loc);
    let snapshot = backend.snapshot().await?;
    if snapshot.changes.is_empty() {
        println!("Nothing to commit — no changed tracked files.");
        return Ok(ExitCode::SUCCESS);
    }

    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err("commit is interactive; run it in a terminal".into());
    }

    let targets = backend.targets().await?;
    let mut tree = TreeModel::build(&snapshot.changes);
    let highlighter = Highlighter::new();

    // Interactive session: the terminal is restored when `guard` drops at the
    // end of this block, before any commit output is printed.
    let plan = {
        let (mut tui, _guard) = TerminalGuard::enter()?;
        interactive(
            &mut tui,
            &backend,
            &snapshot,
            &mut tree,
            &targets,
            args.amend,
            &highlighter,
        )
        .await?
    };

    let Some(plan) = plan else {
        println!("Aborted — nothing committed.");
        return Ok(ExitCode::SUCCESS);
    };

    let paths = tree.selected_paths(&snapshot.changes);
    // Count selected files for the summary, not emitted paths (a rename emits two).
    let file_count = tree.selected_count();
    backend
        .commit(&paths, &plan.message, plan.amend, &plan.target)
        .await?;
    println!("{}", success_line(backend.kind(), &plan, file_count));
    Ok(ExitCode::SUCCESS)
}

/// Drive the three interactive screens. Returns `None` if the user cancels any.
async fn interactive(
    tui: &mut Tui,
    backend: &Backend,
    snapshot: &Snapshot,
    tree: &mut TreeModel,
    targets: &[Target],
    initial_amend: bool,
    highlighter: &Highlighter,
) -> AppResult<Option<Plan>> {
    let target = if targets.len() > 1 {
        match ui::menu::pick(tui, targets)? {
            Some(i) => targets[i].clone(),
            None => return Ok(None),
        }
    } else {
        targets.first().cloned().ok_or("no commit target found")?
    };

    let result = ui::select::run(
        tui,
        snapshot,
        tree,
        &target,
        backend.kind(),
        initial_amend,
        highlighter,
    )?;
    if !result.confirmed {
        return Ok(None);
    }

    let header = message_header(backend.kind(), &target, result.amend);
    let prefill = backend.message_for(&target, result.amend).await?;
    let Some(message) = ui::editor::run(tui, &prefill, &header)? else {
        return Ok(None);
    };
    if message.trim().is_empty() {
        return Err("empty commit message — nothing committed".into());
    }
    Ok(Some(Plan {
        target,
        amend: result.amend,
        message,
    }))
}

fn where_to(kind: BackendKind, target: &Target) -> String {
    if target.label.is_empty() {
        return "working-copy change".to_string();
    }
    match kind {
        BackendKind::Git => format!("branch {}", target.label),
        BackendKind::Jj => format!("bookmark {}", target.label),
    }
}

fn message_header(kind: BackendKind, target: &Target, amend: bool) -> String {
    let verb = if amend { "Amend" } else { "Commit" };
    format!("{verb} message — {}", where_to(kind, target))
}

fn success_line(kind: BackendKind, plan: &Plan, count: usize) -> String {
    let action = if plan.amend { "Amended" } else { "Committed" };
    let noun = if count == 1 { "file" } else { "files" };
    format!(
        "{action} {count} {noun} to {}.",
        where_to(kind, &plan.target)
    )
}
