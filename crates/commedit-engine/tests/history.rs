//! End-to-end: list the commit history of a scratch repo.

mod common;

use commedit_engine::history::history;
use commedit_engine::repo::Repo;

#[test]
fn lists_history_newest_first() {
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

    let repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");

    // jj may add an empty working-copy commit on top; ignore empty subjects.
    let subjects: Vec<String> = commits
        .into_iter()
        .map(|c| c.subject)
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(subjects, vec!["third", "second", "first"]);
}
