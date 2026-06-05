//! Post-push GitHub PR step: list the open PRs whose head is the just-pushed
//! branch (clickable links), or offer to create one — drafting the title and
//! description with AI, letting the user edit them, and opening the PR page in
//! the browser via `gh pr create --web`. While the create question is pending
//! the user can switch into the branch-vs-base diff review (`ui::review`) and
//! bulk-revert marked files in the working copy (backed up to a temp patch).
//!
//! Strictly best-effort: a non-GitHub remote, a missing or unauthenticated
//! `gh`, or any GitHub hiccup skips the step (at most one dim notice). The
//! push has already succeeded; nothing here may make it look failed.

use std::collections::HashSet;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use vcs_github::{GitHub, GitHubAt};

use crate::tree::TreeModel;
use crate::ui;
use crate::ui::diff::Highlighter;
use crate::ui::filter::Pick;
use crate::ui::review::ReviewOutcome;
use crate::vcs::{Backend, Snapshot};
use crate::{AppResult, ai_loop};

/// An open PR whose head is the pushed branch, from `gh pr list --json`.
#[derive(Debug, PartialEq, Eq, Deserialize)]
struct OpenPr {
    number: u64,
    title: String,
    #[serde(rename = "baseRefName")]
    base: String,
    url: String,
    /// CI rollup. Absent in older payloads → empty (no checks line printed).
    #[serde(rename = "statusCheckRollup", default)]
    checks: Vec<Check>,
}

/// One rollup entry. The array is heterogeneous: a CheckRun carries
/// `status`/`conclusion`, a commit StatusContext carries `state` — model all
/// three (defaulted) and let [`check_bucket`] pick whichever is filled.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
struct Check {
    state: String,
    status: String,
    conclusion: String,
}

/// Classify one rollup entry: passed / failed / still pending.
fn check_bucket(c: &Check) -> CheckBucket {
    // CheckRun: `conclusion` is empty until `status` reaches COMPLETED.
    let verdict = if !c.conclusion.is_empty() {
        c.conclusion.as_str()
    } else if !c.state.is_empty() {
        c.state.as_str() // StatusContext
    } else {
        c.status.as_str() // an unfinished CheckRun (QUEUED / IN_PROGRESS)
    };
    match verdict {
        "SUCCESS" | "NEUTRAL" | "SKIPPED" => CheckBucket::Pass,
        "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
            CheckBucket::Fail
        }
        _ => CheckBucket::Pending,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CheckBucket {
    Pass,
    Fail,
    Pending,
}

/// `checks: ✓3 ✗1 ●2` summary (zero buckets omitted); `None` when there are no
/// checks at all.
fn checks_line(checks: &[Check]) -> Option<String> {
    if checks.is_empty() {
        return None;
    }
    let (mut pass, mut fail, mut pending) = (0, 0, 0);
    for c in checks {
        match check_bucket(c) {
            CheckBucket::Pass => pass += 1,
            CheckBucket::Fail => fail += 1,
            CheckBucket::Pending => pending += 1,
        }
    }
    let mut parts = Vec::new();
    if pass > 0 {
        parts.push(format!("✓{pass}"));
    }
    if fail > 0 {
        parts.push(format!("✗{fail}"));
    }
    if pending > 0 {
        parts.push(format!("●{pending}"));
    }
    Some(format!("checks: {}", parts.join(" ")))
}

/// Run the PR step after a successful push of `remote_branch`. Errors that
/// reach the caller are unexpected (terminal/backend IO); everything
/// GitHub-shaped is swallowed here with a dim notice at most.
pub async fn after_push(backend: &Backend, remote_branch: &str) -> AppResult<()> {
    // Cheap pre-filter: only an `origin` pointing at GitHub qualifies. `None`
    // also covers a pure-jj repo (no colocated `.git`), where `gh` can't work.
    let Some(url) = backend.remote_url().await else {
        return Ok(());
    };
    if !url.contains("github.com") {
        return Ok(());
    }

    let client = GitHub::new();
    let gh = client.at(backend.root());
    match gh.auth_status().await {
        Ok(true) => {}
        Ok(false) => {
            dim("gh is not authenticated — run 'gh auth login' to enable the PR step");
            return Ok(());
        }
        Err(_) => {
            dim("gh CLI unavailable — skipping the PR step");
            return Ok(());
        }
    }
    // Failing to resolve the repo (e.g. the remote isn't reachable) just means
    // no PR step — the push itself is done.
    let Ok(repo) = gh.repo_view().await else {
        return Ok(());
    };

    let prs = open_prs_for_branch(&gh, remote_branch).await;
    if !prs.is_empty() {
        let plural = if prs.len() == 1 { "" } else { "s" };
        println!("\nOpen pull request{plural} for '{remote_branch}':");
        for pr in &prs {
            println!("  #{} {}  → {}", pr.number, pr.title, pr.base);
            println!("      {}", osc8(&pr.url));
            if let Some(line) = checks_line(&pr.checks) {
                println!("      {line}");
            }
        }
        offer_open_in_browser(backend, &prs)?;
        return Ok(());
    }

    // Pushing the default branch itself: there's nothing it would merge into
    // by default, so don't offer to create a PR. (Existing PRs *from* it —
    // e.g. main → release — were still listed above.)
    let base = default_base(&repo);
    if remote_branch == base {
        return Ok(());
    }
    create_pr_flow(backend, remote_branch, base).await
}

/// Offer to open one of the listed PRs in the browser (`gh pr view --web`).
/// One PR → yes/no; several → enter the PR number (Enter skips).
fn offer_open_in_browser(backend: &Backend, prs: &[OpenPr]) -> AppResult<()> {
    let number = if let [only] = prs {
        if !crate::prompt::confirm_no("Open it in the browser?")? {
            return Ok(());
        }
        only.number
    } else {
        print!("Open one in the browser? Enter its number (or just Enter to skip): #");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            println!();
            return Ok(());
        }
        let Ok(n) = line.trim().trim_start_matches('#').parse::<u64>() else {
            return Ok(()); // blank or not a number → skip
        };
        if !prs.iter().any(|p| p.number == n) {
            println!("No PR #{n} in the list.");
            return Ok(());
        }
        n
    };
    // std spawn, not processkit: a kill-on-close job would also kill a freshly
    // launched browser the moment `gh` exits (same reasoning as `create_pr`).
    let out = std::process::Command::new("gh")
        .args(["pr", "view", &number.to_string(), "--web"])
        .current_dir(backend.root())
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            dim(&format!("could not open the PR: {}", err.trim()));
        }
        Err(e) => dim(&format!("could not open the PR: {e}")),
    }
    Ok(())
}

/// All open PRs with head = `head`, into any base. Any failure (including
/// malformed JSON) reads as "no PRs" — the create offer is the safe fallback.
///
/// Raw `gh` because the typed API has no "open PRs by head into *any* base".
/// Note `run` on the bound view is a *bare* forwarder — it executes in the
/// process cwd, which `main` pins to the repo root (so `gh` resolves the repo).
async fn open_prs_for_branch(gh: &GitHubAt<'_>, head: &str) -> Vec<OpenPr> {
    let argv = args(&[
        "pr",
        "list",
        "--head",
        head,
        "--state",
        "open",
        "--json",
        "number,title,baseRefName,url,statusCheckRollup",
    ]);
    match gh.run(&argv).await {
        Ok(json) => parse_open_prs(&json),
        Err(_) => Vec::new(),
    }
}

/// What the user answered to the create-PR question.
enum Answer {
    Yes,
    No,
    Base,
    Diff,
}

/// The create-PR flow. The revert summary carries the backup-patch paths, so it
/// must print on *every* exit — including an error propagating out of the inner
/// loop — hence the wrapper/loop split.
async fn create_pr_flow(backend: &Backend, head: &str, initial_base: String) -> AppResult<()> {
    let mut reverted: Vec<String> = Vec::new();
    let mut backups: Vec<PathBuf> = Vec::new();
    let result = create_pr_loop(backend, head, initial_base, &mut reverted, &mut backups).await;
    if !reverted.is_empty() {
        println!(
            "Reverted {} path(s) in the working copy — the pushed branch itself is unchanged.",
            reverted.len()
        );
        for b in &backups {
            println!("Backup patch: {}", b.display());
        }
        println!("Commit and push to update the branch.");
    }
    result
}

/// Ask, optionally re-pick the base or review the diff, and create on
/// agreement. The arms swallow their own errors: a failed picker, diff review,
/// or PR creation must not eject the user from the loop. Only an `ask_create`
/// (stdin/stdout) failure propagates — there's no conversation left without it.
async fn create_pr_loop(
    backend: &Backend,
    head: &str,
    initial_base: String,
    reverted: &mut Vec<String>,
    backups: &mut Vec<PathBuf>,
) -> AppResult<()> {
    let mut base = initial_base;
    // Lazy: loading the syntect dumps costs ~100 ms — pay it on the first `d`,
    // and only once per flow (not per review re-entry).
    let mut highlighter: Option<Highlighter> = None;
    println!("\nNo open pull request for '{head}'.");
    loop {
        match ask_create(head, &base)? {
            Answer::No => return Ok(()),
            Answer::Base => match pick_base(backend, head, &base).await {
                Ok(Some(b)) => base = b,
                Ok(None) => {} // cancelled → keep the current base
                Err(e) => eprintln!("Base picker unavailable: {e}"),
            },
            Answer::Diff => {
                let hl = highlighter.get_or_insert_with(Highlighter::new);
                if let Err(e) = review_loop(backend, head, &base, hl, reverted, backups).await {
                    eprintln!("Diff review unavailable: {e}");
                }
            }
            Answer::Yes => {
                if let Err(e) = create_pr(backend, head, &base).await {
                    eprintln!("Could not prepare the pull request: {e}");
                }
                return Ok(());
            }
        }
    }
}

/// The `[Y]es / [n]o / [b]ase / [d]iff` prompt. Default yes; EOF declines (never
/// open a browser without an explicit answer); anything else re-asks.
fn ask_create(head: &str, base: &str) -> AppResult<Answer> {
    loop {
        print!("Create a pull request '{head}' → '{base}'?  [Y]es / [n]o / [b]ase / [d]iff: ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            println!();
            return Ok(Answer::No); // EOF → decline
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => return Ok(Answer::Yes),
            "n" | "no" => return Ok(Answer::No),
            "b" | "base" => return Ok(Answer::Base),
            "d" | "diff" => return Ok(Answer::Diff),
            _ => {} // unrecognized → re-ask
        }
    }
}

/// Re-enter the TUI for the filterable base-branch picker (no `Ctrl+N` action —
/// the base must already exist). `None` keeps the current base.
async fn pick_base(backend: &Backend, head: &str, current: &str) -> AppResult<Option<String>> {
    let branches: Vec<String> = backend
        .remote_branches()
        .await?
        .into_iter()
        .filter(|b| b != head)
        .collect();
    if branches.is_empty() {
        println!("No other branch on origin to target.");
        return Ok(None);
    }
    let title = format!("Choose the PR base branch (current: {current}):");
    let (mut tui, _guard) = ui::terminal::TerminalGuard::enter()?;
    match ui::filter::run(&mut tui, &title, "Remote branches", &branches, None)? {
        Pick::Existing(b) => Ok(Some(b)),
        _ => Ok(None), // Esc (NewBranch is disabled) → keep the current base
    }
}

/// The diff-review mode: show the branch-vs-base tree, and on a confirmed
/// bulk revert back up + revert the marked paths, then re-enter with those
/// files dropped from the *in-memory* view. (The committed branch diff is
/// unchanged by a working-copy revert; re-querying it would resurrect them.)
async fn review_loop(
    backend: &Backend,
    head: &str,
    base: &str,
    highlighter: &Highlighter,
    reverted: &mut Vec<String>,
    backups: &mut Vec<PathBuf>,
) -> AppResult<()> {
    let mut snap = backend.review_snapshot(base, head).await?;
    retain_unreverted(&mut snap, reverted);
    if snap.changes.is_empty() {
        let qualifier = if reverted.is_empty() {
            ""
        } else {
            " left to review"
        };
        println!("'{head}' has no changes against '{base}'{qualifier}.");
        return Ok(());
    }

    loop {
        let mut tree = TreeModel::build(&snap.changes);
        let outcome = {
            let (mut tui, _guard) = ui::terminal::TerminalGuard::enter()?;
            ui::review::run(&mut tui, &snap, &mut tree, head, base, highlighter)?
        }; // guard dropped → normal terminal for the revert/output below

        match outcome {
            ReviewOutcome::Back => return Ok(()),
            ReviewOutcome::RevertMarked { paths } => {
                match backend.revert_paths(base, head, &paths).await {
                    Ok(backup) => {
                        backups.push(backup);
                        reverted.extend(paths);
                        retain_unreverted(&mut snap, reverted);
                        if snap.changes.is_empty() {
                            return Ok(()); // everything reverted — back to the question
                        }
                    }
                    Err(e) => {
                        // `git apply` is atomic — a failure leaves every file as
                        // it was. The usual cause: a marked file also carries
                        // uncommitted local edits, so the patch context no
                        // longer matches the working tree.
                        eprintln!(
                            "Revert failed (nothing was changed): {e}\n\
                             (uncommitted local edits in a marked file can \
                             prevent the revert — commit or stash them first)"
                        );
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Drop the already-reverted files (by new or old/rename path) from the snapshot.
fn retain_unreverted(snap: &mut Snapshot, reverted: &[String]) {
    let set: HashSet<&str> = reverted.iter().map(String::as_str).collect();
    snap.changes.retain(|c| {
        !set.contains(c.path.as_str()) && !c.old_path.as_deref().is_some_and(|p| set.contains(p))
    });
}

/// Draft title+description (filling the repo's PR template when one exists),
/// let the user edit, optionally mark as draft, and open the PR page in the
/// browser with everything prefilled (`gh pr create --web` launches the browser).
async fn create_pr(backend: &Backend, head: &str, base: &str) -> AppResult<()> {
    let diff = backend.review_diff_text(base, head).await?;
    if diff.trim().is_empty() {
        println!("'{head}' has no changes against '{base}' — nothing to open a PR for.");
        return Ok(());
    }
    // gh's own template auto-fill is bypassed when `--body` is given, so the
    // template must be folded into our draft instead.
    let template = find_pr_template(backend.root());
    // The branch's own commit messages: the WHY the diff can't show. Best-effort
    // — no log just means less context for the draft.
    let commit_msgs = backend.review_log(base, head).await.unwrap_or_default();
    let commits = commit_msgs.join("\n\n");
    // CHANGELOG.md changes are the authoritative user-facing summary; extracted
    // separately so the diff cap can never truncate them away.
    let changelog = extract_changelog_diff(&diff);

    let edited = {
        let (mut tui, _guard) = ui::terminal::TerminalGuard::enter()?;
        // Fallback when drafting is skipped or fails: the branch name as a
        // minimal title, the branch's commit subjects as a starting outline,
        // plus the raw template (if any) to fill by hand.
        let mut fallback = head.to_string();
        let subjects = commit_subjects(&commit_msgs);
        if !subjects.is_empty() {
            fallback.push_str("\n\n");
            fallback.push_str(&subjects);
        }
        if let Some(t) = &template {
            fallback.push_str("\n\n");
            fallback.push_str(t);
        }
        let draft = ai_loop::draft_with_retry(
            &mut tui,
            backend.root(),
            ai_loop::Draft::Pr {
                diff: &diff,
                template: template.as_deref(),
                commits: &commits,
                changelog: changelog.as_deref(),
            },
            "Generating PR title and description…",
            &fallback,
        )
        .await?;
        let header = format!("Pull request — {head} → {base}  (first line = title)");
        ui::editor::run(&mut tui, &draft, &header)?
    };
    let Some(text) = edited else {
        println!("PR creation cancelled.");
        return Ok(());
    };
    let (title, body) = split_title_body(&text);
    if title.is_empty() {
        println!("Empty PR title — not creating a PR.");
        return Ok(());
    }

    let draft_pr = crate::prompt::confirm_no("Open it as a draft PR?")?;

    println!("Opening the PR page in your browser…");
    // Deliberately NOT processkit here: its kill-on-close job would also kill a
    // freshly-spawned browser (a descendant of `gh`) the moment `gh` exits. A
    // plain std spawn leaves the browser alive; `gh … --web` itself returns as
    // soon as the page is launched, so the brief blocking wait is fine.
    let mut argv = vec![
        "pr", "create", "--web", "--head", head, "--base", base, "--title", &title, "--body", &body,
    ];
    if draft_pr {
        argv.push("--draft");
    }
    let out = std::process::Command::new("gh")
        .args(&argv)
        .current_dir(backend.root())
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            let err = if err.is_empty() { "(no output)" } else { err };
            dim(&format!("could not open the PR page: {err}"));
        }
        Err(e) => dim(&format!("could not open the PR page: {e}")),
    }
    Ok(())
}

/// Parse the `gh pr list --json …` output. Malformed or empty input yields an
/// empty list.
fn parse_open_prs(json: &str) -> Vec<OpenPr> {
    serde_json::from_str(json).unwrap_or_default()
}

/// The `diff --git` section of the file whose basename is `CHANGELOG.md` (root
/// or nested), out of the combined branch diff. `None` when absent. Used to
/// give the changelog changes guaranteed, prioritized space in the AI prompt.
///
/// The file is identified by its `+++ b/<path>` line, not the `diff --git`
/// header — the header is ambiguous for unquoted paths with spaces, while the
/// `+++` line carries exactly one path (the same trick the toolkit's own diff
/// parser uses).
fn extract_changelog_diff(diff: &str) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            sections.push(String::new());
        }
        if let Some(s) = sections.last_mut() {
            s.push_str(line);
            s.push('\n');
        }
    }
    sections.into_iter().find(|s| {
        s.lines().any(|l| {
            l.strip_prefix("+++ b/")
                .is_some_and(|p| p == "CHANGELOG.md" || p.ends_with("/CHANGELOG.md"))
        })
    })
}

/// The subject (first) line of each commit message, as a `- ` list — the
/// manual-editing starting outline when the AI draft is unavailable.
fn commit_subjects(messages: &[String]) -> String {
    messages
        .iter()
        .filter_map(|m| m.lines().next())
        .map(|s| format!("- {s}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The repository's PR template, from the locations GitHub itself recognises
/// (`.github/`, the root, `docs/` — any case of `pull_request_template.md`),
/// capped to keep the AI prompt within bounds. `None` when there is none.
fn find_pr_template(root: &Path) -> Option<String> {
    const CAP: usize = 4096;
    for dir in [root.join(".github"), root.to_path_buf(), root.join("docs")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.eq_ignore_ascii_case("pull_request_template.md")
                && let Ok(text) = std::fs::read_to_string(entry.path())
            {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let mut end = trimmed.len().min(CAP);
                while !trimmed.is_char_boundary(end) {
                    end -= 1;
                }
                return Some(trimmed[..end].to_string());
            }
        }
    }
    None
}

/// Wrap `url` in an OSC 8 hyperlink (clickable in Windows Terminal and most
/// modern emulators). The visible text is the plain URL, so terminals without
/// OSC 8 support still show — and often still linkify — the address itself.
fn osc8(url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{url}\x1b]8;;\x1b\\")
}

/// Split the edited text into the PR title (first line) and body (the rest,
/// minus one separating blank line).
fn split_title_body(text: &str) -> (String, String) {
    let mut lines = text.lines();
    let title = lines.next().unwrap_or("").trim().to_string();
    let rest: Vec<&str> = lines.collect();
    let body = match rest.split_first() {
        Some((first, tail)) if first.trim().is_empty() => tail.join("\n"),
        _ => rest.join("\n"),
    };
    (title, body.trim_end().to_string())
}

/// The PR base to offer by default: the repo's default branch, or `main` when
/// GitHub reports none (an empty repository).
fn default_base(repo: &vcs_github::Repo) -> String {
    if repo.default_branch.is_empty() {
        "main".to_string()
    } else {
        repo.default_branch.clone()
    }
}

/// One dim parenthesized notice — the step's only voice when it skips itself.
fn dim(msg: &str) {
    eprintln!("\x1b[2m({msg})\x1b[0m");
}

/// Build an owned-arg vector from string literals.
fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc8_wraps_url_with_visible_fallback() {
        let url = "https://github.com/o/r/pull/12";
        assert_eq!(
            osc8(url),
            "\x1b]8;;https://github.com/o/r/pull/12\x1b\\https://github.com/o/r/pull/12\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn split_title_body_variants() {
        // Title only.
        assert_eq!(
            split_title_body("Add feature"),
            ("Add feature".into(), String::new())
        );
        // Blank-line separated body (the canonical shape) — separator dropped.
        assert_eq!(
            split_title_body("Add feature\n\nDoes things.\n- one\n"),
            ("Add feature".into(), "Does things.\n- one".into())
        );
        // No blank line — the rest is still the body.
        assert_eq!(
            split_title_body("Add feature\nDoes things."),
            ("Add feature".into(), "Does things.".into())
        );
        // CRLF input: `lines()` strips the `\r`.
        assert_eq!(
            split_title_body("Add feature\r\n\r\nBody."),
            ("Add feature".into(), "Body.".into())
        );
        // Empty input.
        assert_eq!(split_title_body(""), (String::new(), String::new()));
        // Title is trimmed.
        assert_eq!(
            split_title_body("  Add feature  "),
            ("Add feature".into(), String::new())
        );
    }

    #[test]
    fn parse_open_prs_accepts_gh_json_and_rejects_garbage() {
        let json = r#"[
            {"number": 12, "title": "Fix login", "baseRefName": "main",
             "url": "https://github.com/o/r/pull/12"},
            {"number": 34, "title": "Docs", "baseRefName": "develop",
             "url": "https://github.com/o/r/pull/34",
             "statusCheckRollup": [
                {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
                {"__typename": "StatusContext", "state": "PENDING"}
             ]}
        ]"#;
        let prs = parse_open_prs(json);
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 12);
        assert_eq!(prs[0].base, "main");
        // Absent rollup → empty checks (older gh payloads keep parsing).
        assert!(prs[0].checks.is_empty());
        assert_eq!(prs[1].title, "Docs");
        assert_eq!(prs[1].checks.len(), 2);
        // Empty list, malformed JSON, and wrong shapes all read as "no PRs".
        assert!(parse_open_prs("[]").is_empty());
        assert!(parse_open_prs("not json").is_empty());
        assert!(parse_open_prs(r#"{"number": 1}"#).is_empty());
    }

    #[test]
    fn checks_line_buckets_heterogeneous_rollup() {
        let check = |state: &str, status: &str, conclusion: &str| Check {
            state: state.into(),
            status: status.into(),
            conclusion: conclusion.into(),
        };
        // CheckRun finished OK / failed; unfinished CheckRun; StatusContexts.
        let checks = vec![
            check("", "COMPLETED", "SUCCESS"),
            check("", "COMPLETED", "FAILURE"),
            check("", "IN_PROGRESS", ""),
            check("SUCCESS", "", ""),
            check("PENDING", "", ""),
            check("", "COMPLETED", "SKIPPED"), // neutral-ish → pass bucket
        ];
        assert_eq!(checks_line(&checks).unwrap(), "checks: ✓3 ✗1 ●2");
        // Zero buckets are omitted; no checks at all → no line.
        assert_eq!(
            checks_line(&[check("", "COMPLETED", "SUCCESS")]).unwrap(),
            "checks: ✓1"
        );
        assert!(checks_line(&[]).is_none());
    }

    #[test]
    fn extracts_changelog_section_from_combined_diff() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n+code\n\
                    diff --git a/CHANGELOG.md b/CHANGELOG.md\nindex 1..2\n\
                    --- a/CHANGELOG.md\n+++ b/CHANGELOG.md\n+- new entry\n\
                    diff --git a/README.md b/README.md\n--- a/README.md\n\
                    +++ b/README.md\n+docs\n";
        let section = extract_changelog_diff(diff).unwrap();
        assert!(section.starts_with("diff --git a/CHANGELOG.md"));
        assert!(section.contains("+- new entry"));
        // The next file's section is excluded.
        assert!(!section.contains("README.md"));
        assert!(!section.contains("+code"));

        // Nested path matches by basename; lookalike names don't.
        let nested = "diff --git a/docs/CHANGELOG.md b/docs/CHANGELOG.md\n\
                      +++ b/docs/CHANGELOG.md\n+x\n";
        assert!(extract_changelog_diff(nested).is_some());
        let lookalike =
            "diff --git a/NOT_CHANGELOG.md b/NOT_CHANGELOG.md\n+++ b/NOT_CHANGELOG.md\n+x\n";
        assert!(extract_changelog_diff(lookalike).is_none());
        // A dir with spaces (unquoted in the header) is matched via `+++ b/`.
        let spaced = "diff --git a/my docs/CHANGELOG.md b/my docs/CHANGELOG.md\n\
                      +++ b/my docs/CHANGELOG.md\n+x\n";
        assert!(extract_changelog_diff(spaced).is_some());
        assert!(extract_changelog_diff("").is_none());
    }

    #[test]
    fn commit_subjects_lists_first_lines() {
        let msgs = vec![
            "feat: add picker\n\nLong body\nwith - a list line".to_string(),
            "fix: clamp scroll".to_string(),
        ];
        assert_eq!(
            commit_subjects(&msgs),
            "- feat: add picker\n- fix: clamp scroll"
        );
        assert_eq!(commit_subjects(&[]), "");
    }

    #[test]
    fn finds_pr_template_case_insensitively_with_github_dir_priority() {
        let root = std::env::temp_dir().join(format!("vcs-flow-prt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".github")).unwrap();
        // No template anywhere → None.
        assert!(find_pr_template(&root).is_none());
        // Root-level template (odd case) is found…
        std::fs::write(root.join("PULL_REQUEST_TEMPLATE.md"), "## Root").unwrap();
        assert_eq!(find_pr_template(&root).unwrap(), "## Root");
        // …but `.github/` wins when both exist (checked first).
        std::fs::write(
            root.join(".github").join("pull_request_template.md"),
            "## GH",
        )
        .unwrap();
        assert_eq!(find_pr_template(&root).unwrap(), "## GH");
        // Blank template counts as absent (falls through to the root one).
        std::fs::write(
            root.join(".github").join("pull_request_template.md"),
            "  \n",
        )
        .unwrap();
        assert_eq!(find_pr_template(&root).unwrap(), "## Root");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn retain_unreverted_drops_by_new_and_old_path() {
        use crate::model::{ChangeKind, FileChange};
        let mut snap = Snapshot {
            changes: vec![
                FileChange {
                    path: "a.rs".into(),
                    old_path: None,
                    kind: ChangeKind::Modified,
                },
                FileChange {
                    path: "new.rs".into(),
                    old_path: Some("old.rs".into()),
                    kind: ChangeKind::Renamed,
                },
                FileChange {
                    path: "keep.rs".into(),
                    old_path: None,
                    kind: ChangeKind::Added,
                },
            ],
            diffs: Default::default(),
            hunks: Default::default(),
        };
        // `selected_paths` emits both sides of a rename; either must match.
        retain_unreverted(&mut snap, &["a.rs".into(), "old.rs".into()]);
        let left: Vec<&str> = snap.changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(left, vec!["keep.rs"]);
    }
}
