//! End-to-end: relocating a single diff hunk between endpoints. Moving one hunk
//! out of a commit and into another (or into the working copy) rewrites the
//! source to drop it, folds it into the destination, rebases descendants, and
//! plain `git` sees the re-attributed, conflict-free history — with the overall
//! tree content preserved.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use commedit_engine::workcopy::WcTarget;

/// The commit id of the commit with subject `subject` on the current branch.
fn id_of(repo: &Repo, subject: &str) -> jj_lib::backend::CommitId {
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    commits
        .iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("commit {subject:?} present"))
        .id
        .clone()
}

/// A file `f.txt` whose commit "A" edits two well-separated regions (line 1 and
/// line 9), so its diff renders as two hunks; commit "B" stacks a separate file
/// `g.txt` above it. Layout (oldest first): base <- A <- B on `main`.
fn two_hunk_repo(dir: &std::path::Path) {
    common::init_repo(
        dir,
        &[
            ("f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n", "base"),
            ("f.txt", "ONE\n2\n3\n4\n5\n6\n7\n8\nNINE\n", "A"),
            ("g.txt", "g\n", "B"),
        ],
    );
}

#[test]
fn moves_the_second_hunk_from_a_commit_into_its_descendant() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    two_hunk_repo(dir);
    // The overall tip tree must be byte-identical afterwards — the change set is
    // only re-attributed between commits, not altered.
    let tip_tree_before = common::git(dir, &["rev-parse", "HEAD^{tree}"]);

    let mut repo = Repo::open(dir).expect("open");
    let source = id_of(&repo, "A");
    let dest = id_of(&repo, "B");

    // Change-group 1 is the line-9 region (1→ONE is group 0). Move that group out
    // of "A" and into its descendant "B".
    let outcome = repo
        .squash_hunk_into(&source, &dest, "f.txt", 1, 1, None)
        .expect("move hunk");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // Still three linear commits; "B" keeps its own message (Fixup default).
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A", "base"]);

    // "A" lost the line-9 hunk (line 9 reverts to "9") but kept hunk 0 (ONE).
    assert_eq!(
        common::git(dir, &["show", "HEAD~1:f.txt"]),
        "ONE\n2\n3\n4\n5\n6\n7\n8\n9"
    );
    // "B" now carries the line-9 hunk: the tip's f.txt has NINE again.
    assert_eq!(
        common::git(dir, &["show", "HEAD:f.txt"]),
        "ONE\n2\n3\n4\n5\n6\n7\n8\nNINE"
    );
    // The hunk really was introduced by "B" (it wasn't in "A").
    let b_patch = common::git(dir, &["show", "HEAD"]);
    assert!(b_patch.contains("+NINE"), "B introduces NINE: {b_patch}");

    // The overall tip tree is unchanged, and git sees an ordinary clean repo.
    assert_eq!(
        common::git(dir, &["rev-parse", "HEAD^{tree}"]),
        tip_tree_before
    );
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn carves_a_hunk_from_a_commit_to_the_launch_working_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // base <- A on `main`, where A edits two well-separated regions of f.txt (two
    // hunks); A is the tip and `@` sits on top of it.
    common::init_repo(
        dir,
        &[
            ("f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n", "base"),
            ("f.txt", "ONE\n2\n3\n4\n5\n6\n7\n8\nNINE\n", "A"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");
    let source = id_of(&repo, "A");

    // Carve change-group 1 (the line-9 region) out of "A" onto the launch working
    // copy.
    let outcome = repo
        .carve_hunk_to_working_copy(WcTarget::Launch, &source, "f.txt", 1, 1)
        .expect("carve hunk");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // The committed tip lost the line-9 hunk (kept hunk 0, ONE)…
    assert_eq!(
        common::git(dir, &["show", "HEAD:f.txt"]),
        "ONE\n2\n3\n4\n5\n6\n7\n8\n9"
    );
    // …and it is now an uncommitted change on disk instead.
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "ONE\n2\n3\n4\n5\n6\n7\n8\nNINE\n"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M f.txt");
    // git diff (worktree vs HEAD) shows exactly the carved hunk uncommitted.
    let diff = common::git(dir, &["diff"]);
    assert!(diff.contains("+NINE"), "diff adds NINE: {diff}");
    assert!(
        diff.lines().any(|l| l == "-9"),
        "diff reverts line 9: {diff}"
    );
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn moves_a_working_copy_hunk_into_a_history_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // base <- A <- B on `main`. base commits f.txt; A and B add unrelated files, so
    // A ("g.txt") is a mid-history commit we can fold a working-copy hunk into while
    // B rides the rebase on top.
    common::init_repo(
        dir,
        &[
            ("f.txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n", "base"),
            ("g.txt", "g\n", "A"),
            ("h.txt", "h\n", "B"),
        ],
    );
    // Two well-separated uncommitted hunks in f.txt: line 1 (1→ONE, group 0) and
    // line 9 (9→NINE, group 1).
    std::fs::write(dir.join("f.txt"), "ONE\n2\n3\n4\n5\n6\n7\n8\nNINE\n").unwrap();
    let disk_before = std::fs::read_to_string(dir.join("f.txt")).unwrap();

    let mut repo = Repo::open(dir).expect("open");
    let dest = id_of(&repo, "A");

    // Fold only the line-9 hunk (change-group 1) into "A"; leave line 1 uncommitted.
    let outcome = repo
        .squash_working_copy_hunk_into("f.txt", 1, 1, &dest, None)
        .expect("move working-copy hunk");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // Topology unchanged; "A" keeps its own message (Fixup default).
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A", "base"]);

    // (a) "A" gained the line-9 hunk (NINE) and did NOT get the still-uncommitted
    // line-1 hunk (line 1 stays "1").
    assert_eq!(
        common::git(dir, &["show", "HEAD~1:f.txt"]),
        "1\n2\n3\n4\n5\n6\n7\n8\nNINE"
    );

    // (d) the on-disk file is byte-identical before and after the move.
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        disk_before
    );

    // (b) the OTHER hunk (line 1) is still uncommitted, and (c) the moved hunk
    // (NINE) is no longer uncommitted — the worktree-vs-HEAD diff shows only line 1.
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M f.txt");
    let diff = common::git(dir, &["diff"]);
    assert!(diff.contains("+ONE"), "diff still adds ONE: {diff}");
    assert!(
        diff.lines().any(|l| l == "-1"),
        "diff reverts line 1: {diff}"
    );
    assert!(
        !diff.contains("NINE"),
        "the NINE hunk moved out of the working copy: {diff}"
    );

    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}
