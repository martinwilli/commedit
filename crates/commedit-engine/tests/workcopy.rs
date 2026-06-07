//! Snapshot the on-disk working copy into jj's `@` commit and materialize it
//! back out — the round-trip the rewrite pipeline relies on to preserve
//! uncommitted changes.

mod common;

use commedit_engine::repo::Repo;

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
