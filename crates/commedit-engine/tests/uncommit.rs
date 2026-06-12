//! Drop a commit while keeping its changes as *uncommitted* edits in the working
//! tree (git's `reset --mixed`): `drop_keeping_changes` for an in-history commit,
//! and `restore_to_working_copy` for an already-abandoned (trashed) one.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::{history, CommitInfo};
use commedit_engine::repo::Repo;

fn subject(repo: &Repo, subject: &str) -> CommitInfo {
    history(&repo.repo, &repo.head_commit_id().expect("head"))
        .expect("history")
        .into_iter()
        .find(|c| c.subject == subject)
        .expect("commit present")
}

#[test]
fn drop_keeping_changes_on_tip_moves_changes_to_the_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("a.txt", "a\nsecond\n", "second"),
            ("a.txt", "a\nsecond\nthird\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let tip = subject(&repo, "third").id;
    let outcome = repo.drop_keeping_changes(&tip).expect("uncommit");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // History dropped the tip; the branch moved to its parent.
    assert_eq!(common::git_log_subjects(dir), vec!["second", "first"]);
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    // The dropped commit's change is now an *unstaged* worktree edit: the index
    // matches HEAD (second), the worktree holds third's content.
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M a.txt");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\nsecond\nthird\n"
    );
    // commedit sees it as one uncommitted-changes entry touching a.txt.
    let info = repo.working_copy_info().expect("dirty working copy");
    assert_eq!(info.changed_files, 1);
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn drop_keeping_changes_mid_history_with_an_independent_descendant() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let second = subject(&repo, "second").id;
    let outcome = repo.drop_keeping_changes(&second).expect("uncommit");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // "second" left history; its independent descendant "third" rebased onto
    // "first" cleanly. b.txt's addition is now an uncommitted change.
    assert_eq!(common::git_log_subjects(dir), vec!["third", "first"]);
    let entry = repo
        .working_copy_chain()
        .into_iter()
        .next()
        .expect("an uncommitted entry");
    assert!(entry.file_names.contains(&"b.txt".to_string()));
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b\n");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn restore_to_working_copy_applies_an_abandoned_commits_diff() {
    // Mirrors the GTK trash-row flow: drop a commit to the trash, then later pull
    // its changes back into the working tree (not into history).
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "1\n2\n3\n", "first"),
            ("a.txt", "1\nSECOND\n3\n", "second"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let second = subject(&repo, "second").id;
    repo.abandon_commit(&second).expect("drop to trash");
    assert_eq!(common::git_log_subjects(dir), vec!["first"]);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");

    // The dropped commit is now an orphan; restore its diff to the worktree.
    let outcome = repo.restore_to_working_copy(&second).expect("restore");
    assert!(matches!(outcome, SaveOutcome::Clean));
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["first"],
        "history unchanged"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M a.txt");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "1\nSECOND\n3\n"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn restore_to_working_copy_conflicts_on_an_overlapping_local_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "1\n2\n3\n", "first"),
            ("a.txt", "1\nSECOND\n3\n", "second"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let second = subject(&repo, "second").id;
    repo.abandon_commit(&second).expect("drop to trash");

    // A local edit that rewrites the same line the orphan's diff touches.
    std::fs::write(dir.join("a.txt"), "1\nLOCAL\n3\n").unwrap();

    let outcome = repo.restore_to_working_copy(&second).expect("restore");
    match outcome {
        SaveOutcome::Conflicts { commits } => {
            assert!(!commits.is_empty(), "the working copy entry is conflicted");
        }
        SaveOutcome::Clean => panic!("expected a conflict on the overlapping edit"),
    }
}
