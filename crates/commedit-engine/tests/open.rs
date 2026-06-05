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
