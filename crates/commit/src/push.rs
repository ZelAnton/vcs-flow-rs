//! Post-commit push flow: offer to push, resolve where to push (tracked upstream →
//! same-named remote → pick an existing remote branch), pull in remote commits when
//! the local branch is behind (merge or rebase, per the `pull` setting; conflicts
//! pause for the user to resolve), then push.
//!
//! Runs in normal terminal mode — simple stdin prompts and status lines — so the
//! conflict "pause & wait" lets the user resolve in their own editor while we block
//! on stdin. The full-screen TUI is re-entered only for the branch picker.

use std::io::{self, Write};
use std::path::PathBuf;

use crate::settings::{self, PullStrategy};
use crate::ui;
use crate::ui::filter::Pick;
use crate::vcs::{Backend, Integration};

/// Offer to push the just-committed work and run the push sequence on agreement.
/// Best-effort and non-fatal in spirit: a declined prompt or a benign stop returns
/// `Ok(())`; only unexpected backend errors propagate.
pub async fn offer(backend: &Backend, target: &crate::model::Target) -> crate::AppResult<()> {
    let Some(name) = backend.push_name(target).await? else {
        return Ok(()); // detached HEAD / no bookmark — nothing to push
    };

    if !confirm(&format!("Push '{name}' to origin?"))? {
        return Ok(());
    }

    println!("Fetching…");
    backend.fetch().await?;

    let Some((local, remote_branch, set_upstream, remote_exists)) =
        resolve_target(backend, &name).await?
    else {
        println!("Push cancelled.");
        return Ok(());
    };

    // Integrate remote commits when the existing remote branch is ahead of us.
    if remote_exists && backend.behind(&local, &remote_branch).await? {
        // A git rebase/merge can't run over uncommitted changes (left behind by a
        // partial commit). Don't risk a half-integration — tell the user and stop.
        if backend.working_tree_dirty().await? {
            println!(
                "'{local}' is behind origin/{remote_branch}, but the working tree has \
                 uncommitted changes. Commit or stash them and pull manually, then push. \
                 Nothing pushed."
            );
            return Ok(());
        }
        let strategy = settings::pull_strategy(&cwd());
        if !integrate_with_pause(backend, &local, &remote_branch, strategy).await? {
            return Ok(()); // user aborted the integration
        }
    }

    println!("Pushing…");
    let result = backend.push(&local, &remote_branch, set_upstream).await?;
    if result.is_success() {
        println!("Pushed '{local}' → origin/{remote_branch}.");
        // Post-push GitHub PR step (list open PRs / offer to create one).
        // Strictly best-effort: the push has succeeded, so even an unexpected
        // error here is reduced to a dim notice rather than a failure.
        if let Err(e) = crate::pr::after_push(backend, &remote_branch).await {
            eprintln!("\x1b[2m(PR step skipped: {e})\x1b[0m");
        }
    } else {
        // `diagnostic()` is stderr, else stdout (git sometimes writes there),
        // trimmed — so a stderr-silent rejection still reports something.
        let message = result.diagnostic();
        let message = if message.is_empty() {
            "(no output)"
        } else {
            message
        };
        eprintln!("Push failed:\n{message}");
        if matches!(backend.kind(), crate::model::BackendKind::Git) {
            eprintln!(
                "(A non-fast-forward rejection after an amend needs a manual force push, \
                 e.g. `git push --force-with-lease`.)"
            );
        }
    }
    Ok(())
}

/// Decide where to push. Returns `(local_name, remote_branch, set_upstream,
/// remote_exists)`, or `None` if the user cancels. `local_name` may differ from the
/// input (jj renames the bookmark when attaching to a differently-named remote
/// branch — see [`Backend::attach`]).
async fn resolve_target(
    backend: &Backend,
    name: &str,
) -> crate::AppResult<Option<(String, String, bool, bool)>> {
    // Already tracked → push there, no upstream change, remote exists.
    if let Some(rb) = backend.upstream(name).await? {
        return Ok(Some((name.to_string(), rb, false, true)));
    }

    // Untracked: a same-named remote branch → attach to it.
    let remotes = backend.remote_branches().await?;
    if remotes.iter().any(|b| b == name) {
        let local = backend.attach(name, name).await?;
        return Ok(Some((local, name.to_string(), true, true)));
    }

    // No same-named remote → pick an existing one, or create a new same-named branch.
    let title = format!("No remote branch named '{name}' — pick one to push to:");
    match pick_remote(&title, &remotes, name)? {
        Pick::Existing(rb) => {
            let local = backend.attach(name, &rb).await?;
            Ok(Some((local, rb, true, true)))
        }
        Pick::NewBranch => Ok(Some((name.to_string(), name.to_string(), true, false))),
        Pick::Cancel => Ok(None),
    }
}

/// Drive integration, pausing for the user to resolve conflicts. Returns `true`
/// when integration is clean (ready to push), `false` if the user aborted.
async fn integrate_with_pause(
    backend: &Backend,
    name: &str,
    remote_branch: &str,
    strategy: PullStrategy,
) -> crate::AppResult<bool> {
    // jj always rebases regardless of the setting; reflect that in the message.
    let how = match (backend.kind(), strategy) {
        (crate::model::BackendKind::Jj, _) | (_, PullStrategy::Rebase) => "rebase",
        (_, PullStrategy::Merge) => "merge",
    };
    println!("'{name}' is behind origin/{remote_branch} — integrating ({how})…");
    let pre_op = backend.pre_integration_op().await?;
    let mut state = backend.integrate(name, remote_branch, strategy).await?;

    loop {
        match state {
            Integration::Clean => return Ok(true),
            Integration::Conflicts(files) => {
                println!("\nConflicts in {} file(s):", files.len());
                for f in &files {
                    println!("  {f}");
                }
                if prompt_abort_or_continue()? {
                    if backend
                        .abort_integration(strategy, pre_op.as_deref())
                        .await?
                    {
                        println!("Integration aborted — nothing pushed.");
                    } else {
                        eprintln!(
                            "Could not fully roll back the integration — the repo may be \
                             mid-merge/rebase. Resolve it manually. Nothing pushed."
                        );
                    }
                    return Ok(false);
                }
                state = backend.continue_integration(name, strategy).await?;
            }
        }
    }
}

/// Re-enter the TUI briefly for the filterable remote-branch picker.
fn pick_remote(title: &str, remotes: &[String], new_name: &str) -> crate::AppResult<Pick> {
    let (mut tui, _guard) = ui::terminal::TerminalGuard::enter()?;
    let alt = format!("new '{new_name}'");
    let pick = ui::filter::run(&mut tui, title, remotes, Some(&alt))?;
    Ok(pick) // guard drops here → terminal restored before we print again
}

/// The repo root (process CWD, set in `main::run`), for settings lookup.
fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// `[Y/n]` prompt, default yes. EOF (closed stdin) declines — never push without
/// an explicit answer.
fn confirm(question: &str) -> crate::AppResult<bool> {
    print!("{question} [Y/n] ");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        println!();
        return Ok(false); // EOF → decline
    }
    let a = line.trim();
    Ok(a.is_empty() || a.eq_ignore_ascii_case("y"))
}

/// Wait for the user to resolve conflicts. Returns `true` if they chose to abort.
/// EOF aborts (so a closed stdin can't spin the re-check loop or push a conflict).
fn prompt_abort_or_continue() -> crate::AppResult<bool> {
    print!("Resolve & stage the conflicts, then press Enter to continue (or 'a' to abort): ");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Ok(true); // EOF → abort
    }
    Ok(line.trim().eq_ignore_ascii_case("a"))
}
