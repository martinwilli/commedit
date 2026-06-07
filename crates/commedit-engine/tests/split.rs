//! End-to-end: split a historical commit into the edited diff plus an inserted
//! "Split of …" commit holding the original tree, confirming plain git sees both
//! commits and that the branch tip / descendants are left unchanged.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::{history, CommitInfo};
use commedit_engine::repo::Repo;

fn commit_named(repo: &Repo, subject: &str) -> CommitInfo {
    history(&repo.repo, &repo.head_commit_id().expect("head"))
        .unwrap()
        .into_iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("commit {subject:?} not found"))
}

#[test]
fn split_middle_commit_inserts_followup_and_preserves_descendants() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("f.txt", "v1\n", "first"),
            ("f.txt", "v2\n", "second"),
            ("g.txt", "g\n", "third"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");
    let target = commit_named(&repo, "second");

    // Rewrite "second" to leave f.txt edited; the inserted commit must restore the
    // original "v2" so the tip and "third" are untouched.
    let outcome = repo
        .split_commit(&target.id, &[("f.txt".to_string(), "v2-edited\n".to_string())])
        .expect("split");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // History gains a "Split of second" commit right after the edited one.
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["third", "Split of second", "second", "first"]
    );

    // The edited commit leaves f.txt edited; the split commit restores the
    // original; the tip is byte-for-byte what it was before the split.
    assert_eq!(common::git(dir, &["show", "HEAD~2:f.txt"]), "v2-edited"); // second (C')
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "v2"); //        Split of second (N)
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "v2"); //          third, unchanged
    assert_eq!(common::git(dir, &["show", "HEAD:g.txt"]), "g");

    // The inserted commit carries the original commit's author.
    assert_eq!(
        common::git(dir, &["show", "-s", "--format=%an <%ae>", "HEAD~1"]),
        "Tester <tester@example.com>"
    );

    // Transparency invariants hold.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn split_tip_commit_moves_branch_to_followup() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("f.txt", "a\n", "first"), ("f.txt", "a\nb\n", "second")],
    );
    let mut repo = Repo::open(dir).expect("open");
    let target = commit_named(&repo, "second"); // the branch tip

    // Edit the tip's diff to add an extra line; the inserted commit restores the
    // original tip content, so the branch tip stays "a\nb".
    let outcome = repo
        .split_commit(&target.id, &[("f.txt".to_string(), "a\nb\nc\n".to_string())])
        .expect("split");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // The branch tip is now the inserted commit, restoring the original tip tree.
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["Split of second", "second", "first"]
    );
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "a\nb"); //   Split of second (N)
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "a\nb\nc"); // second (C')

    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}
