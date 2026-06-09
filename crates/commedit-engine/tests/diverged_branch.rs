//! A branch that has diverged from its upstream is an ordinary git state that
//! plain git rewrites freely (`rebase -i`, `commit --amend`). commedit must too:
//! it only ever edits the checked-out *local* branch and walks HEAD's ancestors,
//! so it imports with `auto_local_bookmark: false` and never lets a divergent
//! `origin/*` ref conflate the local bookmark into a conflicted (unexportable)
//! one. Regression for the `../carlos` "edit has no effect / now errors" report.

mod common;

use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use std::path::Path;

/// Build a repo on `main` (A, B, C) whose `origin/main` remote-tracking ref has
/// diverged: it points at a commit D that branched off B, so neither C nor D is
/// an ancestor of the other — `git status` would report the branch "diverged".
fn init_diverged_repo(dir: &Path) {
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );
    g(&["checkout", "-q", "-b", "tmp", "main~1"]);
    std::fs::write(dir.join("d.txt"), "d\n").unwrap();
    g(&["add", "d.txt"]);
    g(&["commit", "-q", "-m", "D"]);
    let d = g(&["rev-parse", "HEAD"]);
    g(&["checkout", "-q", "main"]);
    g(&["update-ref", "refs/remotes/origin/main", &d]);
    g(&["branch", "-D", "tmp"]);
}

#[test]
fn edits_a_branch_that_has_diverged_from_its_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_diverged_repo(dir);
    let origin_before = common::git(dir, &["rev-parse", "refs/remotes/origin/main"]);

    let mut repo = Repo::open(dir).expect("open");
    let head = repo.head_commit_id().expect("head");
    // Target the HEAD commit ("C"), exactly the reported scenario.
    let target = history(&repo.repo, &head).expect("history")[0].id.clone();

    // The edit must succeed and reach plain git — not silently no-op, not error.
    repo.rewrite_message(&target, "C (edited)")
        .expect("editing a diverged branch must work like plain git");

    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C (edited)", "B", "A"],
        "git sees the rewritten history"
    );
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main",
        "HEAD stays attached to its branch"
    );
    assert_eq!(
        common::git(dir, &["rev-parse", "refs/remotes/origin/main"]),
        origin_before,
        "the divergent remote-tracking ref is left untouched"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}
