//! End-to-end: create brand-new commits (from content or as a revert), commit
//! the working copy, and delete files, confirming plain git sees the result.

mod common;

use commedit_engine::conflict::{FileResolution, SaveOutcome};
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
fn a_modify_delete_conflict_resolves_by_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // x.txt is added then modified, so reverting its addition wants to delete a
    // file whose content has since diverged — a modify/delete conflict.
    common::init_repo(
        dir,
        &[
            ("x.txt", "foo\n", "add x"),
            ("x.txt", "foo\nbar\n", "modify x"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");

    let add_x = commit_named(&repo, "add x");
    let outcome = repo
        .revert_commit(&add_x.id, vec![head(&repo)], vec![], None)
        .expect("revert");
    let SaveOutcome::Conflicts { commits } = outcome else {
        panic!("expected a modify/delete conflict");
    };

    // Resolve by deleting the path — the resolution edited content can't express
    // (it would leave a 0-byte file). No read_conflict / marker_len needed.
    let oldest = commits.into_iter().next().expect("a conflicted commit");
    let path = oldest.files[0].path_str();
    let outcome = repo
        .resolve_conflicts_ext(&oldest.change_id_hex(), &[(path, FileResolution::Delete)])
        .expect("resolve by delete");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // x.txt is gone from the tree (here the only file, so the tree is empty) and
    // from disk — not left behind empty.
    assert_eq!(
        common::git(dir, &["ls-tree", "-r", "--name-only", "HEAD"]),
        ""
    );
    assert!(!dir.join("x.txt").exists(), "x.txt deleted from disk");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
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
