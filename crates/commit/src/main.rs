//! `commit` — an interactive TUI to pick changed files (ignoring the index),
//! preview their diffs, write a message, and commit to git or jj.
//!
//! Flow: detect the backend → resolve the commit target (git branch / nearest jj
//! bookmark, with a picker if several) → file-select screen → message editor →
//! commit. See `AGENTS.md`/`README.md` for the keybindings.

mod ai;
mod model;
mod push;
mod repo;
mod settings;
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

    // Offer to push the fresh commit. Skipped for amend: rewriting an already-pushed
    // tip needs a force push, which is out of scope (and the behind-check would try
    // to merge the pre-amend remote commit back in).
    if !plan.amend {
        push::offer(&backend, &plan.target).await?;
    }
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
    let existing = backend.message_for(&target, result.amend).await?;
    // On amend, keep the prior commit message to tweak. Otherwise draft one with
    // AI from the selected diff (seeded by any existing description), falling back
    // to `existing` if copilot is unavailable, fails, or the user skips.
    let prefill = if result.amend {
        existing
    } else {
        let diff = selected_diff(snapshot, tree);
        generate_message(tui, &diff, &existing).await?
    };
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

/// Concatenate the diffs of exactly the selected files. `selected_paths` carries
/// new (and, for renames, old) paths; `Snapshot::diffs` is keyed by the new path,
/// so filtering changes by membership and joining their diffs yields the diff the
/// user is about to commit.
fn selected_diff(snapshot: &Snapshot, tree: &TreeModel) -> String {
    let selected = tree.selected_paths(&snapshot.changes);
    let set: std::collections::HashSet<&str> = selected.iter().map(String::as_str).collect();
    snapshot
        .changes
        .iter()
        .filter(|c| set.contains(c.path.as_str()))
        .filter_map(|c| snapshot.diffs.get(&c.path))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resolve the model, draft a message, and — if copilot rejects the model — let
/// the user enter another (persisted to the user settings once it works). Returns
/// the draft, or the `existing` message if generation is skipped or fails.
///
/// The repo root is the process CWD (set in `run` so jj filesets resolve), which
/// `settings` uses for the per-repo override file and its git-exclude entry.
async fn generate_message(tui: &mut Tui, diff: &str, existing: &str) -> AppResult<String> {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    // `source` is where the originally-configured model came from; a replacement
    // is saved back there so it isn't shadowed by a higher-precedence source.
    let (mut model, source) = settings::resolve_model(&root);
    let mut entered = false; // whether the current model was typed by the user

    loop {
        let Some(outcome) = run_with_spinner(tui, diff, existing, &model).await? else {
            return Ok(existing.to_string()); // Esc during generation
        };
        match outcome {
            ai::Outcome::Drafted(msg) => {
                // Persist a newly-entered model only once it has actually worked.
                if entered {
                    let _ = settings::save_model(&root, source, &model); // best-effort
                }
                return Ok(msg);
            }
            ai::Outcome::ModelUnavailable => {
                let title = format!("Model \"{model}\" is unavailable — enter another:");
                match ui::input::run(tui, &title, "")? {
                    Some(next) => {
                        model = next;
                        entered = true;
                    }
                    None => return Ok(existing.to_string()),
                }
            }
            ai::Outcome::Failed => return Ok(existing.to_string()),
        }
    }
}

/// Draw the animated "Generating…" frame while one `ai::generate` attempt runs.
/// `Ok(None)` means the user pressed Esc to skip; dropping the future kills the
/// copilot subprocess (its job dies with the dropped handle).
async fn run_with_spinner(
    tui: &mut Tui,
    diff: &str,
    existing: &str,
    model: &str,
) -> AppResult<Option<ai::Outcome>> {
    use std::time::Duration;

    use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};

    let mut generate = std::pin::pin!(ai::generate(diff, existing, model));
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    let mut tick: usize = 0;

    loop {
        tokio::select! {
            biased;
            outcome = &mut generate => return Ok(Some(outcome)),
            _ = ticker.tick() => {
                let glyph = ui::busy::SPINNER[tick % ui::busy::SPINNER.len()];
                ui::busy::frame(tui, glyph, "Generating commit message…")?;
                tick = tick.wrapping_add(1);
                // Non-blocking input drain: Esc abandons generation.
                while event::poll(Duration::ZERO)? {
                    if let Event::Key(key) = event::read()?
                        && key.kind == KeyEventKind::Press
                        && key.code == KeyCode::Esc
                    {
                        return Ok(None);
                    }
                }
            }
        }
    }
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
