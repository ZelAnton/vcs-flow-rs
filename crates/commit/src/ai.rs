//! AI drafting via the GitHub Copilot CLI (`copilot`): commit messages
//! ([`generate`]) and PR titles + descriptions ([`generate_pr`]).
//!
//! The diff (plus, for commits, any existing jj change description as context)
//! is handed to `copilot -p … -s` in non-interactive mode; its stdout becomes
//! the editor's pre-fill. The result is an [`Outcome`] so the caller can
//! distinguish "the chosen model isn't available" (worth re-prompting for another)
//! from any other failure (silently fall back) — a commit or PR is never blocked.

use std::time::Duration;

use processkit::Command;

/// Cap the diff handed to copilot. A commit message captures intent, not every
/// line; this also keeps the prompt well under the OS command-line limit (the
/// diff travels as a `-p` argument). Mirrors the reference `commit.ps1`.
const DIFF_LIMIT: usize = 8000;

/// Cap the existing-description context. It's only a hint for the model, and
/// bounding it keeps the whole `-p` argument under the OS command-line limit
/// regardless of how long a prior jj description grew.
const CONTEXT_LIMIT: usize = 2000;

/// Kill the copilot run if it outstays this — the TUI is paused on the "busy"
/// screen meanwhile, so an unbounded hang would strand the user. The reference
/// script had no timeout; the TUI context warrants one.
const TIMEOUT: Duration = Duration::from_secs(45);

/// The outcome of a generation attempt.
pub enum Outcome {
    /// A usable commit message.
    Drafted(String),
    /// Copilot rejected the requested model — the caller can ask for another.
    ModelUnavailable,
    /// Anything else (CLI missing, auth/network error, timeout, empty output) —
    /// the caller falls back to the existing description.
    Failed,
}

/// The instruction block prepended to the diff. Lifted from the reference
/// `commit.ps1` so both tools draft in the same voice.
const PROMPT_PREAMBLE: &str = "\
Write a git commit message for this diff. Describe the PURPOSE and ESSENCE of the changes — what was done and why.
NEVER list file names or paths. NEVER enumerate changes file-by-file.
First line: imperative mood, max 72 chars.
If more context is needed, add a body after a blank line.
Output ONLY the raw commit message text. No markdown, no quotes, no prefixes.";

/// The instruction block for a PR title + description. Unlike the commit
/// preamble it *invites* markdown — that's what the PR body renders as.
const PR_PROMPT_PREAMBLE: &str = "\
Write a GitHub pull request title and description for this branch diff.
First line: a concise title in imperative mood, max 72 chars.
Then a blank line, then a markdown description: summarize WHAT changed and WHY,
with bullet points for the notable changes. NEVER enumerate every file.
Output ONLY the title and the description. Do not wrap the answer in code fences.";

/// Draft a commit message from `diff` with `model`, optionally seeded with the
/// `existing` description (the jj `@` change description) as context.
pub async fn generate(diff: &str, existing: &str, model: &str) -> Outcome {
    run_copilot(&build_commit_prompt(diff, existing), model).await
}

/// Draft a PR title (first line) + markdown body from the branch-vs-base `diff`.
pub async fn generate_pr(diff: &str, model: &str) -> Outcome {
    run_copilot(&build_pr_prompt(diff), model).await
}

/// Run one copilot attempt for `prompt` and classify the result.
async fn run_copilot(prompt: &str, model: &str) -> Outcome {
    // No `--allow-all-tools`: this is pure text generation; copilot needs no
    // file or tool access. `output_string` captures output without raising on a
    // non-zero exit, so we can classify the failure ourselves. `--model=<v>` (not
    // `--model <v>`) so a model name starting with `-` can't be reparsed as a flag.
    let result = Command::new("copilot")
        .args([
            "-p",
            prompt,
            "-s",
            "--no-auto-update",
            "--effort",
            "medium",
            &format!("--model={model}"),
        ])
        .timeout(TIMEOUT)
        .output_string()
        .await;

    // A spawn failure (e.g. copilot not on PATH) is the only `Err`; a timeout
    // comes back as `Ok` with `timed_out()` set (and no exit code), so it must be
    // checked explicitly before the stderr-based classification below.
    let result = match result {
        Ok(r) => r,
        Err(_) => return Outcome::Failed,
    };
    if result.timed_out() {
        return Outcome::Failed;
    }

    if !result.is_success() {
        return if is_model_unavailable(result.stderr()) {
            Outcome::ModelUnavailable
        } else {
            Outcome::Failed
        };
    }
    match sanitize(result.stdout()) {
        Some(msg) => Outcome::Drafted(msg),
        None => Outcome::Failed,
    }
}

/// Whether copilot's stderr says the requested *model* isn't available. Copilot
/// emits `Error: Model "X" from --model flag is not available.` — require both
/// "model" and "not available" so we don't re-prompt on unrelated failures (a
/// service/feature being "not available", auth, or network errors).
fn is_model_unavailable(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("model") && lower.contains("not available")
}

/// Assemble the commit-message prompt: preamble, the existing description as
/// context (when present), then the (truncated) diff.
fn build_commit_prompt(diff: &str, existing: &str) -> String {
    let mut prompt = String::from(PROMPT_PREAMBLE);
    let existing = existing.trim();
    if !existing.is_empty() {
        prompt.push_str(
            "\n\nCurrent draft description (improve or replace it — do not just echo it):\n",
        );
        prompt.push_str(&truncate(existing, CONTEXT_LIMIT));
    }
    prompt.push_str("\n\n");
    prompt.push_str(&truncate(diff, DIFF_LIMIT));
    prompt
}

/// Assemble the PR title+description prompt: preamble, then the (truncated) diff.
fn build_pr_prompt(diff: &str) -> String {
    format!("{PR_PROMPT_PREAMBLE}\n\n{}", truncate(diff, DIFF_LIMIT))
}

/// Truncate on a char boundary at or below `limit`, appending a marker so the
/// model knows the diff was cut.
fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... (truncated)", &text[..end])
}

/// Clean copilot's output: drop any `Co-authored-by:` trailer it appends and
/// trim surrounding whitespace. `None` if nothing meaningful remains.
fn sanitize(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .lines()
        .filter(|line| {
            !line
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("co-authored-by:")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_existing_description_as_context() {
        let p = build_commit_prompt("diff body", "  WIP: refactor parser  ");
        assert!(p.starts_with(PROMPT_PREAMBLE));
        assert!(p.contains("Current draft description"));
        assert!(p.contains("WIP: refactor parser"));
        assert!(p.trim_end().ends_with("diff body"));
    }

    #[test]
    fn prompt_omits_description_block_when_empty() {
        let p = build_commit_prompt("diff body", "   ");
        assert!(!p.contains("Current draft description"));
        assert!(p.ends_with("diff body"));
    }

    #[test]
    fn pr_prompt_is_preamble_plus_truncated_diff() {
        let p = build_pr_prompt("diff body");
        assert!(p.starts_with(PR_PROMPT_PREAMBLE));
        assert!(p.ends_with("diff body"));
        // No commit-only context block sneaks in.
        assert!(!p.contains("Current draft description"));
        // The diff cap applies here too.
        let long = "x".repeat(DIFF_LIMIT + 100);
        assert!(build_pr_prompt(&long).ends_with("... (truncated)"));
    }

    #[test]
    fn truncate_marks_cut_and_respects_char_boundary() {
        let short = "abc";
        assert_eq!(truncate(short, 8000), "abc");
        // A multi-byte char straddling the limit must not be split.
        let text = "é".repeat(20); // 40 bytes
        let out = truncate(&text, 9);
        assert!(out.ends_with("... (truncated)"));
        assert!(out.starts_with("éééé")); // 4 chars = 8 bytes, the 9th byte is mid-char
    }

    #[test]
    fn sanitize_strips_coauthor_trailer_and_trims() {
        let raw = "Fix the parser bug\n\nHandle empty input.\nCo-authored-by: Copilot <x@y.z>\n";
        assert_eq!(
            sanitize(raw).unwrap(),
            "Fix the parser bug\n\nHandle empty input."
        );
    }

    #[test]
    fn sanitize_returns_none_for_blank() {
        assert!(sanitize("   \n  ").is_none());
        assert!(sanitize("Co-authored-by: only <a@b.c>").is_none());
    }

    #[test]
    fn detects_model_unavailable_but_not_generic_errors() {
        // The real copilot message for a bad `--model`.
        assert!(is_model_unavailable(
            "Error: Model \"bogus\" from --model flag is not available."
        ));
        assert!(!is_model_unavailable("Error: authentication required"));
        assert!(!is_model_unavailable("Error: network unreachable"));
        // "not available" about something other than the model must not match.
        assert!(!is_model_unavailable("Error: service is not available"));
    }

    /// Spawns the real `copilot` CLI, so it's `#[ignore]`d (project convention) —
    /// run with `cargo test -p vcs-flow-commit -- --ignored`. Requires the Copilot
    /// CLI installed and authenticated.
    #[tokio::test]
    #[ignore = "spawns the real copilot CLI"]
    async fn generate_drafts_a_message() {
        let diff = "diff --git a/greet.rs b/greet.rs\n\
            --- a/greet.rs\n+++ b/greet.rs\n@@ -1 +1 @@\n\
            -fn greet() { println!(\"hi\"); }\n\
            +fn greet(name: &str) { println!(\"hi {name}\"); }\n";
        let Outcome::Drafted(msg) = generate(diff, "", crate::settings::DEFAULT_MODEL).await else {
            panic!("copilot did not draft a message");
        };
        assert!(!msg.trim().is_empty());
        assert!(!msg.to_ascii_lowercase().contains("co-authored-by:"));
    }
}
