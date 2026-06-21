//! The multi-branch DAG read: `Repo::history_multi` walks the union of several
//! branches' ancestries, and `Repo::local_branches` enumerates the dropdown
//! candidates. The extra branches are read-only — folding one into the view must
//! not entangle it in a later rewrite of the edited branch (the `sibling_branch`
//! invariant still holds), because the extra heads are made index-visible only in
//! a transient transaction that is rolled back.

mod common;

use commedit_engine::repo::Repo;

/// Build `main: A-B-C` (checked out) with `feature` branching off `B` and adding
/// its own commit `F`, then return the opened repo.
fn setup(dir: &std::path::Path) {
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "A"),
            ("b.txt", "b\n", "B"),
            ("c.txt", "c\n", "C"),
        ],
    );
    g(&["checkout", "-q", "-b", "feature", "HEAD~1"]); // at B
    std::fs::write(dir.join("f.txt"), "f\n").unwrap();
    g(&["add", "f.txt"]);
    g(&["commit", "-q", "-m", "F"]);
    g(&["checkout", "-q", "main"]); // import `main`, not `feature`
}

#[test]
fn history_multi_unions_branch_ancestries() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);

    let repo = Repo::open(dir).expect("open");

    // local_branches lists both, flags the edited one, and carries readable tips.
    let branches = repo.local_branches();
    let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
    assert!(
        names.contains(&"main") && names.contains(&"feature"),
        "{names:?}"
    );
    let feature = branches.iter().find(|b| b.name == "feature").unwrap();
    let main = branches.iter().find(|b| b.name == "main").unwrap();
    assert!(main.is_current, "main is the edited branch");
    assert!(!feature.is_current, "feature is a view-only extra branch");

    // Single-head view: only main's chain.
    let head = repo.head_commit_id().expect("head");
    let (main_only, _) = repo
        .history_multi(std::slice::from_ref(&head), 0, usize::MAX)
        .unwrap();
    let mut subj: Vec<_> = main_only.iter().map(|c| c.subject.clone()).collect();
    subj.sort();
    assert_eq!(subj, vec!["A", "B", "C"], "single head walks only main");

    // Multi-head view: union of main and feature — F (only on feature) appears,
    // and a bare unimported-head walk would have failed without the transient
    // add_head, so reaching here at all proves the fix works.
    let (union, has_more) = repo
        .history_multi(&[head, feature.head.clone()], 0, usize::MAX)
        .unwrap();
    let mut subj: Vec<_> = union.iter().map(|c| c.subject.clone()).collect();
    subj.sort();
    assert_eq!(subj, vec!["A", "B", "C", "F"], "union of both branches");
    assert!(!has_more, "whole history fit under the limit");
}

#[test]
fn multi_head_read_does_not_entangle_a_later_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let feature_before = g(&["rev-parse", "feature"]);

    let mut repo = Repo::open(dir).expect("open");
    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;

    // Fold feature into the view (the transient add_head walk)...
    let (union, _) = repo
        .history_multi(&[head, feature_head], 0, usize::MAX)
        .unwrap();
    assert_eq!(union.len(), 4);

    // ...then rewrite a commit on the edited branch. `B` is a shared ancestor of
    // both main and feature; rewriting it must rebase main's descendants but
    // leave feature pinned, exactly as if it had never been folded in.
    let head = repo.head_commit_id().expect("head");
    let target = commedit_engine::history::history(&repo.repo, &head).expect("history")[1]
        .id
        .clone();
    repo.rewrite_message(&target, "B (edited)")
        .expect("rewrite");

    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C", "B (edited)", "A"],
        "main carries the edit"
    );
    assert_eq!(
        g(&["rev-parse", "feature"]),
        feature_before,
        "feature is left exactly where it was — the multi-head read left no residue"
    );
    g(&["fsck", "--no-progress"]);
}
