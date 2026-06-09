//! Session-scoped time-travel: undo / redo / jump over the operations performed
//! this session, exercised headless against plain `git`. The cursor walks a
//! linear op-log; each recorded op is a clean, git-exported state, so a jump
//! rewinds jj's view and re-exports it. Mirrors the rewrite/conflict test shape
//! (`init_repo`, `git_log_subjects`, `git fsck`).

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use jj_lib::backend::CommitId;

/// The id of the commit with the given subject on the current branch.
fn id_of(repo: &Repo, subject: &str) -> CommitId {
    history(&repo.repo, &repo.head_commit_id().expect("head"))
        .expect("history")
        .into_iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("commit {subject:?} present"))
        .id
}

/// Plan and perform "drag the commit at display row `from` to gap `to`".
fn reorder(repo: &mut Repo, from: usize, to: usize) -> SaveOutcome {
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let mv = repo.plan_reorder(&commits, from, to).expect("reorder plan");
    repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder")
}

#[test]
fn undo_one_clean_edit_restores_git() {
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
    let head_before = common::git(dir, &["rev-parse", "HEAD"]);
    let mut repo = Repo::open(dir).expect("open");

    let second = id_of(&repo, "second");
    repo.rewrite_message(&second, "second (edited)").expect("edit");
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["third", "second (edited)", "first"]
    );
    assert!(repo.can_undo());
    assert_eq!(repo.op_cursor(), 1);
    assert_eq!(repo.session_ops().len(), 1);
    assert_eq!(repo.session_ops()[0].label(), "Edit message of \"second\"");

    let outcome = repo.undo().expect("undo");
    assert!(matches!(outcome, SaveOutcome::Clean));
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["third", "second", "first"]
    );
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert!(!repo.can_undo());
    assert!(repo.can_redo());
    assert_eq!(repo.op_cursor(), 0);
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn redo_reapplies_after_undo() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")]);
    let mut repo = Repo::open(dir).expect("open");

    let second = id_of(&repo, "second");
    repo.rewrite_message(&second, "second (edited)").expect("edit");
    repo.undo().expect("undo");
    assert_eq!(common::git_log_subjects(dir), vec!["second", "first"]);

    let outcome = repo.redo().expect("redo");
    assert!(matches!(outcome, SaveOutcome::Clean));
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["second (edited)", "first"]
    );
    assert_eq!(repo.op_cursor(), 1);
    assert!(!repo.can_redo());
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn jump_to_intermediate_op() {
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

    // Three distinct mutations.
    let first = id_of(&repo, "first");
    repo.rewrite_message(&first, "first (edited)").expect("edit 1");
    let third = id_of(&repo, "third");
    repo.abandon_commit(&third).expect("drop 3");
    let second = id_of(&repo, "second");
    repo.rewrite_file(&second, "b.txt", "b changed\n")
        .expect("edit file");
    assert_eq!(repo.session_ops().len(), 3);
    assert_eq!(repo.op_cursor(), 3);
    let latest = common::git(dir, &["rev-parse", "HEAD"]);

    // Jump back to the state after only the first mutation.
    repo.jump_to_op(1).expect("jump back");
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["third", "second", "first (edited)"]
    );
    assert_eq!(common::git(dir, &["show", "HEAD:b.txt"]), "b");
    assert_eq!(repo.op_cursor(), 1);

    // Jump forward to the latest state.
    repo.jump_to_op(3).expect("jump forward");
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), latest);
    assert_eq!(common::git_log_subjects(dir), vec!["second", "first (edited)"]);
    assert_eq!(common::git(dir, &["show", "HEAD:b.txt"]), "b changed");
    assert_eq!(repo.op_cursor(), 3);
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn new_edit_after_undo_truncates_redo() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")]);
    let mut repo = Repo::open(dir).expect("open");

    let first = id_of(&repo, "first");
    repo.rewrite_message(&first, "edit A").expect("edit A");
    let first = id_of(&repo, "edit A");
    repo.rewrite_message(&first, "edit B").expect("edit B");
    assert_eq!(repo.session_ops().len(), 2);

    repo.undo().expect("undo to A");
    assert_eq!(repo.op_cursor(), 1);
    assert!(repo.can_redo());

    // A fresh edit from the back-jumped state truncates the unreachable "edit B".
    let first = id_of(&repo, "edit A");
    repo.rewrite_message(&first, "edit C").expect("edit C");
    assert_eq!(repo.session_ops().len(), 2);
    assert_eq!(repo.op_cursor(), 2);
    assert!(!repo.can_redo());
    assert_eq!(repo.session_ops()[1].label(), "Edit message of \"edit A\"");
    assert_eq!(common::git_log_subjects(dir), vec!["second", "edit C"]);
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn undo_reorder_rebases_descendants() {
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

    // Move "third" (display row 0) down below "second" (gap 2).
    reorder(&mut repo, 0, 2);
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["second", "third", "first"]
    );

    repo.undo().expect("undo reorder");
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["third", "second", "first"]
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn undo_floor_is_session_start() {
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
    let session_head = common::git(dir, &["rev-parse", "HEAD"]);
    let mut repo = Repo::open(dir).expect("open");

    let second = id_of(&repo, "second");
    repo.rewrite_message(&second, "second (edited)").expect("edit");
    let third = id_of(&repo, "third");
    repo.abandon_commit(&third).expect("drop");

    // Undo repeatedly; it stops at the session-start floor and stays there.
    repo.undo().expect("undo 1");
    repo.undo().expect("undo 2");
    assert!(!repo.can_undo());
    repo.undo().expect("undo at floor is a no-op");
    assert_eq!(repo.op_cursor(), 0);
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), session_head);
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["third", "second", "first"]
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn conflict_then_resolve_records_exactly_one_op() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("f.txt", "1\n2\n3\n", "base"),
            ("f.txt", "1\nA\n3\n", "A"),
            ("f.txt", "1\nB\n3\n", "B"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");

    // Move A (display row 1) on top of B (gap 0): doesn't commute, conflicts.
    let mut outcome = reorder(&mut repo, 1, 0);
    assert!(matches!(outcome, SaveOutcome::Conflicts { .. }));
    assert!(repo.is_pending());
    // Held back: nothing recorded yet, git untouched.
    assert_eq!(repo.session_ops().len(), 0);

    while let SaveOutcome::Conflicts { commits } = outcome {
        let oldest = commits.into_iter().next().expect("a conflicted commit");
        let path = oldest.files[0].path_str();
        let change = oldest.change_id_hex();
        let cf = repo.read_conflict(&change, &path).expect("read conflict");
        outcome = repo
            .resolve_conflict(&change, &path, "1\nR\n3\n", cf.marker_len)
            .expect("resolve");
    }
    assert!(!repo.is_pending());
    // The whole reorder-then-resolve recorded exactly one session op.
    assert_eq!(repo.session_ops().len(), 1);
    assert_eq!(repo.op_cursor(), 1);
    assert_eq!(common::git_log_subjects(dir), vec!["A", "B", "base"]);
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn pending_conflict_is_dropped_on_jump() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("f.txt", "1\n2\n3\n", "base"),
            ("f.txt", "1\nA\n3\n", "A"),
            ("f.txt", "1\nB\n3\n", "B"),
        ],
    );
    let head_before = common::git(dir, &["rev-parse", "HEAD"]);
    let mut repo = Repo::open(dir).expect("open");

    let outcome = reorder(&mut repo, 1, 0);
    assert!(matches!(outcome, SaveOutcome::Conflicts { .. }));
    assert!(repo.is_pending());

    // Jump to the session-start floor: drops the held rewrite, restores git.
    repo.jump_to_op(0).expect("jump to start");
    assert!(!repo.is_pending());
    assert_eq!(repo.op_cursor(), 0);
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A", "base"]);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn revert_all_still_lands_cursor_at_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
        ],
    );
    let session_head = common::git(dir, &["rev-parse", "HEAD"]);
    let mut repo = Repo::open(dir).expect("open");

    let second = id_of(&repo, "second");
    repo.rewrite_message(&second, "second (edited)").expect("edit");

    repo.revert_all().expect("revert all");
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), session_head);
    assert_eq!(common::git_log_subjects(dir), vec!["second", "first"]);
    assert_eq!(repo.op_cursor(), 0);
    common::git(dir, &["fsck", "--no-progress"]);
}
