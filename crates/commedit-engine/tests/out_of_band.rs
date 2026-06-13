//! A mutation (or read) must survive an out-of-band `git commit` on top of HEAD
//! without the destructive full reopen `reload_repo` does. jj imports git state
//! only at open, so the out-of-band commit is initially absent from jj's view and
//! reading from the live HEAD fails; `sync_to_git_head` re-imports it into the
//! existing session — preserving the trash and op-log — so editing continues.

mod common;

use commedit_engine::conflict::SaveOutcome;
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
fn mutation_after_out_of_band_commit_catches_up_without_reload() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "A"),
            ("b.txt", "b\n", "B"),
            ("c.txt", "c\n", "C"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");

    // Record one session op so the op-log floor is non-trivial: edit C.
    let c = subject_id(&repo, "C").id;
    repo.rewrite_message(&c, "C v2").expect("first rewrite");
    assert_eq!(repo.op_cursor(), 1, "first rewrite recorded one op");

    // Out-of-band: the caller crystallizes a new unit with plain git, on top of
    // HEAD. The commedit session is not told.
    std::fs::write(dir.join("d.txt"), "d\n").unwrap();
    common::git(dir, &["add", "d.txt"]);
    common::git(dir, &["commit", "-q", "-m", "D"]);
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["D", "C v2", "B", "A"],
        "out-of-band commit landed on top of the exported tip"
    );

    // The new commit is absent from jj's view, so resolving from the live HEAD
    // fails until we catch up.
    let live_head = repo.head_commit_id().expect("head");
    assert!(
        history(&repo.repo, &live_head).is_err(),
        "before sync, the out-of-band commit is not in jj's index"
    );

    // Catch up without reopening: import the out-of-band commit into the session.
    let synced = repo.sync_to_git_head().expect("sync");
    assert!(synced, "the out-of-band move is detected and imported");
    assert!(
        !repo.sync_to_git_head().expect("sync again"),
        "a second sync is a no-op once in sync"
    );

    // Reads now work, and a mutation of an *older* commit rebases the out-of-band
    // commit forward.
    let b = subject_id(&repo, "B").id;
    let outcome = repo
        .rewrite_message(&b, "B (edited)")
        .expect("second rewrite");
    assert!(
        matches!(outcome, SaveOutcome::Clean),
        "second rewrite should land clean, got {outcome:?}"
    );
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["D", "C v2", "B (edited)", "A"],
        "out-of-band D survives and rebases onto the edited B"
    );

    // The session survived intact: the import is not a session op, so the floor
    // is preserved and the second rewrite advances the cursor to 2 (not reset).
    assert_eq!(
        repo.op_cursor(),
        2,
        "op-log preserved across the catch-up, not reset like reload_repo"
    );
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn sync_refuses_an_out_of_band_branch_switch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");

    // The user checks out a different branch outside commedit. A session is
    // scoped to its one branch, so a catch-up import can't absorb this — it must
    // refuse and point at a fresh reopen rather than import the wrong branch.
    common::git(dir, &["checkout", "-q", "-b", "other"]);
    let err = repo
        .sync_to_git_head()
        .expect_err("branch switch must refuse");
    assert!(
        err.to_string().contains("branch changed"),
        "unexpected error: {err}"
    );
}
