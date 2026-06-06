//! End-to-end: a reorder that doesn't commute produces a conflict the engine
//! holds back from git, the conflict is resolved in jj, and only then does plain
//! `git` see the rewritten — conflict-free — history. Plus the abort path.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::history;
use commedit_engine::repo::Repo;

/// Build `base <- A <- B` where A and B both change the middle line of `f.txt`,
/// so moving A on top of B can't be applied cleanly.
fn conflicting_repo(dir: &std::path::Path) {
    common::init_repo(
        dir,
        &[
            ("f.txt", "1\n2\n3\n", "base"),
            ("f.txt", "1\nA\n3\n", "A"),
            ("f.txt", "1\nB\n3\n", "B"),
        ],
    );
}

/// Plan and perform "drag A (display row 1) to the top".
fn reorder_a_to_top(repo: &mut Repo) -> SaveOutcome {
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let mv = repo.plan_reorder(&commits, 1, 0).expect("reorder plan");
    repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder")
}

#[test]
fn conflicting_reorder_is_held_back_then_resolved() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    conflicting_repo(dir);
    let head_before = common::git(dir, &["rev-parse", "HEAD"]);

    let mut repo = Repo::open(dir).expect("open");
    let mut outcome = reorder_a_to_top(&mut repo);

    // The reorder conflicts, so nothing is exported.
    assert!(
        matches!(outcome, SaveOutcome::Conflicts { .. }),
        "expected a conflict from the non-commuting reorder"
    );
    assert!(repo.is_pending(), "engine should hold a pending resolution");

    // Transparency while pending: git still sees the original history, HEAD
    // unmoved, working tree clean.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A", "base"]);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");

    // Resolve the chain oldest-first; resolving each conflicted commit re-derives
    // its descendants, so a fresh conflict may surface until the chain is clean.
    let mut steps = 0;
    while let SaveOutcome::Conflicts { commits } = outcome {
        let oldest = commits.into_iter().next().expect("a conflicted commit");
        let path_str = oldest
            .files
            .iter()
            .find(|f| f.resolvable)
            .expect("a resolvable file")
            .path_str();
        assert_eq!(path_str, "f.txt");
        let change_hex = oldest.change_id_hex();

        // The conflict materializes with 2-way markers (no base section).
        let file = repo.read_conflict(&change_hex, &path_str).expect("read conflict");
        assert!(file.text.contains("<<<<<<<"), "opening marker: {}", file.text);
        assert!(file.text.contains("======="), "separator: {}", file.text);
        assert!(file.text.contains(">>>>>>>"), "closing marker: {}", file.text);
        assert!(!file.text.contains("|||||||"), "2-way markers omit the base");

        outcome = repo
            .resolve_conflict(&change_hex, &path_str, "1\nR\n3\n", file.marker_len)
            .expect("resolve");
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }
    assert!(!repo.is_pending(), "no pending resolution after finalize");

    // Plain git now sees the reordered, conflict-free history.
    assert_eq!(common::git_log_subjects(dir), vec!["A", "B", "base"]);
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "1\nR\n3");
    // No conflict residue leaked into the tree, and the repo is intact.
    let tree = common::git(dir, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree.contains(".jjconflict"), "no .jjconflict-* in the tree: {tree}");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn aborting_a_conflicted_reorder_restores_the_original_history() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    conflicting_repo(dir);
    let head_before = common::git(dir, &["rev-parse", "HEAD"]);

    let mut repo = Repo::open(dir).expect("open");
    let outcome = reorder_a_to_top(&mut repo);
    assert!(matches!(outcome, SaveOutcome::Conflicts { .. }));
    assert!(repo.is_pending());

    repo.abort().expect("abort");
    assert!(!repo.is_pending(), "pending cleared after abort");

    // The original history is intact for plain git and for the engine's own view.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A", "base"]);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, vec!["B", "A", "base"]);
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Two app instances opened at the same op head and each performing a conflicting
/// reorder produce *divergent operations*; a later open reconciles them into
/// divergent commits (one change id, several visible commits). Resolving a
/// reorder on such a repo must still work: the resolver scopes change-id lookup
/// to the current branch chain instead of the store-wide `resolve_change_id`,
/// which would otherwise bail as "divergent commits". Regression for a reorder
/// whose second commit could not be resolved.
#[test]
fn resolving_works_despite_divergent_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    conflicting_repo(dir);

    // Two instances open at the same operation head (neither has mutated yet),
    // then each rewrites the *same* commit's message — concurrent operations jj
    // will later reconcile into divergent commits (the base commit ends up with
    // two visible successors sharing its change id).
    let base = {
        let probe = Repo::open(dir).expect("probe");
        let head = probe.head_commit_id().expect("head");
        let commits = history(&probe.repo, &head).expect("history");
        commits
            .iter()
            .find(|c| c.subject == "base")
            .expect("base commit")
            .id
            .clone()
    };
    let mut a = Repo::open(dir).expect("open a");
    let mut b = Repo::open(dir).expect("open b");
    a.rewrite_message(&base, "base (a)").expect("edit a");
    b.rewrite_message(&base, "base (b)").expect("edit b");

    // A fresh open loads at head, reconciling the divergent operations; the
    // change ids on the branch now resolve to multiple visible commits.
    let mut repo = Repo::open(dir).expect("open after divergence");
    let mut outcome = reorder_a_to_top(&mut repo);
    assert!(
        matches!(outcome, SaveOutcome::Conflicts { .. }),
        "expected a conflict from the non-commuting reorder"
    );

    // Drive the resolution oldest-first to completion — this is what used to fail.
    let mut steps = 0;
    while let SaveOutcome::Conflicts { commits } = outcome {
        let oldest = commits.into_iter().next().expect("a conflicted commit");
        let path = oldest
            .files
            .iter()
            .find(|f| f.resolvable)
            .expect("a resolvable file")
            .path_str();
        let change_hex = oldest.change_id_hex();
        let file = repo.read_conflict(&change_hex, &path).expect("read conflict");
        outcome = repo
            .resolve_conflict(&change_hex, &path, "1\nR\n3\n", file.marker_len)
            .expect("resolve");
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }
    assert!(!repo.is_pending());

    // Plain git sees the reordered, conflict-free history; the repo is intact.
    // (The base's subject is whichever of the divergent message edits won the
    // reconciliation — the point is A and B were reordered and resolved at all.)
    let subjects = common::git_log_subjects(dir);
    assert_eq!(subjects.len(), 3);
    assert_eq!(&subjects[..2], &["A", "B"]);
    assert!(subjects[2].starts_with("base"), "base commit: {}", subjects[2]);
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "1\nR\n3");
    common::git(dir, &["fsck", "--no-progress"]);
}
