//! End-to-end: editing the history of a branch that is *not* checked out in the
//! worktree. The session moves only that branch's ref; HEAD, the index and the
//! on-disk worktree stay frozen, and there is no working copy (working-copy
//! operations are refused). A branch that is live in *another* worktree is now
//! editable — commedit maps that worktree onto a jj workspace and keeps it in
//! sync (Phase 1b) — while a branch that doesn't exist is still refused up front.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::history;
use commedit_engine::index_cache::IndexCache;
use commedit_engine::repo::Repo;

/// `main` (A, B, C) checked out, plus a `feature` branch (A, B, C, D) that is
/// *not* checked out — the off-worktree editing target.
fn init_two_branch_repo(dir: &std::path::Path) {
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "A"),
            ("b.txt", "b\n", "B"),
            ("c.txt", "c\n", "C"),
        ],
    );
    common::git(dir, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("d.txt"), "d\n").unwrap();
    common::git(dir, &["add", "d.txt"]);
    common::git(dir, &["commit", "-q", "-m", "D"]);
    // Back to main: feature is now a branch we are not on.
    common::git(dir, &["checkout", "-q", "main"]);
}

fn rev(dir: &std::path::Path, r: &str) -> String {
    common::git(dir, &["rev-parse", r])
}

fn subjects(dir: &std::path::Path, r: &str) -> Vec<String> {
    common::git(dir, &["log", "--format=%s", r])
        .lines()
        .map(str::to_string)
        .collect()
}

fn open_feature(dir: &std::path::Path) -> Repo {
    Repo::open_branch(dir, IndexCache::Disabled, Some("feature"))
        .expect("open feature off-worktree")
}

fn commit_id(repo: &Repo, subject: &str) -> jj_lib::backend::CommitId {
    history(&repo.repo, &repo.head_commit_id().expect("head"))
        .expect("history")
        .into_iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("{subject} commit present"))
        .id
}

#[test]
fn rewriting_an_off_worktree_branch_moves_only_its_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_two_branch_repo(dir);

    let main_before = rev(dir, "main");
    let head_before = rev(dir, "HEAD");
    let feature_before = rev(dir, "feature");

    let mut repo = open_feature(dir);
    assert!(
        !repo.is_worktree_bound(),
        "editing a non-checked-out branch is an off-worktree session"
    );

    // History is the *feature* branch's, not the checked-out main's.
    assert_eq!(subjects(dir, "feature"), vec!["D", "C", "B", "A"]);
    let b = commit_id(&repo, "B");
    repo.rewrite_message(&b, "B (edited)").expect("rewrite");

    // feature's ref moved and shows the edit, with descendants rebased.
    assert_eq!(subjects(dir, "feature"), vec!["D", "C", "B (edited)", "A"]);
    assert_ne!(rev(dir, "feature"), feature_before, "feature ref moved");

    // Everything worktree-bound stayed frozen: main, HEAD, the index, the tree.
    assert_eq!(rev(dir, "main"), main_before, "main ref untouched");
    assert_eq!(rev(dir, "HEAD"), head_before, "HEAD untouched");
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main",
        "still on main"
    );
    assert_eq!(
        common::git(dir, &["status", "--porcelain"]),
        "",
        "worktree and index clean"
    );
    assert!(
        common::git_allow_failure(dir, &["diff", "--cached", "--quiet"]).0,
        "no staged changes"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn off_worktree_editing_leaves_a_detached_head_detached() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_two_branch_repo(dir);
    common::git(dir, &["checkout", "-q", "--detach"]);
    let head_before = rev(dir, "HEAD");

    let mut repo = open_feature(dir);
    assert!(!repo.is_worktree_bound());
    let b = commit_id(&repo, "B");
    repo.rewrite_message(&b, "B (edited)").expect("rewrite");

    assert_eq!(subjects(dir, "feature"), vec!["D", "C", "B (edited)", "A"]);
    // HEAD is still detached at the same commit it was before.
    assert!(
        !common::git_allow_failure(dir, &["symbolic-ref", "-q", "HEAD"]).0,
        "HEAD stays detached"
    );
    assert_eq!(rev(dir, "HEAD"), head_before, "detached HEAD did not move");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn working_copy_operations_are_refused_off_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_two_branch_repo(dir);
    let mut repo = open_feature(dir);

    // No working copy is presented for a branch that isn't checked out.
    assert!(repo.working_copy_info().is_none());
    assert!(repo.working_copy_chain().is_empty());

    let err = repo
        .commit_working_copy("x", None)
        .expect_err("commit_working_copy must be refused off-worktree");
    assert!(
        err.to_string().contains("not checked out"),
        "clear refusal message: {err}"
    );
    assert!(repo.drop_working_copy(None).is_err());
    assert!(repo
        .split_working_copy(None, &[("d.txt".to_string(), "d\n".to_string())])
        .is_err());
    assert!(repo
        .squash_working_copy_into(None, &commit_id(&repo, "B"), None)
        .is_err());
    // restore_to_working_copy (the trash "restore to working tree" button) also
    // writes the worktree, so it is refused too — the guard short-circuits before
    // it touches the source commit.
    let b = commit_id(&repo, "B");
    let err = match repo.restore_to_working_copy(&b) {
        Ok(_) => panic!("restore_to_working_copy must be refused off-worktree"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("not checked out"),
        "clear refusal message: {err}"
    );
}

#[test]
fn dropping_a_commit_off_worktree_rebases_only_the_target_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_two_branch_repo(dir);
    let main_before = rev(dir, "main");
    let head_before = rev(dir, "HEAD");

    let mut repo = open_feature(dir);
    let c = commit_id(&repo, "C");
    let outcome = repo.abandon_commit(&c).expect("drop C");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // C is gone from feature; D rebased onto B (distinct files, so clean).
    assert_eq!(subjects(dir, "feature"), vec!["D", "B", "A"]);
    assert_eq!(rev(dir, "main"), main_before, "main untouched");
    assert_eq!(rev(dir, "HEAD"), head_before, "HEAD untouched");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn session_changes_and_undo_track_the_off_worktree_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_two_branch_repo(dir);
    let feature_before = rev(dir, "feature");

    let mut repo = open_feature(dir);
    assert!(
        repo.session_changes().expect("session changes").is_empty(),
        "no changes before any edit"
    );

    // Edit B's file content; the delta flows up to the tip, so the session diff
    // (tip-now vs session-start tip) reflects it.
    let b = commit_id(&repo, "B");
    repo.rewrite_file(&b, "b.txt", "b changed\n")
        .expect("rewrite file");
    assert_ne!(rev(dir, "feature"), feature_before);
    let changes = repo.session_changes().expect("session changes");
    assert!(
        changes.iter().any(|c| c.path == "b.txt"),
        "session diff shows the edited file"
    );

    // Undo rolls the feature ref back; the worktree was never involved.
    repo.undo().expect("undo");
    assert_eq!(rev(dir, "feature"), feature_before, "feature restored");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
}

#[test]
fn editing_a_branch_live_in_another_worktree_syncs_that_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_two_branch_repo(dir);

    // Check `feature` out in a linked worktree, then edit its history from the main
    // checkout. Phase 1b maps that worktree onto a jj workspace, so the rewrite is
    // now allowed and keeps the linked worktree in sync (it was refused before).
    let wt_parent = tempfile::tempdir().unwrap();
    let wt = wt_parent.path().join("wt");
    common::git(dir, &["worktree", "add", wt.to_str().unwrap(), "feature"]);
    let main_before = rev(dir, "main");
    let head_before = rev(dir, "HEAD");

    let mut repo =
        Repo::open_branch(dir, IndexCache::Disabled, Some("feature")).expect("open feature");

    // Rewrite D's file content; feature's ref moves and the linked worktree is
    // re-materialized onto the new tip.
    let d = commit_id(&repo, "D");
    repo.rewrite_file(&d, "d.txt", "d rewritten\n")
        .expect("rewrite D");

    assert_eq!(subjects(dir, "feature"), vec!["D", "C", "B", "A"]);
    assert_eq!(
        std::fs::read_to_string(wt.join("d.txt")).unwrap(),
        "d rewritten\n",
        "the linked worktree's file follows the rewrite"
    );
    // The linked worktree's index matches its new tip → a clean status there.
    assert_eq!(
        common::git(&wt, &["status", "--porcelain"]),
        "",
        "the linked worktree's index was reset to the rewritten tip"
    );
    // The launch worktree (main) is untouched: ref, HEAD and a clean tree.
    assert_eq!(rev(dir, "main"), main_before, "main ref unmoved");
    assert_eq!(rev(dir, "HEAD"), head_before, "launch HEAD frozen");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");

    // Undo: feature's tip moves back, so the linked worktree re-materializes onto
    // the original content (its tip-moved gate fires in reverse, too).
    repo.undo().expect("undo");
    assert_eq!(
        std::fs::read_to_string(wt.join("d.txt")).unwrap(),
        "d\n",
        "undo restored the linked worktree's file"
    );
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn opening_a_nonexistent_branch_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_two_branch_repo(dir);

    let err = match Repo::open_branch(dir, IndexCache::Disabled, Some("nope")) {
        Ok(_) => panic!("a nonexistent branch must be refused"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("no local branch"),
        "clear error: {err}"
    );
}
