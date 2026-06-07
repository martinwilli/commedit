//! End-to-end: read a commit's file changes and rewrite a file's content in a
//! historical commit, confirming plain git sees the result.

mod common;

use commedit_engine::diff::{commit_changes, ChangeKind};
use commedit_engine::history::history;
use commedit_engine::repo::Repo;

fn second_commit_id(repo: &Repo) -> commedit_engine::history::CommitInfo {
    history(&repo.repo, &repo.head_commit_id().expect("head"))
        .unwrap()
        .into_iter()
        .find(|c| c.subject == "second")
        .expect("second commit")
}

#[test]
fn reports_file_changes_for_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("f.txt", "line1\nline2\n", "first"),
            ("f.txt", "line1\nCHANGED\n", "second"),
        ],
    );
    let repo = Repo::open(dir).expect("open");
    let target = history(&repo.repo, &repo.head_commit_id().expect("head"))
        .unwrap()
        .into_iter()
        .find(|c| c.subject == "second")
        .unwrap();

    let changes = commit_changes(&repo.repo, &target.id).expect("changes");
    let change = changes.iter().find(|c| c.path == "f.txt").expect("f.txt change");
    assert_eq!(change.kind, ChangeKind::Modified);
    assert_eq!(change.old_text.as_deref(), Some("line1\nline2\n"));
    assert_eq!(change.new_text.as_deref(), Some("line1\nCHANGED\n"));
    assert!(!change.is_binary);
}

#[test]
fn rewrites_file_content_in_middle_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("f.txt", "v1\n", "first"),
            ("f.txt", "v2\n", "second"),
            ("g.txt", "g\n", "third"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");
    let target = second_commit_id(&repo);

    // Edit the content the middle commit introduces.
    repo.rewrite_file(&target.id, "f.txt", "v2-edited\n")
        .expect("rewrite file");

    // The middle commit now leaves f.txt as "v2-edited", and the change carries
    // through to the tip (descendants rebased).
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "v2-edited");
    let subjects = common::git_log_subjects(dir);
    assert_eq!(subjects, vec!["third", "second", "first"]);

    // Transparency invariants hold.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn rewrites_several_files_in_one_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a1\n", "first")]);
    // The "second" commit changes a.txt and adds b.txt; that's the one we re-edit.
    std::fs::write(dir.join("a.txt"), "a2\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b2\n").unwrap();
    common::git(dir, &["add", "-A"]);
    common::git(dir, &["commit", "-qm", "second"]);
    common::git(dir, &["commit", "--allow-empty", "-qm", "third"]);

    let mut repo = Repo::open(dir).expect("open");
    let target = second_commit_id(&repo);

    // One Save touching both files -> one rewrite, descendants rebased.
    repo.rewrite_files(
        &target.id,
        &[
            ("a.txt".to_string(), "a2-edited\n".to_string()),
            ("b.txt".to_string(), "b2-edited\n".to_string()),
        ],
    )
    .expect("rewrite files");

    assert_eq!(common::git(dir, &["show", "HEAD:a.txt"]), "a2-edited");
    assert_eq!(common::git(dir, &["show", "HEAD:b.txt"]), "b2-edited");
    assert_eq!(common::git_log_subjects(dir), vec!["third", "second", "first"]);
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}
