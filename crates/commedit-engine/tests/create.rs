//! End-to-end: create brand-new commits (from content or as a revert), commit
//! the working copy, and delete files, confirming plain git sees the result.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::{history, CommitInfo};
use commedit_engine::repo::Repo;
use commedit_engine::tree::FileEdit;
use jj_lib::backend::CommitId;

fn head(repo: &Repo) -> CommitId {
    repo.head_commit_id().expect("head")
}

fn commit_named(repo: &Repo, subject: &str) -> CommitInfo {
    history(&repo.repo, &head(repo))
        .unwrap()
        .into_iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("commit {subject:?} not found"))
}

#[test]
fn create_commit_on_top_of_head() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let mut repo = Repo::open(dir).expect("open");

    let outcome = repo
        .create_commit(
            vec![head(&repo)],
            vec![],
            "third",
            None,
            &[FileEdit::write("c.txt".into(), "three\n".into())],
        )
        .expect("create");
    assert!(matches!(outcome, SaveOutcome::Clean));

    assert_eq!(common::git_log_subjects(dir), ["third", "second", "first"]);
    assert_eq!(common::git(dir, &["show", "HEAD:c.txt"]), "three");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
}

#[test]
fn create_commit_can_delete_a_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let mut repo = Repo::open(dir).expect("open");

    let outcome = repo
        .create_commit(
            vec![head(&repo)],
            vec![],
            "drop a",
            None,
            &[FileEdit::delete("a.txt".into())],
        )
        .expect("create");
    assert!(matches!(outcome, SaveOutcome::Clean));

    assert_eq!(
        common::git(dir, &["ls-tree", "-r", "--name-only", "HEAD"]),
        "b.txt"
    );
    assert!(!dir.join("a.txt").exists());
}

#[test]
fn revert_commit_on_top_inverts_the_change() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "one\n", "first"),
            ("a.txt", "one\ntwo\n", "second"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");

    let second = commit_named(&repo, "second");
    let outcome = repo
        .revert_commit(&second.id, vec![head(&repo)], vec![], None)
        .expect("revert");
    assert!(matches!(outcome, SaveOutcome::Clean));

    assert_eq!(common::git(dir, &["show", "HEAD:a.txt"]), "one");
    assert_eq!(common::git_log_subjects(dir)[0], "Revert \"second\"");
}

#[test]
fn commit_working_copy_crystallizes_the_dirt() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")],
    );
    let mut repo = Repo::open(dir).expect("open");

    std::fs::write(dir.join("a.txt"), "1\nlocal\n").unwrap();
    let outcome = repo
        .commit_working_copy("local work", None)
        .expect("commit wc");
    assert!(matches!(outcome, SaveOutcome::Clean));

    assert_eq!(
        common::git_log_subjects(dir),
        ["local work", "second", "first"]
    );
    assert_eq!(common::git(dir, &["show", "HEAD:a.txt"]), "1\nlocal");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    // The tree is clean now: nothing left to commit.
    assert!(repo.working_copy_info().is_none());
}
