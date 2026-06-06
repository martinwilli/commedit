//! The git-level backstop that guarantees a rewrite only moves the checked-out
//! branch: whatever nudges an unrelated branch, `restore_unrelated_heads`
//! reverts it before the user sees it.

mod common;

use commedit_engine::transparency::{local_head_oids, restore_unrelated_heads};

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
