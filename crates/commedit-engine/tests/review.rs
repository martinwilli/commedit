//! `Repo::session_changes` — the content delta the read-only "Review" view
//! shows: the current tree against the one the session started with.

mod common;

use commedit_engine::diff::ChangeKind;
use commedit_engine::history::history;
use commedit_engine::repo::Repo;

#[test]
fn untouched_session_has_no_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );

    let mut repo = Repo::open(dir).expect("open");
    assert!(
        repo.session_changes().expect("session changes").is_empty(),
        "a session with no edits has an empty review"
    );
}

#[test]
fn working_tree_edits_show_up_as_content_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );

    let mut repo = Repo::open(dir).expect("open");

    // Uncommitted on-disk state: edit a tracked file and add an untracked one.
    std::fs::write(dir.join("a.txt"), "a edited\n").unwrap();
    std::fs::write(dir.join("new.txt"), "brand new\n").unwrap();

    // session_changes snapshots the working copy itself, so the on-disk edit to
    // the tracked file surfaces without any prior mutation.
    let changes = repo.session_changes().expect("session changes");

    let edited = changes
        .iter()
        .find(|c| c.path == "a.txt")
        .expect("a.txt in the review");
    assert_eq!(edited.kind, ChangeKind::Modified);
    assert_eq!(edited.new_text.as_deref(), Some("a edited\n"));

    // The untracked file is excluded from the uncommitted-changes set, so it
    // does not appear in the review.
    assert!(
        changes.iter().all(|c| c.path != "new.txt"),
        "untracked file must not show up in the review: {changes:?}"
    );
}

#[test]
fn committed_content_edits_show_up_but_message_edits_do_not() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );

    let mut repo = Repo::open(dir).expect("open");

    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let second = commits
        .iter()
        .find(|c| c.subject == "second")
        .expect("second commit")
        .id
        .clone();

    // A message-only edit changes no tree, so the review stays empty.
    repo.rewrite_message(&second, "second (edited)")
        .expect("rewrite message");
    assert!(
        repo.session_changes().expect("session changes").is_empty(),
        "a message-only edit does not change any tree content"
    );

    // Editing a committed file's content does show up — and only that file.
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let second = commits
        .iter()
        .find(|c| c.subject == "second (edited)")
        .expect("edited commit")
        .id
        .clone();
    repo.rewrite_file(&second, "b.txt", "b rewritten\n")
        .expect("rewrite file");

    let changes = repo.session_changes().expect("session changes");
    assert_eq!(changes.len(), 1, "only the content edit shows: {changes:?}");
    assert_eq!(changes[0].path, "b.txt");
    assert_eq!(changes[0].kind, ChangeKind::Modified);
    assert_eq!(changes[0].new_text.as_deref(), Some("b rewritten\n"));
}

#[test]
fn revert_all_empties_the_review() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );

    let mut repo = Repo::open(dir).expect("open");

    // A working-tree edit plus a committed content rewrite — a non-empty review.
    std::fs::write(dir.join("a.txt"), "a edited\n").unwrap();
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let first = commits
        .iter()
        .find(|c| c.subject == "first")
        .expect("first commit")
        .id
        .clone();
    repo.rewrite_file(&first, "a.txt", "a committed\n")
        .expect("rewrite file");
    assert!(
        !repo.session_changes().expect("session changes").is_empty(),
        "edits made this session populate the review"
    );

    // Reverting the session restores the session-start tree, so the review goes
    // empty again.
    repo.revert_all().expect("revert all");
    assert!(
        repo.session_changes().expect("session changes").is_empty(),
        "after revert_all the current tree matches the session start"
    );
}
