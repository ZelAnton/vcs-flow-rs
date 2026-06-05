//! `commit` — an interactive TUI to pick changed files (ignoring the index),
//! preview their diffs, write a message, and commit to git or jj.
//!
//! Flow: detect the backend → resolve the commit target (git branch / nearest jj
//! bookmark, with a picker if several) → file-select screen → message editor →
//! commit. See `AGENTS.md`/`README.md` for the keybindings.

mod ai;
mod ai_loop;
mod model;
mod patch;
mod pr;
mod prompt;
mod push;
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
    // Make it absolute so detection can walk up parents — a relative path like `.`
    // or `../x` can't be `pop()`ed past its own components.
    let start = std::path::absolute(&start).unwrap_or(start);
    // `Backend::open` detects git/jj at-or-above `start` (via `vcs_core::detect`)
    // and binds the handle to the repo root.
    let backend = Backend::open(&start)?;
    // Operate from the repo root so the raw escape-hatch commands (which run in the
    // process cwd) and root-relative paths agree.
    std::env::set_current_dir(backend.root())?;

    let mut snapshot = backend.snapshot().await?;
    if snapshot.changes.is_empty() {
        println!("Nothing to commit — no changed tracked files.");
        return Ok(ExitCode::SUCCESS);
    }

    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err("commit is interactive; run it in a terminal".into());
    }

    let highlighter = Highlighter::new();
    // The last commit made this session — the push offer at the end targets it.
    let mut last: Option<Plan> = None;
    let mut round: u32 = 0;

    // Multi-commit session: after each commit, re-snapshot and offer another
    // round over whatever is left, until nothing remains or the user stops.
    loop {
        if round > 0 {
            snapshot = backend.snapshot().await?;
            if snapshot.changes.is_empty() {
                println!("Nothing left to commit.");
                break;
            }
            let n = snapshot.changes.len();
            let noun = if n == 1 { "file" } else { "files" };
            if !prompt::confirm_no(&format!("{n} changed {noun} remain — commit more?"))? {
                break;
            }
        }

        // Fresh targets each round: a jj commit moves the bookmark (and `@`).
        let targets = backend.targets().await?;
        let mut tree = TreeModel::build(&snapshot.changes);
        // Hunk-level granularity for multi-hunk modified files (git only —
        // `snapshot.hunks` is empty for jj, leaving the tree whole-file).
        tree.with_hunks(&snapshot.changes, &snapshot.hunks);

        // Interactive session: the terminal is restored when `guard` drops at
        // the end of this block, before any commit output is printed.
        let plan = {
            let (mut tui, _guard) = TerminalGuard::enter()?;
            interactive(
                &mut tui,
                &backend,
                &snapshot,
                &mut tree,
                &targets,
                args.amend && round == 0, // `--amend` applies to the first round
                &highlighter,
            )
            .await?
        };

        let Some(plan) = plan else {
            if round == 0 {
                println!("Aborted — nothing committed.");
                return Ok(ExitCode::SUCCESS);
            }
            break; // a later-round cancel still push-offers the earlier commits
        };

        // Whole files and per-file hunk subsets travel separately; `partial` is
        // non-empty only when the user narrowed a file to some of its hunks.
        let whole = tree.selected_whole_paths(&snapshot.changes);
        let partial = tree.selected_partial(&snapshot.changes);
        // Count selected files for the summary, not emitted paths (a rename emits two).
        let file_count = tree.selected_count();
        if let Err(e) = backend
            .commit(&whole, &partial, &plan.message, plan.amend, &plan.target)
            .await
        {
            // A first-round failure is the session failing. A *later*-round
            // failure must not abandon the commits already made — report it
            // and fall through to their push offer.
            if round == 0 {
                return Err(e);
            }
            eprintln!("commit: {e}");
            eprintln!("(the session's earlier commits are intact — continuing to the push offer)");
            break;
        }
        println!("{}", success_line(backend.kind(), &plan, file_count));
        last = Some(plan);
        round += 1;
    }

    // One push offer for the session, aimed at the last commit. An amend
    // rewrites the tip, so its offer is the guarded force-push variant.
    if let Some(plan) = last {
        if plan.amend {
            push::offer_amend(&backend, &plan.target).await?;
        } else {
            push::offer(&backend, &plan.target).await?;
        }
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

    // Amend needs something to amend into: a jj describe-only target (no bookmark)
    // has no prior commit, so it's really a normal commit — don't claim "Amend".
    let amend =
        result.amend && !(matches!(backend.kind(), BackendKind::Jj) && target.label.is_empty());

    let header = message_header(backend.kind(), &target, amend);
    let existing = backend.message_for(&target, amend).await?;
    // On amend, keep the prior commit message to tweak. Otherwise draft one with
    // AI from the selected diff (seeded by any existing description), falling back
    // to `existing` if copilot is unavailable, fails, or the user skips.
    let prefill = if amend {
        existing
    } else {
        // The repo root is the process CWD (set in `run` so jj filesets resolve),
        // which `settings` uses for the per-repo override file.
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let conventional = settings::conventional(&root);
        let diff = selected_diff(snapshot, tree);
        let mut draft = ai_loop::draft_with_retry(
            tui,
            &root,
            ai_loop::Draft::Commit {
                diff: &diff,
                existing: &existing,
                conventional,
            },
            "Generating commit message…",
            &existing,
        )
        .await?;
        // Conventional Commits, but the draft has no type (AI unavailable,
        // skipped, or off-format): offer the type picker; Esc keeps it plain.
        if conventional && !has_conventional_prefix(&draft) {
            let types: Vec<String> = CC_TYPES.iter().map(|s| (*s).to_string()).collect();
            if let ui::filter::Pick::Existing(t) = ui::filter::run(
                tui,
                "Conventional Commit type (Esc — none):",
                "Types",
                &types,
                None,
            )? {
                draft = format!("{t}: {draft}");
            }
        }
        draft
    };
    let Some(message) = ui::editor::run(tui, &prefill, &header)? else {
        return Ok(None);
    };
    if message.trim().is_empty() {
        return Err("empty commit message — nothing committed".into());
    }
    Ok(Some(Plan {
        target,
        amend,
        message,
    }))
}

/// Concatenate the diff of exactly what's being committed: the full diff of
/// whole-selected files, and only the selected hunks of partially-selected
/// ones — so the AI drafts from what will actually land.
fn selected_diff(snapshot: &Snapshot, tree: &TreeModel) -> String {
    let whole = tree.selected_whole_paths(&snapshot.changes);
    let whole_set: std::collections::HashSet<&str> = whole.iter().map(String::as_str).collect();
    let partial: std::collections::HashMap<String, Vec<usize>> = tree
        .selected_partial(&snapshot.changes)
        .into_iter()
        .collect();

    let mut parts: Vec<String> = Vec::new();
    for c in &snapshot.changes {
        if whole_set.contains(c.path.as_str()) {
            if let Some(d) = snapshot.diffs.get(&c.path) {
                parts.push(d.clone());
            }
        } else if let Some(selected) = partial.get(&c.path)
            && let Some(hunks) = snapshot.hunks.get(&c.path)
        {
            let mut text = format!("diff --git a/{p} b/{p} (selected hunks)\n", p = c.path);
            for &k in selected {
                if let Some(h) = hunks.get(k) {
                    text.push_str(&h.text);
                }
            }
            parts.push(text);
        }
    }
    parts.join("\n")
}

/// The Conventional Commits types offered by the picker (and recognised by
/// [`has_conventional_prefix`]).
const CC_TYPES: &[&str] = &[
    "feat", "fix", "docs", "refactor", "test", "chore", "build", "ci", "perf", "style", "revert",
];

/// Whether the first line already looks like a Conventional Commit
/// (`type(optional-scope)!?: …` with a known type).
fn has_conventional_prefix(message: &str) -> bool {
    let first = message.lines().next().unwrap_or("");
    let Some((head, _)) = first.split_once(':') else {
        return false;
    };
    let head = head.trim_end_matches('!');
    let head = head.split('(').next().unwrap_or(head);
    CC_TYPES.contains(&head)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_conventional_prefixes() {
        assert!(has_conventional_prefix("feat: add picker"));
        assert!(has_conventional_prefix("fix(ui): clamp scroll\n\nbody"));
        assert!(has_conventional_prefix("refactor!: drop old API"));
        assert!(has_conventional_prefix("chore(deps)!: bump toolkit"));
        assert!(!has_conventional_prefix("Add picker"));
        assert!(!has_conventional_prefix("feature: not a known type"));
        assert!(!has_conventional_prefix("feat add picker")); // no colon
        assert!(!has_conventional_prefix(""));
    }
}
