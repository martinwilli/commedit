//! End-to-end: squashing one commit into another (the drag-to-squash gesture)
//! merges its changes, recomposes the target message per mode, drops the source,
//! and plain `git` sees the rewritten, conflict-free history. Plus a
//! conflict-then-resolve path and the autosquash-prefix UI flow.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use commedit_engine::squash::{parse_squash_mode, SquashMode};

/// A linear `first <- second <- third` repo on `main`.
fn three_commits(dir: &std::path::Path) {
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );
}

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

#[test]
fn folding_the_whole_working_copy_into_a_commit_leaves_a_clean_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "1\n", "A"), ("b.txt", "b\n", "B")]);
    let mut repo = Repo::open(dir).expect("open");

    // An uncommitted change that belongs in the older commit "A".
    std::fs::write(dir.join("a.txt"), "1\n2\n").unwrap();
    let dest = id_of(&repo, "A");

    // Dragging the whole pile (the leaf @) onto "A" folds it in as a Fixup.
    let outcome = repo
        .squash_working_copy_into(None, &dest, None)
        .expect("fold");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // "A" gained the change, the branch is unchanged, and the tree is clean —
    // jj recreated a fresh empty @, so there are no uncommitted entries left.
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A"]);
    assert_eq!(common::git(dir, &["show", "HEAD~1:a.txt"]), "1\n2");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    assert!(
        repo.working_copy_chain().is_empty(),
        "clean tree after folding"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "1\n2\n"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn folding_a_peeled_entry_keeps_the_remainder_uncommitted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    let mut repo = Repo::open(dir).expect("open");

    // Two uncommitted changes; peel the a.txt change into its own entry.
    std::fs::write(dir.join("a.txt"), "a\nAA\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\nBB\n").unwrap();
    repo.split_working_copy(None, &[("b.txt".to_string(), "b\n".to_string())])
        .expect("split");
    // The peeled (oldest) entry carries the a.txt change; fold it into "A".
    let chain = repo.working_copy_chain();
    assert_eq!(chain.len(), 2);
    let peeled = chain.last().expect("oldest entry").info.change_id_hex();
    let dest = id_of(&repo, "A");
    let outcome = repo
        .squash_working_copy_into(Some(&peeled), &dest, None)
        .expect("fold peeled entry");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // "A" gained the a.txt change; the b.txt change is still uncommitted; disk is
    // unchanged (both edits still present, one now committed).
    assert_eq!(common::git(dir, &["show", "HEAD~1:a.txt"]), "a\nAA");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\nAA\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "b\nBB\n"
    );
    let remaining = repo.working_copy_chain();
    assert_eq!(
        remaining.len(),
        1,
        "only the b.txt change remains uncommitted"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn a_conflicting_fold_defers_and_leaves_git_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "1\n2\n3\n", "A"), ("a.txt", "1\nB\n3\n", "B")],
    );
    let mut repo = Repo::open(dir).expect("open");

    // Uncommitted change to the same line "B" touched; folding into "A" can't apply.
    std::fs::write(dir.join("a.txt"), "1\nLOCAL\n3\n").unwrap();
    let dest = id_of(&repo, "A");
    let outcome = repo
        .squash_working_copy_into(None, &dest, None)
        .expect("fold");

    let SaveOutcome::Conflicts { commits } = outcome else {
        panic!("expected the fold to defer as a conflict");
    };
    assert!(!commits.is_empty());
    // git is left untouched while the conflict is pending.
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A"]);
    assert_eq!(common::git(dir, &["show", "HEAD:a.txt"]), "1\nB\n3");
    assert!(
        !std::fs::read_to_string(dir.join("a.txt"))
            .unwrap()
            .contains("<<<<<<<"),
        "the worktree must be untouched while the conflict is pending"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn fixup_merges_content_keeps_target_message_drops_source() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    three_commits(dir);

    let mut repo = Repo::open(dir).expect("open");
    let source = id_of(&repo, "third");
    let dest = id_of(&repo, "second");
    let outcome = repo
        .squash_into(&source, &dest, SquashMode::Fixup, None)
        .expect("squash");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // "third" is gone; "second" keeps its own message but now carries c.txt.
    assert_eq!(common::git_log_subjects(dir), vec!["second", "first"]);
    common::git(dir, &["cat-file", "-e", "HEAD:c.txt"]);
    common::git(dir, &["cat-file", "-e", "HEAD:b.txt"]);

    // Transparency.
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn squashes_a_trashed_commit_into_a_chain_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    three_commits(dir);

    let mut repo = Repo::open(dir).expect("open");

    // Drop "second" to the trash: it becomes an orphan, and "third" rebases onto
    // "first". Capture its CommitInfo first — its id stays resolvable afterwards.
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let from = commits.iter().position(|c| c.subject == "second").unwrap();
    let second = commits[from].clone();
    let target = repo.plan_drop(&commits, from).expect("droppable");
    repo.abandon_commit(&target).expect("drop");
    assert_eq!(common::git_log_subjects(dir), vec!["third", "first"]);

    // Squash the trashed "second" (an orphan, unrelated to "first") into "first".
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let onto = commits.iter().position(|c| c.subject == "first").unwrap();
    let (source, dest) = repo
        .plan_squash_restore(&commits, &second, onto)
        .expect("plan");
    let outcome = repo
        .squash_restore_into(&source, &dest, SquashMode::Fixup, None)
        .expect("squash from trash");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // "second"'s content (b.txt) is merged into "first"; the trashed commit is
    // gone from the graph and "first" keeps its own message (Fixup).
    assert_eq!(common::git_log_subjects(dir), vec!["third", "first"]);
    common::git(dir, &["cat-file", "-e", "HEAD:b.txt"]);
    common::git(dir, &["cat-file", "-e", "HEAD:c.txt"]);

    // Transparency.
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn squash_extends_the_target_message() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    three_commits(dir);

    let mut repo = Repo::open(dir).expect("open");
    let source = id_of(&repo, "third");
    let dest = id_of(&repo, "second");
    repo.squash_into(&source, &dest, SquashMode::Squash, None)
        .expect("squash");

    // The target's message gains the source's (unprefixed → full) message.
    assert_eq!(
        common::git(dir, &["show", "-s", "--format=%B", "main"]),
        "second\n\nthird"
    );
    assert_eq!(common::git_log_subjects(dir), vec!["second", "first"]);
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn amend_replaces_the_target_message() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    three_commits(dir);

    let mut repo = Repo::open(dir).expect("open");
    let source = id_of(&repo, "third");
    let dest = id_of(&repo, "second");
    repo.squash_into(&source, &dest, SquashMode::Amend, None)
        .expect("squash");

    // The target's message is replaced by the source's.
    assert_eq!(common::git_log_subjects(dir), vec!["third", "first"]);
    common::git(dir, &["cat-file", "-e", "HEAD:c.txt"]);
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn squash_preserves_the_target_author() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Build the repo by hand so "second" has a distinct author.
    common::git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    common::git(dir, &["add", "a.txt"]);
    common::git(dir, &["commit", "-q", "-m", "first"]);
    std::fs::write(dir.join("b.txt"), "b\n").unwrap();
    common::git(dir, &["add", "b.txt"]);
    common::git(
        dir,
        &[
            "commit",
            "-q",
            "--author",
            "Alice <alice@example.com>",
            "-m",
            "second",
        ],
    );
    std::fs::write(dir.join("c.txt"), "c\n").unwrap();
    common::git(dir, &["add", "c.txt"]);
    common::git(dir, &["commit", "-q", "-m", "third"]);

    let mut repo = Repo::open(dir).expect("open");
    let source = id_of(&repo, "third");
    let dest = id_of(&repo, "second");
    repo.squash_into(&source, &dest, SquashMode::Fixup, None)
        .expect("squash");

    // The target keeps its author (committer may be re-stamped — git-autosquash
    // style — so we don't assert on it).
    assert_eq!(
        common::git(dir, &["show", "-s", "--format=%an <%ae>", "main"]),
        "Alice <alice@example.com>"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn fixup_into_a_non_adjacent_ancestor_moves_the_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    three_commits(dir);

    // Squash the tip "third" into the oldest "first", skipping "second": the tip
    // is abandoned, "second" rebases, and the branch must follow to the new tip.
    let mut repo = Repo::open(dir).expect("open");
    let source = id_of(&repo, "third");
    let dest = id_of(&repo, "first");
    let outcome = repo
        .squash_into(&source, &dest, SquashMode::Fixup, None)
        .expect("squash");
    assert!(matches!(outcome, SaveOutcome::Clean));

    assert_eq!(common::git_log_subjects(dir), vec!["second", "first"]);
    common::git(dir, &["cat-file", "-e", "HEAD:c.txt"]); // source content carried to the tip
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn squash_via_the_autosquash_prefix_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "fixup! second"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let from = commits
        .iter()
        .position(|c| c.subject == "fixup! second")
        .expect("fixup row");

    // The prefixed commit recommends "second" (its bare target) as the target.
    let rec = repo.squash_recommendations(&commits, from);
    let second = commits
        .iter()
        .position(|c| c.subject == "second")
        .expect("second row");
    assert_eq!(rec.targets, vec![second]);
    assert!(rec.siblings.is_empty());

    let mode = parse_squash_mode(&commits[from].subject).expect("prefixed");
    let (source, dest) = repo.plan_squash(&commits, from, second).expect("plan");
    repo.squash_into(&source, &dest, mode, None)
        .expect("squash");

    // The fixup folds into "second", keeping its message.
    assert_eq!(common::git_log_subjects(dir), vec!["second", "first"]);
    common::git(dir, &["cat-file", "-e", "HEAD:c.txt"]);
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn squash_into_can_reword_the_target() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    three_commits(dir);

    let mut repo = Repo::open(dir).expect("open");
    let source = id_of(&repo, "third");
    let dest = id_of(&repo, "second");
    // An explicit message overrides the mode-derived composition (here it would
    // otherwise keep "second" for a Fixup) — fold and reword in one step.
    repo.squash_into(
        &source,
        &dest,
        SquashMode::Fixup,
        Some("merged second + third"),
    )
    .expect("squash");

    assert_eq!(
        common::git(dir, &["show", "-s", "--format=%B", "main"]),
        "merged second + third"
    );
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["merged second + third", "first"]
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn folding_the_working_copy_can_reword_the_target() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "1\n", "A"), ("b.txt", "b\n", "B")]);
    let mut repo = Repo::open(dir).expect("open");

    std::fs::write(dir.join("a.txt"), "1\n2\n").unwrap();
    let dest = id_of(&repo, "A");
    repo.squash_working_copy_into(None, &dest, Some("A (with the a.txt fix)"))
        .expect("fold");

    // "A" gained both the change and the new message (a working-copy fold has no
    // source message of its own, so the override is the only way to reword it).
    assert_eq!(
        common::git(dir, &["show", "-s", "--format=%B", "HEAD~1"]),
        "A (with the a.txt fix)"
    );
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["B", "A (with the a.txt fix)"]
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn conflicting_squash_is_held_back_then_resolved() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // base <- A <- B, A and B both change the middle line, so squashing B into
    // base (over A) can't be applied cleanly.
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
    let source = id_of(&repo, "B");
    let dest = id_of(&repo, "base");
    let mut outcome = repo
        .squash_into(&source, &dest, SquashMode::Fixup, None)
        .expect("squash");

    assert!(
        matches!(outcome, SaveOutcome::Conflicts { .. }),
        "expected a conflict from the non-commuting squash"
    );
    assert!(repo.is_pending(), "engine should hold a pending resolution");

    // Transparency while pending: git still sees the original history.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A", "base"]);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");

    // Resolve oldest-first until the chain is clean.
    let mut steps = 0;
    while let SaveOutcome::Conflicts { commits } = outcome {
        let oldest = commits.into_iter().next().expect("a conflicted commit");
        let path_str = oldest
            .files
            .iter()
            .find(|f| f.resolvable)
            .expect("a resolvable file")
            .path_str();
        let change_hex = oldest.change_id_hex();
        let file = repo
            .read_conflict(&change_hex, &path_str)
            .expect("read conflict");
        outcome = repo
            .resolve_conflict(&change_hex, &path_str, "1\nR\n3\n", file.marker_len)
            .expect("resolve");
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }
    assert!(!repo.is_pending(), "no pending resolution after finalize");

    // Plain git now sees the squashed, conflict-free history: B folded into base,
    // A rebased on top.
    assert_eq!(common::git_log_subjects(dir), vec!["A", "base"]);
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    let tree = common::git(dir, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        !tree.contains(".jjconflict"),
        "no .jjconflict-* in the tree: {tree}"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}
