//! Shared data types: change kinds, the per-file change record, and the commit
//! target (a git branch or a jj bookmark).

/// Which kind of change a file underwent, relative to the commit base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// A single changed file, with its path relative to the repository root and
/// always using forward slashes (normalised on ingest so git and jj agree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    /// For a detected rename, the previous path. Committing the change must
    /// include this too, so the deletion of the old path lands with the new one
    /// (otherwise the rename becomes an orphaned add). `None` for non-renames.
    pub old_path: Option<String>,
    pub kind: ChangeKind,
}

/// Which VCS drives the repo. `.jj` present → [`BackendKind::Jj`] (also covers
/// colocated git+jj); otherwise `.git` → [`BackendKind::Git`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Git,
    Jj,
}

/// Where the commit will land: a git branch, or a jj bookmark (with the commit
/// id it currently points at, used for amend and for display).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Branch name (git) or bookmark name (jj). Empty string means "no bookmark"
    /// for the jj describe-only fallback.
    pub label: String,
    /// The commit the target currently points at (jj only); `None` for git.
    pub revision: Option<String>,
}
