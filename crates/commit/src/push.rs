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

use crate::prompt::{confirm, confirm_no};
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
                 uncommitted changes (e.g. files or hunks left out of the commit). \
                 Commit or stash them and pull manually, then push. Nothing pushed."
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
        // Control characters are stripped (newlines kept): push output echoes
        // `remote:` lines the server controls, which must not be able to emit
        // terminal escapes.
        let message = sanitize_multiline(result.diagnostic());
        let message = if message.is_empty() {
            "(no output)".to_string()
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

/// Offer to push an *amended* commit. Rewriting a tip that's already on the
/// remote needs a force push, so when the branch tracks a remote one this asks
/// a default-**no** question and pushes with lease semantics (git
/// `--force-with-lease`; jj's bookmark push is leased by design). Deliberately
/// no fetch first — refreshing the remote-tracking ref would defeat the lease.
/// An untracked branch has nothing remote to rewrite → the normal offer runs.
pub async fn offer_amend(backend: &Backend, target: &crate::model::Target) -> crate::AppResult<()> {
    let Some(name) = backend.push_name(target).await? else {
        return Ok(()); // detached HEAD / no bookmark — nothing to push
    };
    let Some(remote_branch) = backend.upstream(&name).await? else {
        return offer(backend, target).await; // never pushed — a plain push is safe
    };

    if !confirm_no(&format!(
        "Force-push '{name}' to origin/{remote_branch}? This rewrites the remote tip."
    ))? {
        return Ok(());
    }

    println!("Pushing (force-with-lease)…");
    let result = backend.push_force(&name, &remote_branch).await?;
    if result.is_success() {
        println!("Pushed '{name}' → origin/{remote_branch} (forced).");
        // Post-push GitHub PR step — same best-effort hook as the normal push.
        if let Err(e) = crate::pr::after_push(backend, &remote_branch).await {
            eprintln!("\x1b[2m(PR step skipped: {e})\x1b[0m");
        }
    } else {
        // Control-stripped like the normal push: `remote:` lines are
        // server-controlled.
        let message = sanitize_multiline(result.diagnostic());
        let message = if message.is_empty() {
            "(no output)".to_string()
        } else {
            message
        };
        eprintln!("Push failed:\n{message}");
        eprintln!(
            "(A lease rejection means the remote moved since your last fetch — \
             fetch, re-check the remote commits, and push manually if rewriting \
             them is really intended.)"
        );
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
            // A dead-end the tool can't drive further — same graceful stop as
            // the other push-flow bail-outs (the commit already succeeded).
            Integration::Unresolved(msg) => {
                println!("{msg} Nothing pushed.");
                return Ok(false);
            }
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
    let pick = ui::filter::run(&mut tui, title, "Remote branches", remotes, Some(&alt))?;
    Ok(pick) // guard drops here → terminal restored before we print again
}

/// Strip terminal control characters from a multi-line subprocess diagnostic,
/// keeping line breaks. Push output relays `remote:` lines the server authors —
/// they must not be able to emit SGR/CSI/OSC escapes into our terminal.
fn sanitize_multiline(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\n' || !c.is_control())
        .collect()
}

/// The repo root (process CWD, set in `main::run`), for settings lookup.
fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
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
