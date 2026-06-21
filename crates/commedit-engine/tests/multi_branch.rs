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

/// Find the imported commit on the multi-branch DAG whose subject is `subject`.
fn find_commit(
    repo: &Repo,
    heads: &[jj_lib::backend::CommitId],
    subject: &str,
) -> jj_lib::backend::CommitId {
    let (union, _) = repo.history_multi(heads, 0, usize::MAX).expect("history");
    union
        .iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| {
            panic!(
                "no commit {subject:?} in {:?}",
                union.iter().map(|c| &c.subject).collect::<Vec<_>>()
            )
        })
        .id
        .clone()
}

/// Editing the editable set: open `main` (checked out) and `feature` (no
/// worktree) as one editable DAG. Rewriting a commit that lives **only** on
/// `feature` moves *that* branch's ref and rebases its descendants, while leaving
/// `main`, HEAD and the launch worktree untouched — the per-branch ref movement
/// the editable set delivers.
#[test]
fn editing_a_worktreeless_branch_moves_only_its_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let main_before = g(&["rev-parse", "main"]);
    let head_before = g(&["rev-parse", "HEAD"]);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");
    assert!(
        repo.is_worktree_bound(),
        "launch branch main is checked out"
    );

    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let heads = [head, feature_head];

    // Rewrite F (lives only on feature, no worktree).
    let f = find_commit(&repo, &heads, "F");
    repo.rewrite_message(&f, "F (edited)").expect("rewrite F");

    // feature carries the edit; main and the worktree are untouched.
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["F (edited)", "B", "A"],
        "feature ref moved to the rewritten F"
    );
    assert_eq!(g(&["rev-parse", "main"]), main_before, "main ref unmoved");
    assert_eq!(g(&["rev-parse", "HEAD"]), head_before, "HEAD frozen");
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C", "B", "A"],
        "the launch worktree branch (main) is untouched"
    );
    g(&["fsck", "--no-progress"]);
}

/// Rewriting a commit that is a **shared ancestor** of two editable branches
/// rewrites it for both: both bookmarks move and both branches' descendants
/// rebase. This is the inherent consequence of editing a unified DAG, and the
/// launch worktree (main) re-materializes since its tip moved.
#[test]
fn editing_a_shared_ancestor_moves_both_branches() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let heads = [head, feature_head];

    // B is the shared ancestor of main (A-B-C) and feature (A-B-F).
    let b = find_commit(&repo, &heads, "B");
    repo.rewrite_message(&b, "B (edited)").expect("rewrite B");

    assert_eq!(
        common::git_log_subjects_of(dir, "main"),
        vec!["C", "B (edited)", "A"],
        "main rebased onto the rewritten ancestor"
    );
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["F", "B (edited)", "A"],
        "feature rebased onto the same rewritten ancestor"
    );
    // The launch worktree tracks main, whose tip moved → HEAD follows.
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C", "B (edited)", "A"],
        "the launch worktree re-materialized onto main's new tip"
    );
    g(&["fsck", "--no-progress"]);
}

/// A 1-element editable set (`open_multi` with one branch) is byte-identical to
/// the classic `open_branch`: only that branch's ref moves, no sibling is
/// disturbed. Guards the singleton-equivalence the MCP relies on.
#[test]
fn a_singleton_set_behaves_like_classic_single_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let feature_before = g(&["rev-parse", "feature"]);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into()],
    )
    .expect("open singleton");

    let head = repo.head_commit_id().expect("head");
    let target = commedit_engine::history::history(&repo.repo, &head).expect("history")[0]
        .id
        .clone();
    repo.rewrite_message(&target, "C (edited)")
        .expect("rewrite");

    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C (edited)", "B", "A"],
        "main carries the edit"
    );
    assert_eq!(
        g(&["rev-parse", "feature"]),
        feature_before,
        "the unimported sibling feature is left exactly where it was"
    );
    g(&["fsck", "--no-progress"]);
}
