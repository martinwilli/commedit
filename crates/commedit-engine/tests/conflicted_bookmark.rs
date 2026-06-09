//! A history rewrite that would leave the checked-out branch's jj bookmark
//! *conflicted* (pointing at several commits) can't be exported — jj silently
//! skips a conflicted bookmark, so the edit never reaches git. commedit must
//! refuse such an edit with a clear error rather than appear to succeed while
//! silently doing nothing.

mod common;

use commedit_engine::history::history;
use commedit_engine::repo::Repo;

#[test]
fn refuses_a_conflicted_bookmark_instead_of_silently_no_opping() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    // Two instances opened at the same op head each rewrite the *same* commit:
    // concurrent operations jj reconciles, on the next open, into a conflicted
    // `main` bookmark (purely local op divergence — no remote involved).
    let base = {
        let probe = Repo::open(dir).expect("probe");
        let head = probe.head_commit_id().expect("head");
        history(&probe.repo, &head)
            .expect("history")
            .into_iter()
            .find(|c| c.subject == "A")
            .expect("commit A")
            .id
    };
    let mut a = Repo::open(dir).expect("open a");
    let mut b = Repo::open(dir).expect("open b");
    a.rewrite_message(&base, "A (a)").expect("edit a");
    b.rewrite_message(&base, "A (b)").expect("edit b");

    let head_before = common::git(dir, &["rev-parse", "HEAD"]);
    let subjects_before = common::git_log_subjects(dir);

    // A fresh open reconciles the divergent ops into a conflicted bookmark.
    let mut repo = Repo::open(dir).expect("open after divergence");
    let head = repo.head_commit_id().expect("head");
    let target = history(&repo.repo, &head).expect("history")[0].id.clone();

    // A message edit can't collapse a conflicted bookmark, so it must be refused
    // with a clear error — never a silent, unexported no-op.
    let err = repo
        .rewrite_message(&target, "tip (edited)")
        .expect_err("a conflicted bookmark must refuse a non-resolving edit");
    assert!(
        err.to_string().contains("conflicted"),
        "error should explain the conflicted bookmark, got: {err}"
    );
    // Git is untouched by the refused edit.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git_log_subjects(dir), subjects_before);
}
