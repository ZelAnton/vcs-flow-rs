//! Persisted `commit` settings — currently just the AI model name.
//!
//! Resolved highest-precedence first:
//! 1. `COMMIT_AI_MODEL` env var (ad-hoc / CI),
//! 2. per-repo override `<root>/.vcs-flow-commit.toml` (kept out of version
//!    control — see [`ensure_excluded`] — so it's never committed or pushed),
//! 3. per-user `<config_dir>/vcs-flow/commit.toml` (applies to every repo),
//! 4. the built-in [`DEFAULT_MODEL`].
//!
//! When the user enters a replacement model (the configured one was unavailable),
//! it's saved back to the *source* that supplied the failing model — the per-repo
//! file if a repo override was in effect, otherwise the per-user file — so the new
//! value actually takes effect next run rather than being shadowed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default copilot model, matching the reference `commit.ps1`.
pub const DEFAULT_MODEL: &str = "gpt-5.4-mini";

/// Env var that overrides every settings file.
const MODEL_ENV: &str = "COMMIT_AI_MODEL";

/// Per-repo override file name (lives in the repo root, version-control-excluded).
const REPO_FILE: &str = ".vcs-flow-commit.toml";

/// Which source supplied a resolved model — used to save a replacement back to
/// the same place so it isn't immediately shadowed by a higher-precedence source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    Env,
    Repo,
    User,
    Default,
}

/// The on-disk settings shape. Extensible: unknown future keys would need their
/// own fields, but `model` is all we persist today.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// The copilot model name. `None` → fall through to the next source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// The user-level config path (`<config_dir>/vcs-flow/commit.toml`), or `None`
/// when the platform has no config dir.
pub fn user_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("vcs-flow").join("commit.toml"))
}

/// The per-repo override path (`<root>/.vcs-flow-commit.toml`).
pub fn repo_path(root: &Path) -> PathBuf {
    root.join(REPO_FILE)
}

/// Read and parse a settings file; missing file or parse error yields defaults
/// (best-effort — a malformed config must never block a commit).
fn read(path: &Path) -> Settings {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

/// Resolve the model to use for `root` and report which source it came from,
/// applying the precedence documented above. Side effect: if the repo override
/// file exists, best-effort ensure it's version-control-excluded so it can never
/// be committed or pushed.
pub fn resolve_model(root: &Path) -> (String, ModelSource) {
    let env = std::env::var(MODEL_ENV).ok();

    let repo_file = repo_path(root);
    let repo = if repo_file.exists() {
        ensure_excluded(root);
        read(&repo_file).model
    } else {
        None
    };

    let user = user_path().map(|p| read(&p)).and_then(|s| s.model);

    pick_model(env, repo, user)
}

/// Pure precedence: env → repo → user → [`DEFAULT_MODEL`]. Split out so the
/// ordering is unit-testable without touching the filesystem. A blank value at
/// one level is treated as absent and falls through to the next source.
fn pick_model(
    env: Option<String>,
    repo: Option<String>,
    user: Option<String>,
) -> (String, ModelSource) {
    for (value, source) in [
        (env, ModelSource::Env),
        (repo, ModelSource::Repo),
        (user, ModelSource::User),
    ] {
        if let Some(s) = value {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return (trimmed.to_string(), source);
            }
        }
    }
    (DEFAULT_MODEL.to_string(), ModelSource::Default)
}

/// Persist `model` to the source it should live in: the per-repo override file
/// when a repo override was in effect, otherwise the per-user file. (A model from
/// the env var can't be persisted into the env, so it's written to the user file
/// as the best available fallback — it will still be shadowed while the env var is
/// set, which is the caller's own choice.)
pub fn save_model(root: &Path, source: ModelSource, model: &str) -> std::io::Result<()> {
    match source {
        ModelSource::Repo => save_repo_model(root, model),
        ModelSource::Env | ModelSource::User | ModelSource::Default => save_user_model(model),
    }
}

/// Write `model` into the per-user settings file (only the `model` key is stored).
fn save_user_model(model: &str) -> std::io::Result<()> {
    let path =
        user_path().ok_or_else(|| std::io::Error::other("no user config directory available"))?;
    write_model(&path, model)
}

/// Write `model` into the per-repo override file, then ensure that file stays out
/// of version control.
fn save_repo_model(root: &Path, model: &str) -> std::io::Result<()> {
    write_model(&repo_path(root), model)?;
    ensure_excluded(root);
    Ok(())
}

/// Read `path`, set `model`, and write it back. Only the `model` key is persisted.
fn write_model(path: &Path, model: &str) -> std::io::Result<()> {
    let mut settings = read(path);
    settings.model = Some(model.to_string());
    let text = toml::to_string(&settings)
        .map_err(|e| std::io::Error::other(format!("serialise settings: {e}")))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)
}

/// Ensure the per-repo override file is ignored by version control so it can't be
/// committed or pushed. Colocated git repos get a `.git/info/exclude` entry (local,
/// never pushed; honoured by git and colocated jj alike). When `.git` is a file
/// (linked worktrees / submodules) or absent (a pure-jj repo), fall back to a
/// tracked `.gitignore` entry, which git and jj honour everywhere. Idempotent and
/// silent on failure — purely a safety net for a hand-authored override file.
fn ensure_excluded(root: &Path) {
    let git = root.join(".git");
    if git.is_dir() {
        let info = git.join("info");
        let _ = std::fs::create_dir_all(&info);
        append_line_once(&info.join("exclude"), REPO_FILE);
    } else {
        append_line_once(&root.join(".gitignore"), REPO_FILE);
    }
}

/// Append `line` to `path` (creating it if needed) unless it's already listed.
/// Idempotent; best-effort (a write failure just means the safety net didn't
/// apply this time).
fn append_line_once(path: &Path, line: &str) {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if current.lines().any(|l| l.trim() == line) {
        return;
    }
    let mut updated = current;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(line);
    updated.push('\n');
    let _ = std::fs::write(path, updated);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_env_beats_repo_beats_user_beats_default() {
        assert_eq!(
            pick_model(Some("e".into()), Some("r".into()), Some("u".into())),
            ("e".to_string(), ModelSource::Env)
        );
        assert_eq!(
            pick_model(None, Some("r".into()), Some("u".into())),
            ("r".to_string(), ModelSource::Repo)
        );
        assert_eq!(
            pick_model(None, None, Some("u".into())),
            ("u".to_string(), ModelSource::User)
        );
        assert_eq!(
            pick_model(None, None, None),
            (DEFAULT_MODEL.to_string(), ModelSource::Default)
        );
    }

    #[test]
    fn pick_model_ignores_blank_values() {
        assert_eq!(
            pick_model(Some("  ".into()), None, None),
            (DEFAULT_MODEL.to_string(), ModelSource::Default)
        );
        // A blank repo value falls through to the user source, not to default.
        assert_eq!(
            pick_model(None, Some("  ".into()), Some("u".into())),
            ("u".to_string(), ModelSource::User)
        );
    }

    #[test]
    fn settings_toml_round_trips() {
        let s = Settings {
            model: Some("gpt-5.2".into()),
        };
        let text = toml::to_string(&s).unwrap();
        assert!(text.contains("model = \"gpt-5.2\""));
        let back: Settings = toml::from_str(&text).unwrap();
        assert_eq!(back.model.as_deref(), Some("gpt-5.2"));
    }

    #[test]
    fn empty_settings_serialises_without_model_key() {
        let text = toml::to_string(&Settings::default()).unwrap();
        assert!(!text.contains("model"), "got: {text:?}");
        // And an empty/garbage file reads back as defaults.
        assert!(toml::from_str::<Settings>("").unwrap().model.is_none());
    }

    /// A unique temp dir per test, removed on completion.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vcs-flow-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ensure_excluded_uses_git_info_exclude_when_git_is_a_dir() {
        let dir = temp_dir("excl-git");
        let info = dir.join(".git").join("info");
        std::fs::create_dir_all(&info).unwrap();
        let exclude = info.join("exclude");
        std::fs::write(&exclude, "/target/\n").unwrap();

        ensure_excluded(&dir);
        ensure_excluded(&dir); // second call must not duplicate the entry

        let text = std::fs::read_to_string(&exclude).unwrap();
        let hits = text.lines().filter(|l| l.trim() == REPO_FILE).count();
        assert_eq!(hits, 1, "exclude file:\n{text}");
        assert!(text.contains("/target/"), "kept existing entries");
        // No stray .gitignore was created in the tree.
        assert!(!dir.join(".gitignore").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_excluded_falls_back_to_gitignore_without_git_dir() {
        // Pure-jj repo (no `.git`) or a worktree where `.git` is a file: the entry
        // must land in a tracked `.gitignore`, which jj honours everywhere.
        let dir = temp_dir("excl-jj");
        std::fs::write(dir.join(".git"), "gitdir: /elsewhere\n").unwrap(); // `.git` is a file

        ensure_excluded(&dir);
        ensure_excluded(&dir);

        let gitignore = dir.join(".gitignore");
        let text = std::fs::read_to_string(&gitignore).unwrap();
        let hits = text.lines().filter(|l| l.trim() == REPO_FILE).count();
        assert_eq!(hits, 1, ".gitignore:\n{text}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
