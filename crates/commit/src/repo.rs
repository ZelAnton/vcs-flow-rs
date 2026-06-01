//! Locate the repository root and decide which backend drives it.
//!
//! Walks up from a starting directory looking for `.jj` or `.git`. `.jj` wins so
//! a colocated git+jj repo is driven through jj (the user's choice).

use std::path::{Path, PathBuf};

use crate::model::BackendKind;

/// A located repository: its root and the backend that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLocation {
    pub root: PathBuf,
    pub kind: BackendKind,
}

/// Search `start` and its ancestors for a repo marker. Returns `None` if neither
/// `.jj` nor `.git` is found anywhere up to the filesystem root.
pub fn locate(start: &Path) -> Option<RepoLocation> {
    let mut cur = start.to_path_buf();
    loop {
        // `.jj` is always a directory; `.git` may be a file (worktrees/submodules),
        // so probe existence rather than is_dir for it.
        if cur.join(".jj").is_dir() {
            return Some(RepoLocation {
                root: cur,
                kind: BackendKind::Jj,
            });
        }
        if cur.join(".git").exists() {
            return Some(RepoLocation {
                root: cur,
                kind: BackendKind::Git,
            });
        }
        if !cur.pop() {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway temp dir that cleans itself up. Uses the process id and a
    /// caller-supplied tag for a unique name (no `Math.random`/time needed).
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("vcsflow-repo-{}-{tag}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn jj_wins_over_git_when_colocated() {
        let s = Scratch::new("colo");
        std::fs::create_dir_all(s.0.join(".git")).unwrap();
        std::fs::create_dir_all(s.0.join(".jj")).unwrap();
        let found = locate(&s.0).unwrap();
        assert_eq!(found.kind, BackendKind::Jj);
        assert_eq!(found.root, s.0);
    }

    #[test]
    fn git_only_detected() {
        let s = Scratch::new("git");
        std::fs::create_dir_all(s.0.join(".git")).unwrap();
        assert_eq!(locate(&s.0).unwrap().kind, BackendKind::Git);
    }

    #[test]
    fn walks_up_from_subdir() {
        let s = Scratch::new("walk");
        std::fs::create_dir_all(s.0.join(".jj")).unwrap();
        let sub = s.0.join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        let found = locate(&sub).unwrap();
        assert_eq!(found.kind, BackendKind::Jj);
        assert_eq!(found.root, s.0);
    }

    #[test]
    fn none_outside_a_repo() {
        let s = Scratch::new("none");
        // A bare temp dir with no marker (and, in practice, no marker above it in
        // the temp tree) resolves to None.
        assert!(locate(&s.0).is_none());
    }
}
