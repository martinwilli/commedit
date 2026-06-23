//! Reproducer: two concurrent multi-tenant sessions, each editing only its OWN
//! branch in its OWN worktree over a shared git common-dir, must never clobber
//! each other. `restore_unrelated_heads` (the per-session "protect unrelated
//! heads" backstop) treats every branch outside *this* session's editable set
//! as unrelated and force-restores it to *this* session's pre-rewrite snapshot —
//! so session A resets session B's branch to a stale value. This is the
//! revert-to-base/dirty-worktree symptom seen in the dogfood tournament.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use commedit_engine::repo::Repo;

/// One worker: open a session on its own worktree/branch and rewrite that
/// branch's tip N times, stamping a monotonic counter into the message. Before
/// each rewrite, verify the branch still holds *this worker's own last value*.
/// A regression means the sibling session's backstop clobbered our ref.
fn worker(worktree: PathBuf, branch: &str, tag: &str, n: usize, clobbers: Arc<AtomicUsize>) {
    let refname = format!("refs/heads/{branch}");
    let mut repo = Repo::open(&worktree).expect("open session");
    let mut last = 0usize;
    for i in 1..=n {
        if last > 0 {
            // Our branch must still carry the message we last wrote.
            let subj = common::git(&worktree, &["log", "-1", "--format=%s", &refname]);
            let expected = format!("{tag} {last}");
            if subj != expected {
                clobbers.fetch_add(1, Ordering::Relaxed);
                eprintln!("CLOBBER on {branch}: expected {expected:?}, git holds {subj:?}");
            }
        }
        let tip = repo.head_commit_id().expect("tip");
        repo.rewrite_message(&tip, &format!("{tag} {i}"))
            .expect("rewrite");
        last = i;
    }
}

#[test]
fn concurrent_sessions_do_not_clobber_each_others_branches() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("f.txt", "f\n", "base")]);
    // Quiet git's maintenance so the only ref-writer in the picture is commedit.
    common::git(dir, &["config", "gc.auto", "0"]);
    common::git(dir, &["config", "maintenance.auto", "false"]);
    common::git(dir, &["branch", "dev-x"]);
    common::git(dir, &["branch", "dev-y"]);

    // Worktrees live in their own tempdir (outside the repo's own worktree).
    let wt = tempfile::tempdir().unwrap();
    let wx = wt.path().join("wx");
    let wy = wt.path().join("wy");
    common::git(dir, &["worktree", "add", wx.to_str().unwrap(), "dev-x"]);
    common::git(dir, &["worktree", "add", wy.to_str().unwrap(), "dev-y"]);

    let n = 150;
    let clob = Arc::new(AtomicUsize::new(0));
    let (cx, cy) = (clob.clone(), clob.clone());
    let hx = std::thread::spawn(move || worker(wx, "dev-x", "x", n, cx));
    let hy = std::thread::spawn(move || worker(wy, "dev-y", "y", n, cy));
    hx.join().unwrap();
    hy.join().unwrap();

    let total = clob.load(Ordering::Relaxed);
    assert_eq!(
        total, 0,
        "{total} cross-session ref clobbers — a session's backstop reset a sibling session's branch"
    );
}
