//! Editing the checked-out branch must leave *other* local branches exactly
//! where they are — even when they share the very commits being rewritten. This
//! is plain git's `commit --amend`/rebase behavior: amending `main` diverges a
//! sibling branch, it never silently drags it along.
//!
//! commedit gets this for free by importing only the checked-out branch into jj
//! (see `Repo::import_git`): a git ref jj never imported is invisible to jj's
//! export, so it cannot be moved or deleted. The companion `diverged_branch.rs`
//! covers the same invariant for a remote-tracking ref.

mod common;

use commedit_engine::history::history;
use commedit_engine::repo::Repo;

#[test]
fn rewriting_the_current_branch_leaves_a_sibling_branch_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );
    // A sibling branch sitting at the tip, sharing the whole chain with `main`.
    g(&["branch", "feature"]);
    let feature_before = g(&["rev-parse", "feature"]);

    let mut repo = Repo::open(dir).expect("open");
    let head = repo.head_commit_id().expect("head");
    // Rewrite a middle commit ("B"), an ancestor of both `main`'s tip and
    // `feature`. Its descendants are rebased, so `main` moves to a new tip.
    let target = history(&repo.repo, &head).expect("history")[1].id.clone();
    repo.rewrite_message(&target, "B (edited)")
        .expect("editing the current branch must work");

    // The current branch carries the edit through to plain git...
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C", "B (edited)", "A"],
        "git sees the rewritten history on main"
    );
    assert_ne!(
        g(&["rev-parse", "main"]),
        feature_before,
        "main moved off the old tip"
    );
    // ...while the sibling branch stays pinned to the original, un-rewritten
    // commit (it still has the old "B" as an ancestor — a clean divergence).
    assert_eq!(
        g(&["rev-parse", "feature"]),
        feature_before,
        "the sibling branch is left exactly where it was"
    );
    assert_eq!(
        common::git(dir, &["log", "--format=%s", "feature"])
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        vec!["C", "B", "A"],
        "the sibling branch keeps the original commits"
    );
    assert_eq!(
        g(&["symbolic-ref", "HEAD"]),
        "refs/heads/main",
        "HEAD stays attached to its branch"
    );
    g(&["fsck", "--no-progress"]);
}
