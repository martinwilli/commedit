//! Snapshot the on-disk working copy into jj's `@` commit and materialize it
//! back out — the round-trip the rewrite pipeline relies on to preserve
//! uncommitted changes.

mod common;

use commedit_engine::history::history;
use commedit_engine::repo::Repo;

fn subject_id(repo: &Repo, subject: &str) -> commedit_engine::history::CommitInfo {
    history(&repo.repo, &repo.head_commit_id().expect("head"))
        .expect("history")
        .into_iter()
        .find(|c| c.subject == subject)
        .expect("commit present")
}

#[test]
fn snapshots_disk_into_working_copy_and_materializes_it_back() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")]);

    let mut repo = Repo::open(dir).expect("open");
    let head = repo.head_commit_id().expect("head");

    // Local uncommitted state: edit a tracked file and add an untracked one.
    std::fs::write(dir.join("a.txt"), "a\nlocal edit\n").unwrap();
    std::fs::write(dir.join("new.txt"), "brand new\n").unwrap();

    // Snapshotting records that state into the working-copy commit @.
    repo.snapshot_working_copy().expect("snapshot");
    let wc = repo.working_copy_commit_id().expect("@ present");
    assert_ne!(wc, head, "@ should be a distinct commit on top of HEAD");

    // Checking out clean HEAD reverts the working tree: the tracked edit is
    // undone and the (now-tracked) new file is removed.
    repo.materialize_working_copy(&head).expect("materialize head");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
    assert!(!dir.join("new.txt").exists(), "untracked file cleared by checkout");

    // Checking @ back out restores exactly what we snapshotted, proving the
    // snapshot captured both the edit and the untracked file.
    repo.materialize_working_copy(&wc).expect("materialize @");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\nlocal edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("new.txt")).unwrap(),
        "brand new\n"
    );
}

#[test]
fn unstaged_edit_to_an_untouched_file_survives_a_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );

    let mut repo = Repo::open(dir).expect("open");
    // Local edit to a.txt, which the rewrite of B does not touch.
    std::fs::write(dir.join("a.txt"), "a\nlocal edit\n").unwrap();

    let target = subject_id(&repo, "B").id;
    repo.rewrite_message(&target, "B (edited)").expect("rewrite");

    // History rewritten, descendants preserved.
    assert_eq!(common::git_log_subjects(dir), vec!["C", "B (edited)", "A"]);
    // The local edit is still on disk, shown by git as an unstaged modification.
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\nlocal edit\n"
    );
    // (the common::git helper trims, so the porcelain " M a.txt" loses its lead)
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M a.txt");
    // Transparency holds: HEAD attached, no jj keep-ref clutter, repo intact.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(
        common::git(dir, &["for-each-ref", "--format=%(refname)", "refs/jj/keep/"]),
        ""
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn untracked_file_survives_a_rewrite_and_jj_dir_never_leaks() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");
    std::fs::write(dir.join("new.txt"), "brand new\n").unwrap();

    // Snapshotting must capture the untracked file but never jj's own .jj dir.
    repo.snapshot_working_copy().expect("snapshot");
    let wc = repo.working_copy_commit_id().expect("@").to_string();
    let tracked = common::git(dir, &["ls-tree", "-r", "--name-only", &wc]);
    assert!(tracked.lines().any(|l| l == "new.txt"), "untracked file captured");
    assert!(
        !tracked.lines().any(|l| l.starts_with(".jj")),
        ".jj must never be snapshotted into @, got: {tracked}"
    );

    let target = subject_id(&repo, "A").id;
    repo.rewrite_message(&target, "A (edited)").expect("rewrite");

    // The untracked file is still on disk and still untracked.
    assert_eq!(
        std::fs::read_to_string(dir.join("new.txt")).unwrap(),
        "brand new\n"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "?? new.txt");
}

#[test]
fn non_overlapping_edit_to_a_rewritten_file_is_merged_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("f.txt", "1\n2\n3\n4\n5\n", "base"), ("g.txt", "g\n", "top")],
    );

    let mut repo = Repo::open(dir).expect("open");
    // Local edit to the last line of f.txt...
    std::fs::write(dir.join("f.txt"), "1\n2\n3\n4\n5-local\n").unwrap();

    // ...while the rewrite changes the first line of f.txt in the base commit.
    let base = subject_id(&repo, "base").id;
    repo.rewrite_file(&base, "f.txt", "1-rewritten\n2\n3\n4\n5\n")
        .expect("rewrite file");

    // jj's 3-way merge carries the local edit onto the rewritten content: the
    // working tree ends up with both changes.
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "1-rewritten\n2\n3\n4\n5-local\n"
    );
    // The committed history has the rewrite but not the uncommitted edit.
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "1-rewritten\n2\n3\n4\n5");
    common::git(dir, &["fsck", "--no-progress"]);
}
