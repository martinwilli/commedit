//! End-to-end: rewrite a middle commit's message and confirm plain `git` sees
//! the rewritten history (descendants rebased, branch moved).

mod common;

use commedit_engine::history::history;
use commedit_engine::repo::Repo;

#[test]
fn rewrites_middle_commit_message_visible_to_git() {
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

    // Find the middle commit ("second").
    let commits = history(&repo.repo).expect("history");
    let target = commits
        .iter()
        .find(|c| c.subject == "second")
        .expect("second commit present")
        .id
        .clone();

    repo.rewrite_message(&target, "second (edited)")
        .expect("rewrite message");

    // Plain git must see the rewritten message with descendants preserved.
    let subjects = common::git_log_subjects(dir);
    assert_eq!(subjects, vec!["third", "second (edited)", "first"]);

    // Transparency invariants: HEAD attached to the original branch, and a
    // clean working tree — a plain-git user sees nothing unusual.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");

    // Repository must remain intact.
    common::git(dir, &["fsck", "--no-progress"]);
}
