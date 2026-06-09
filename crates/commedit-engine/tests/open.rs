//! End-to-end: attach jj to a scratch git repo and confirm the colocated layout
//! stays invisible to plain git (HEAD attached, clean status).

mod common;

use commedit_engine::repo::Repo;

#[test]
fn opens_repo_transparently() {
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

    let _repo = Repo::open(dir).expect("open colocated repo");

    assert!(dir.join(".jj").is_dir(), ".jj should be created");
    // jj manages colocated repos with a detached HEAD; we must re-attach it.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    // .jj is excluded, so a plain-git user sees a clean working tree.
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn refuses_a_non_git_folder() {
    // A plain directory that was never `git init`-ed: commedit edits existing
    // history, so it must refuse rather than spawn a fresh repository.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    let err = match Repo::open(dir) {
        Ok(_) => panic!("opening a non-git folder should fail"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("not a git repository"),
        "error should explain the folder is not a git repo, got: {err}"
    );
    // It must not have initialized anything on the way out.
    assert!(!dir.join(".git").exists(), ".git must not be created");
    assert!(!dir.join(".jj").exists(), ".jj must not be created");
}
