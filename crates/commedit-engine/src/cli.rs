//! Shared command-line argument parsing for the frontends.
//!
//! Both binaries take the same positional surface: an optional repository path
//! and an optional branch to edit (`commedit [PATH] [BRANCH]`). Keeping the
//! disambiguation here means the GTK app and the MCP server agree on it, and it
//! can be unit-tested headless.

use std::path::{Path, PathBuf};

/// Resolve commedit's positional arguments (everything after the binary name)
/// into a `(repo path, optional branch)` pair. The branch, when present, selects
/// which branch's history to edit; `None` edits the branch checked out in the
/// worktree (the default).
///
/// Accepted forms:
/// - no arguments → current directory, checked-out branch;
/// - one argument → a *path* when it names an existing directory, otherwise a
///   *branch* in the current directory (so `commedit feature` works from inside
///   the repo);
/// - two or more → `<path> <branch>` (extra arguments are ignored).
///
/// A lone argument that is both an existing directory and a branch name resolves
/// as the directory; pass it explicitly as `. <branch>` to edit such a branch.
pub fn parse_repo_and_branch(args: &[String]) -> (PathBuf, Option<String>) {
    match args {
        [] => (PathBuf::from("."), None),
        [one] => {
            if Path::new(one).is_dir() {
                (PathBuf::from(one), None)
            } else {
                (PathBuf::from("."), Some(one.clone()))
            }
        }
        [path, branch, ..] => (PathBuf::from(path), Some(branch.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_is_current_dir_no_branch() {
        assert_eq!(parse_repo_and_branch(&v(&[])), (PathBuf::from("."), None));
    }

    #[test]
    fn a_lone_directory_is_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().to_str().unwrap();
        assert_eq!(parse_repo_and_branch(&v(&[p])), (PathBuf::from(p), None));
    }

    #[test]
    fn a_lone_non_directory_is_a_branch_in_the_current_dir() {
        // A name that doesn't exist on disk is taken as a branch.
        assert_eq!(
            parse_repo_and_branch(&v(&["feature"])),
            (PathBuf::from("."), Some("feature".to_string()))
        );
    }

    #[test]
    fn two_args_are_path_then_branch() {
        assert_eq!(
            parse_repo_and_branch(&v(&["/repo", "feature"])),
            (PathBuf::from("/repo"), Some("feature".to_string()))
        );
    }
}
