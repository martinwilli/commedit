//! The git-level backstop that guarantees a rewrite only moves the checked-out
//! branch: whatever nudges an unrelated branch, `restore_unrelated_heads`
//! reverts it before the user sees it.

mod common;

use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use commedit_engine::transparency::{local_head_oids, restore_unrelated_heads};

/// jj's own refs — its `refs/jj/keep/*` GC anchors above all — must never appear
/// in the user's repository: not after a rewrite, and not even during a
/// browse-only session. jj writes its refs into a session-local git dir whose
/// object store alone is shared with the user's repo (see `Repo::init_detached`),
/// so the rewritten *objects* reach the user's ODB while the refs stay out. The
/// one branch ref jj moves is mirrored out explicitly (`bridge_branch_to_git`).
#[test]
fn jj_refs_never_appear_in_the_user_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );
    let assert_no_jj_refs = |when: &str| {
        assert_eq!(
            common::git(dir, &["for-each-ref", "--format=%(refname)", "refs/jj/"]),
            "",
            "refs/jj/* leaked into the user repo {when}"
        );
        assert!(!dir.join(".jj").exists(), ".jj leaked into the user repo {when}");
    };

    let mut repo = Repo::open(dir).expect("open");
    // Open snapshots the working copy into jj's @ (a keep-ref in jj's git dir);
    // the user's repo must still be clean of refs/jj on a browse-only session.
    assert_no_jj_refs("right after open (browse-only)");

    // A real rewrite: objects land in the user's shared ODB (git sees the new
    // history below) but still no refs/jj.
    let head = repo.head_commit_id().expect("head");
    let target = history(&repo.repo, &head)
        .expect("history")
        .into_iter()
        .find(|c| c.subject == "B")
        .expect("B present")
        .id;
    repo.rewrite_message(&target, "B (edited)").expect("rewrite");

    assert_eq!(common::git_log_subjects(dir), vec!["C", "B (edited)", "A"]);
    assert_no_jj_refs("after a rewrite");
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    common::git(dir, &["fsck", "--no-progress"]);

    // Nothing persists once the session closes (jj's temp git dir is removed).
    drop(repo);
    assert_no_jj_refs("after the session closed");
}

#[test]
fn restores_an_unrelated_branch_but_leaves_the_current_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );
    g(&["branch", "backup"]); // at the tip, like main

    // Before-image: both branches at C.
    let before = local_head_oids(dir);
    let tip = g(&["rev-parse", "main"]);

    // Simulate the leak the backstop exists to undo: an unrelated branch
    // dragged back, *and* a legitimate move of the current branch.
    g(&["update-ref", "refs/heads/backup", "main~1"]);
    g(&["update-ref", "refs/heads/main", "main~2"]);

    let restored = restore_unrelated_heads(dir, Some("refs/heads/main"), &before);

    // Only the unrelated branch is reverted; the current branch keeps its move.
    assert_eq!(restored, vec!["refs/heads/backup".to_string()]);
    assert_eq!(g(&["rev-parse", "backup"]), tip, "backup restored to its tip");
    assert_ne!(g(&["rev-parse", "main"]), tip, "current branch left alone");
}

#[test]
fn recreates_a_branch_that_was_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    g(&["branch", "backup"]);
    let before = local_head_oids(dir);
    let tip = g(&["rev-parse", "backup"]);

    g(&["branch", "-D", "backup"]);
    let restored = restore_unrelated_heads(dir, Some("refs/heads/main"), &before);

    assert_eq!(restored, vec!["refs/heads/backup".to_string()]);
    assert_eq!(g(&["rev-parse", "backup"]), tip, "deleted branch recreated");
}

#[test]
fn no_op_when_nothing_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(dir, &[("a.txt", "a\n", "A")]);
    g(&["branch", "backup"]);
    let before = local_head_oids(dir);

    let restored = restore_unrelated_heads(dir, Some("refs/heads/main"), &before);
    assert!(restored.is_empty(), "untouched branches need no restoring");
}
