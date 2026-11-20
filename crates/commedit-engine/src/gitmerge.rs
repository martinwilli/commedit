//! Rename-aware merges through `git merge-tree`.
//!
//! jj-lib merges trees purely by path, so a rebase across a file rename ends up
//! as a modify/delete conflict, where git's merge-ort follows the rename. The
//! session's jj git dir shares the object store with the user's repo (see
//! [`crate::transparency::init_shared_git_dir`]), so git can merge jj's commits
//! directly by id and writes its result tree where jj reads it.

use std::path::Path;
use std::process::Command;

use jj_lib::backend::{CommitId, TreeId};
use jj_lib::object_id::ObjectId as _;

/// Outcome of [`git_merge_tree`].
// Unused until the git re-merge pass in settle() consumes it.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GitMerge {
    /// git merged cleanly and wrote the result tree to the shared object store.
    Clean(TreeId),
    /// git found conflicts it can't settle on its own.
    Conflicted,
    /// git can't do this kind of merge at all: it is missing, or older than
    /// 2.40 and rejects `--merge-base` as a usage error. A property of the
    /// installation, so it holds for every later attempt too.
    Unavailable,
    /// This one merge failed (a bad input object, an unexpected exit or
    /// signal, unparsable output), which says nothing about the next one.
    Failed,
}

/// Three-way merge `ours` and `theirs` over `base` with `git merge-tree
/// --write-tree`, all three given as commits (tree-ish arguments need a newer
/// git than `--merge-base` itself does). Runs in `root`, the user's worktree,
/// so their rename settings (`merge.renames`, `merge.renameLimit`,
/// `merge.directoryRenames`) apply. Never fails: anything short of a clean
/// merge or a reported conflict is [`GitMerge::Unavailable`] or
/// [`GitMerge::Failed`].
// Unused until the git re-merge pass in settle() consumes it.
#[allow(dead_code)]
pub(crate) fn git_merge_tree(
    root: &Path,
    base: &CommitId,
    ours: &CommitId,
    theirs: &CommitId,
) -> GitMerge {
    let Ok(out) = Command::new("git")
        .current_dir(root)
        .args(["merge-tree", "--write-tree", "--no-messages"])
        .arg(format!("--merge-base={}", base.hex()))
        .arg(ours.hex())
        .arg(theirs.hex())
        .output()
    else {
        return GitMerge::Unavailable;
    };
    // Exit 0 is a clean merge, 1 a conflicted one. 129 is git's usage error, as
    // for an unknown option on an old git; anything else (128 for a bad object,
    // or death by a signal) is a failure of this run only.
    match out.status.code() {
        Some(0) => {}
        Some(1) => return GitMerge::Conflicted,
        Some(129) => return GitMerge::Unavailable,
        _ => return GitMerge::Failed,
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .and_then(|line| TreeId::try_from_hex(line.trim()))
        .map_or(GitMerge::Failed, GitMerge::Clean)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `git` in `dir` with a fixed identity, asserting success and returning
    /// trimmed stdout.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn head(dir: &Path) -> CommitId {
        CommitId::try_from_hex(git(dir, &["rev-parse", "HEAD"])).unwrap()
    }

    /// `seq 1 20` with line 10 replaced by `line10`.
    fn numbers(line10: &str) -> String {
        (1..=20)
            .map(|n| match n {
                10 => format!("{line10}\n"),
                n => format!("{n}\n"),
            })
            .collect()
    }

    /// A repo whose `main` holds `a.txt` as `seq 1 20`, returning the base commit.
    fn init(dir: &Path) -> CommitId {
        git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
        std::fs::write(dir.join("a.txt"), numbers("10")).unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", "base"]);
        head(dir)
    }

    /// Branch off `base` as `name`, set line 10 of `a.txt`, commit.
    fn edit_line10(dir: &Path, base: &CommitId, name: &str, line10: &str) -> CommitId {
        git(dir, &["checkout", "-q", "-b", name, &base.hex()]);
        std::fs::write(dir.join("a.txt"), numbers(line10)).unwrap();
        git(dir, &["commit", "-q", "-am", name]);
        head(dir)
    }

    #[test]
    fn a_rename_merges_with_an_edit_of_the_old_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let base = init(dir);
        git(dir, &["checkout", "-q", "-b", "rename"]);
        git(dir, &["mv", "a.txt", "b.txt"]);
        git(dir, &["commit", "-q", "-m", "rename"]);
        let renamed = head(dir);
        let edited = edit_line10(dir, &base, "edit", "ten");

        let GitMerge::Clean(tree) = git_merge_tree(dir, &base, &renamed, &edited) else {
            panic!("a rename plus an edit must merge clean");
        };
        let tree = tree.hex();
        assert_eq!(git(dir, &["ls-tree", "--name-only", &tree]), "b.txt");
        assert_eq!(
            git(dir, &["cat-file", "-p", &format!("{tree}:b.txt")]),
            numbers("ten").trim_end()
        );
    }

    #[test]
    fn overlapping_edits_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let base = init(dir);
        let ours = edit_line10(dir, &base, "ours", "ours");
        let theirs = edit_line10(dir, &base, "theirs", "theirs");

        assert_eq!(
            git_merge_tree(dir, &base, &ours, &theirs),
            GitMerge::Conflicted
        );
    }

    #[test]
    fn an_unknown_base_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let base = init(dir);
        let ours = edit_line10(dir, &base, "ours", "ours");
        let bogus = CommitId::try_from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();

        assert_eq!(git_merge_tree(dir, &bogus, &ours, &base), GitMerge::Failed);
    }
}
