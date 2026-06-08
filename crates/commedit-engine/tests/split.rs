//! End-to-end: split a historical commit into the edited diff plus an inserted
//! "fixup! …" commit holding the original tree, confirming plain git sees both
//! commits and that the branch tip / descendants are left unchanged.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::{history, CommitInfo};
use commedit_engine::repo::Repo;

fn commit_named(repo: &Repo, subject: &str) -> CommitInfo {
    history(&repo.repo, &repo.head_commit_id().expect("head"))
        .unwrap()
        .into_iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("commit {subject:?} not found"))
}

#[test]
fn split_middle_commit_inserts_followup_and_preserves_descendants() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("f.txt", "v1\n", "first"),
            ("f.txt", "v2\n", "second"),
            ("g.txt", "g\n", "third"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");
    let target = commit_named(&repo, "second");

    // Rewrite "second" to leave f.txt edited; the inserted commit must restore the
    // original "v2" so the tip and "third" are untouched.
    let outcome = repo
        .split_commit(&target.id, &[("f.txt".to_string(), "v2-edited\n".to_string())])
        .expect("split");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // History gains a "fixup! second" commit right after the edited one.
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["third", "fixup! second", "second", "first"]
    );

    // The edited commit leaves f.txt edited; the split commit restores the
    // original; the tip is byte-for-byte what it was before the split.
    assert_eq!(common::git(dir, &["show", "HEAD~2:f.txt"]), "v2-edited"); // second (C')
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "v2"); //        fixup! second (N)
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "v2"); //          third, unchanged
    assert_eq!(common::git(dir, &["show", "HEAD:g.txt"]), "g");

    // The inserted commit carries the original commit's author.
    assert_eq!(
        common::git(dir, &["show", "-s", "--format=%an <%ae>", "HEAD~1"]),
        "Tester <tester@example.com>"
    );

    // Transparency invariants hold.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn split_working_copy_peels_a_file_into_a_second_entry_without_touching_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    let mut repo = Repo::open(dir).expect("open");
    let head_before = common::git(dir, &["rev-parse", "HEAD"]);

    // Two uncommitted changes.
    std::fs::write(dir.join("a.txt"), "a\nAA\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\nBB\n").unwrap();

    // Split the leaf: the edited entry keeps only the a.txt change (b.txt reverted
    // to HEAD), so the remainder — the b.txt change — spills into the new leaf.
    repo.split_working_copy(None, &[("b.txt".to_string(), "b\n".to_string())])
        .expect("split working copy");

    // Two uncommitted entries now, each carrying one peeled-apart file.
    let chain = repo.working_copy_chain();
    assert_eq!(chain.len(), 2, "split yields two uncommitted entries");
    assert!(
        chain.iter().all(|e| e.changed_files == 1),
        "each entry holds exactly one file's change"
    );

    // git is completely untouched and the on-disk content is byte-identical.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A"]);
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    let status = common::git(dir, &["status", "--porcelain"]);
    assert!(status.contains("a.txt") && status.contains("b.txt"), "got: {status:?}");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\nAA\n");
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b\nBB\n");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn split_working_copy_chain_survives_a_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    let mut repo = Repo::open(dir).expect("open");

    std::fs::write(dir.join("a.txt"), "a\nAA\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\nBB\n").unwrap();
    repo.split_working_copy(None, &[("b.txt".to_string(), "b\n".to_string())])
        .expect("split working copy");
    assert_eq!(repo.working_copy_chain().len(), 2);

    // A bare snapshot (run at the start of every mutation) must not re-attach @
    // onto HEAD and collapse the split chain.
    repo.snapshot_working_copy().expect("snapshot");
    assert_eq!(
        repo.working_copy_chain().len(),
        2,
        "the split chain must survive a snapshot"
    );
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\nAA\n");
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b\nBB\n");
}

#[test]
fn reopening_collapses_a_persisted_split_chain_to_one_entry() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    {
        let mut repo = Repo::open(dir).expect("open");
        std::fs::write(dir.join("a.txt"), "a\nAA\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b\nBB\n").unwrap();
        repo.split_working_copy(None, &[("b.txt".to_string(), "b\n".to_string())])
            .expect("split");
        assert_eq!(repo.working_copy_chain().len(), 2);
    } // close the session — the chain persists only in jj's op log

    // Reopening reconciles to git's view (one unstaged pile): the split chain
    // collapses to a single entry carrying both file changes, disk unchanged.
    let repo = Repo::open(dir).expect("reopen");
    let chain = repo.working_copy_chain();
    assert_eq!(chain.len(), 1, "the persisted chain collapses on reopen");
    assert_eq!(chain[0].changed_files, 2, "both changes in one entry");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\nAA\n");
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b\nBB\n");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn split_tip_commit_moves_branch_to_followup() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("f.txt", "a\n", "first"), ("f.txt", "a\nb\n", "second")],
    );
    let mut repo = Repo::open(dir).expect("open");
    let target = commit_named(&repo, "second"); // the branch tip

    // Edit the tip's diff to add an extra line; the inserted commit restores the
    // original tip content, so the branch tip stays "a\nb".
    let outcome = repo
        .split_commit(&target.id, &[("f.txt".to_string(), "a\nb\nc\n".to_string())])
        .expect("split");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // The branch tip is now the inserted commit, restoring the original tip tree.
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["fixup! second", "second", "first"]
    );
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "a\nb"); //   fixup! second (N)
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "a\nb\nc"); // second (C')

    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}
