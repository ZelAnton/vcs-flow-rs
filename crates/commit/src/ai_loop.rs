//! The shared TUI loop around AI drafting: resolve the configured model, show
//! the animated busy frame while copilot runs (Esc skips and kills the
//! subprocess), and on "model unavailable" re-prompt for another model name,
//! persisting it once it has actually worked. Used by both the commit-message
//! draft (`main.rs`) and the PR title+description draft (`pr.rs`).

use std::path::Path;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};

use crate::ui::terminal::Tui;
use crate::{AppResult, ai, settings, ui};

/// What to draft — selects the `ai` entry point (and its prompt) per attempt.
pub enum Draft<'a> {
    /// A commit message from the selected diff, seeded with the existing description.
    Commit { diff: &'a str, existing: &'a str },
    /// A PR title (first line) + markdown body from the branch-vs-base diff.
    Pr { diff: &'a str },
}

impl Draft<'_> {
    async fn attempt(&self, model: &str) -> ai::Outcome {
        match self {
            Draft::Commit { diff, existing } => ai::generate(diff, existing, model).await,
            Draft::Pr { diff } => ai::generate_pr(diff, model).await,
        }
    }
}

/// Resolve the model, draft, and — if copilot rejects the model — let the user
/// enter another (saved back to the source that supplied the failing one, once
/// it works). Returns the draft, or `fallback` when generation is skipped (Esc),
/// fails, or the user cancels the model re-prompt. Never blocks the flow.
///
/// `root` is the repo root, where `settings` finds the per-repo override file.
pub async fn draft_with_retry(
    tui: &mut Tui,
    root: &Path,
    req: Draft<'_>,
    busy: &str,
    fallback: &str,
) -> AppResult<String> {
    // `source` is where the originally-configured model came from; a replacement
    // is saved back there so it isn't shadowed by a higher-precedence source.
    let (mut model, source) = settings::resolve_model(root);
    let mut entered = false; // whether the current model was typed by the user

    loop {
        let Some(outcome) = run_with_spinner(tui, &req, &model, busy).await? else {
            return Ok(fallback.to_string()); // Esc during generation
        };
        match outcome {
            ai::Outcome::Drafted(text) => {
                // Persist a newly-entered model only once it has actually worked.
                if entered {
                    let _ = settings::save_model(root, source, &model); // best-effort
                }
                return Ok(text);
            }
            ai::Outcome::ModelUnavailable => {
                let title = format!("Model \"{model}\" is unavailable — enter another:");
                match ui::input::run(tui, &title, "")? {
                    Some(next) => {
                        model = next;
                        entered = true;
                    }
                    None => return Ok(fallback.to_string()),
                }
            }
            ai::Outcome::Failed => return Ok(fallback.to_string()),
        }
    }
}

/// Draw the animated `busy` frame while one drafting attempt runs. `Ok(None)`
/// means the user pressed Esc to skip; dropping the future kills the copilot
/// subprocess (its job dies with the dropped handle).
async fn run_with_spinner(
    tui: &mut Tui,
    req: &Draft<'_>,
    model: &str,
    busy: &str,
) -> AppResult<Option<ai::Outcome>> {
    let mut generate = std::pin::pin!(req.attempt(model));
    let mut ticker = tokio::time::interval(Duration::from_millis(120));
    let mut tick: usize = 0;

    loop {
        tokio::select! {
            biased;
            outcome = &mut generate => return Ok(Some(outcome)),
            _ = ticker.tick() => {
                let glyph = ui::busy::SPINNER[tick % ui::busy::SPINNER.len()];
                ui::busy::frame(tui, glyph, busy)?;
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
