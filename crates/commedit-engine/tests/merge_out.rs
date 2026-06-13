//! End-to-end: introduce an artificial merge above a commit, confirming plain git
//! sees the degenerate `merge(P, C)` with `C` as a side branch, content unchanged
//! and the tree clean.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::{history, CommitInfo};
use commedit_engine::repo::Repo;
use jj_lib::backend::CommitId;

fn head(repo: &Repo) -> CommitId {
    repo.head_commit_id().expect("head")
}

fn current(repo: &Repo) -> Vec<CommitInfo> {
    history(&repo.repo, &head(repo)).unwrap()
}

fn by<'a>(commits: &'a [CommitInfo], subject: &str) -> &'a CommitInfo {
    commits
        .iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("commit {subject:?} not found"))
}

/// The current children of `target` — the commits whose parent edge points at it,
/// i.e. the slot the new merge splices into (what the GTK side reads off the graph
/// boundaries).
fn children_of(commits: &[CommitInfo], target: &CommitId) -> Vec<CommitId> {
    commits
        .iter()
        .filter(|c| c.parents.contains(target))
        .map(|c| c.id.clone())
        .collect()
}

/// Subject of `rev` per plain git.
fn subject(dir: &std::path::Path, rev: &str) -> String {
    common::git(dir, &["log", "-1", "--format=%s", rev])
}

/// Assert plain git sees an ordinary attached-HEAD repo: HEAD symbolic on `main`,
/// object database intact.
fn assert_transparent(dir: &std::path::Path) {
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    common::git(dir, &["fsck"]);
}

#[test]
fn merge_out_mid_history() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "one\n", "first"),
            ("b.txt", "two\n", "second"),
            ("c.txt", "three\n", "third"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");

    // Merge out the middle commit "second" (parent "first").
    let commits = current(&repo);
    let target = by(&commits, "second").id.clone();
    let children = children_of(&commits, &target);
    let outcome = repo.merge_out_commit(&target, children).expect("merge out");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // The tip is still "third", now sitting on the new merge.
    assert_eq!(subject(dir, "HEAD"), "third");
    // HEAD~1 is the new merge with both parents.
    assert!(
        common::is_merge(dir, "HEAD~1"),
        "the introduced commit is a 2-parent merge"
    );
    assert_eq!(subject(dir, "HEAD~1"), "Merge \"second\"");
    // Parent order [P, C]: first parent P = "first" (mainline), second parent
    // C = "second" (the merged-out side branch).
    assert_eq!(subject(dir, "HEAD~1^1"), "first");
    assert_eq!(subject(dir, "HEAD~1^2"), "second");
    // The merge carries C's tree (no change of its own), and "third" is intact.
    assert_eq!(common::git(dir, &["show", "HEAD~1:b.txt"]), "two");
    assert_eq!(common::git(dir, &["show", "HEAD:c.txt"]), "three");

    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    assert_transparent(dir);
}

#[test]
fn merge_out_the_tip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let mut repo = Repo::open(dir).expect("open");

    // Merge out the tip "second": the merge becomes the new HEAD.
    let commits = current(&repo);
    let target = by(&commits, "second").id.clone();
    let children = children_of(&commits, &target); // empty — second is the tip
    assert!(children.is_empty());
    let outcome = repo.merge_out_commit(&target, children).expect("merge out");
    assert!(matches!(outcome, SaveOutcome::Clean));

    assert!(common::is_merge(dir, "HEAD"), "the new tip is a merge");
    assert_eq!(subject(dir, "HEAD"), "Merge \"second\"");
    assert_eq!(subject(dir, "HEAD^1"), "first");
    assert_eq!(subject(dir, "HEAD^2"), "second");
    assert_eq!(common::git(dir, &["show", "HEAD:b.txt"]), "two");

    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    assert_transparent(dir);
}

#[test]
fn merge_out_refuses_a_merge_and_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);
    let mut repo = Repo::open(dir).expect("open");

    let commits = current(&repo);
    // A merge commit has no single parent to fold out.
    let merge = by(&commits, "merge").id.clone();
    let err = repo
        .merge_out_commit(&merge, children_of(&commits, &merge))
        .expect_err("a merge cannot be merged out");
    assert!(err.to_string().contains("single-parent"), "{err}");

    // The root has no parent either.
    let base = by(&commits, "base").id.clone();
    let err = repo
        .merge_out_commit(&base, children_of(&commits, &base))
        .expect_err("the root cannot be merged out");
    assert!(err.to_string().contains("single-parent"), "{err}");
}

#[test]
fn merge_out_preserves_uncommitted_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let mut repo = Repo::open(dir).expect("open");

    // An uncommitted edit to a tracked file present before the merge-out.
    std::fs::write(dir.join("a.txt"), "one\nDIRTY\n").unwrap();

    let commits = current(&repo);
    let target = by(&commits, "second").id.clone();
    let children = children_of(&commits, &target);
    let outcome = repo.merge_out_commit(&target, children).expect("merge out");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // The edit survives on disk and stays uncommitted (unstaged).
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "one\nDIRTY\n"
    );
    let status = common::git(dir, &["status", "--porcelain"]);
    assert!(
        status.contains("a.txt"),
        "a.txt should be dirty: {status:?}"
    );
    assert!(common::is_merge(dir, "HEAD"), "the new tip is a merge");
    assert_transparent(dir);
}
