//! End-to-end: a reorder that doesn't commute produces a conflict the engine
//! holds back from git, the conflict is resolved in jj, and only then does plain
//! `git` see the rewritten — conflict-free — history. Plus the abort path.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::history;
use commedit_engine::repo::Repo;

#[test]
fn a_history_rewrite_detects_and_resolves_conflicts_across_the_whole_wc_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("f.txt", "1\n2\n3\n", "A"), ("g.txt", "g\n", "B")]);
    let mut repo = Repo::open(dir).expect("open");

    // Uncommitted: change f.txt's line 2 and g.txt; split so the f.txt change
    // lands in the intermediate entry and the g.txt change in the leaf.
    std::fs::write(dir.join("f.txt"), "1\nUNC\n3\n").unwrap();
    std::fs::write(dir.join("g.txt"), "g\nGG\n").unwrap();
    repo.split_working_copy(None, &[("g.txt".to_string(), "g\n".to_string())])
        .expect("split");
    assert_eq!(repo.working_copy_chain().len(), 2);

    // Rewrite f.txt's line 2 in commit "A"; rebasing the intermediate entry (which
    // also changed line 2) conflicts — a conflict that lives *above* the branch
    // tip, which the old leaf-only walk would only partly see.
    let a = history(&repo.repo, &repo.head_commit_id().unwrap())
        .unwrap()
        .into_iter()
        .find(|c| c.subject == "A")
        .unwrap()
        .id;
    let outcome = repo
        .rewrite_file(&a, "f.txt", "1\nREWRITTEN\n3\n")
        .expect("rewrite");

    let SaveOutcome::Conflicts { commits } = outcome else {
        panic!("expected the rewrite to conflict the uncommitted chain");
    };
    // Both uncommitted entries are surfaced — proof the whole chain is walked.
    let wc: Vec<_> = commits
        .iter()
        .filter(|c| c.subject == "Uncommitted changes")
        .collect();
    assert_eq!(wc.len(), 2, "both uncommitted entries detected");
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A"]);

    // Resolve the intermediate (oldest "Uncommitted changes"); the leaf's
    // inherited conflict clears on rebase, so the chain settles clean and exports.
    let intermediate = wc[0].change_id_hex();
    let cf = repo.read_conflict(&intermediate, "f.txt").expect("read conflict");
    let outcome = repo
        .resolve_conflict(&intermediate, "f.txt", "1\nRESOLVED\n3\n", cf.marker_len)
        .expect("resolve");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // The rewrite now applies to git and the resolved uncommitted changes land on
    // disk (both the resolved f.txt and the still-uncommitted g.txt edit).
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "1\nREWRITTEN\n3");
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), "1\nRESOLVED\n3\n");
    assert_eq!(std::fs::read_to_string(dir.join("g.txt")).unwrap(), "g\nGG\n");
    common::git(dir, &["fsck", "--no-progress"]);
}

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

/// A reorder that conflicts in *two* files of the same commit is resolved by a
/// single `resolve_conflicts` call per commit (all of that commit's files at
/// once), and only then does git see the conflict-free history.
#[test]
fn multi_file_conflict_resolves_per_commit_in_one_call() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // base carries f.txt and g.txt; A and B each change the middle line of both.
    common::init_repo(dir, &[("f.txt", "1\n2\n3\n", "base")]);
    std::fs::write(dir.join("g.txt"), "x\ny\nz\n").unwrap();
    common::git(dir, &["add", "g.txt"]);
    common::git(dir, &["commit", "--amend", "-qm", "base"]);
    std::fs::write(dir.join("f.txt"), "1\nA\n3\n").unwrap();
    std::fs::write(dir.join("g.txt"), "x\nA\nz\n").unwrap();
    common::git(dir, &["commit", "-aqm", "A"]);
    std::fs::write(dir.join("f.txt"), "1\nB\n3\n").unwrap();
    std::fs::write(dir.join("g.txt"), "x\nB\nz\n").unwrap();
    common::git(dir, &["commit", "-aqm", "B"]);

    let mut repo = Repo::open(dir).expect("open");
    let mut outcome = reorder_a_to_top(&mut repo);
    assert!(matches!(outcome, SaveOutcome::Conflicts { .. }));

    let mut steps = 0;
    let mut max_files = 0;
    while let SaveOutcome::Conflicts { commits } = outcome {
        let oldest = commits.into_iter().next().expect("a conflicted commit");
        let change_hex = oldest.change_id_hex();
        // Resolve ALL of this commit's resolvable files together, in one call.
        let files: Vec<(String, String, usize)> = oldest
            .files
            .iter()
            .filter(|f| f.resolvable)
            .map(|f| {
                let path = f.path_str();
                let cf = repo.read_conflict(&change_hex, &path).expect("read conflict");
                let resolved = if path == "f.txt" { "1\nR\n3\n" } else { "x\nR\nz\n" };
                (path, resolved.to_string(), cf.marker_len)
            })
            .collect();
        max_files = max_files.max(files.len());
        outcome = repo.resolve_conflicts(&change_hex, &files).expect("resolve");
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }
    assert!(!repo.is_pending());
    assert!(max_files >= 2, "a commit with two conflicted files was resolved at once");

    assert_eq!(common::git_log_subjects(dir), vec!["A", "B", "base"]);
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "1\nR\n3");
    assert_eq!(common::git(dir, &["show", "HEAD:g.txt"]), "x\nR\nz");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

/// A merge-derived conflict: an evil merge changes `base.txt`'s middle line, so
/// rewriting a parent to change that same line means the merge's recorded delta
/// can no longer apply cleanly — the rebased merge becomes conflicted. The merge
/// goes through the *same* held-back / oldest-first resolution flow as a linear
/// conflict (git untouched until clean, no `.jjconflict-*` residue), and the
/// merge stays a 2-parent merge once resolved.
#[test]
fn merge_rebase_conflict_is_held_back_then_resolved() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_evil_merge_repo(dir);
    let head_before = common::git(dir, &["rev-parse", "HEAD"]);

    let mut repo = Repo::open(dir).expect("open");
    let side = history(&repo.repo, &repo.head_commit_id().expect("head"))
        .expect("history")
        .into_iter()
        .find(|c| c.subject == "side-1")
        .expect("side-1 commit")
        .id;

    // side-1 now also touches base.txt's middle line, which the merge's evil
    // delta also rewrote — the two can't both apply, so the rebased merge conflicts.
    let mut outcome = repo
        .rewrite_file(&side, "base.txt", "1\nSIDE\n3\n")
        .expect("rewrite");
    assert!(
        matches!(outcome, SaveOutcome::Conflicts { .. }),
        "the merge's delta can no longer apply over the edited parent"
    );
    assert!(repo.is_pending());

    // Transparency while pending: git still sees the original merge history.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");

    // Resolve oldest-first through the existing loop.
    let mut steps = 0;
    while let SaveOutcome::Conflicts { commits } = outcome {
        let oldest = commits.into_iter().next().expect("a conflicted commit");
        let path = oldest
            .files
            .iter()
            .find(|f| f.resolvable)
            .expect("a resolvable file")
            .path_str();
        assert_eq!(path, "base.txt");
        let change_hex = oldest.change_id_hex();
        let file = repo.read_conflict(&change_hex, &path).expect("read conflict");
        assert!(file.text.contains("<<<<<<<"), "opening marker: {}", file.text);
        assert!(!file.text.contains("|||||||"), "2-way markers omit the base");
        outcome = repo
            .resolve_conflict(&change_hex, &path, "1\nMERGED\n3\n", file.marker_len)
            .expect("resolve");
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }
    assert!(!repo.is_pending(), "no pending resolution after finalize");

    // git now sees the conflict-free rewritten history; the tip is still a merge.
    assert!(common::is_merge(dir, "HEAD"), "tip stays a 2-parent merge");
    assert_eq!(common::git(dir, &["show", "HEAD:base.txt"]), "1\nMERGED\n3");
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
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

/// Each commedit session gets its own throwaway jj workspace, so concurrent
/// sessions on the same repo no longer share a persistent jj op log. That
/// sharing used to let two sessions record divergent reorder operations which,
/// once a later open reconciled them, corrupted jj's operation graph and made
/// the next rebase panic with "graph has cycle". With independent workspaces the
/// divergence can't happen: a fresh session sees only git's (unchanged) refs and
/// reorders cleanly. (The panic→`Err` safety net itself is covered by a unit
/// test on `catch_jj`.)
#[test]
fn concurrent_sessions_do_not_corrupt_a_shared_op_log() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    conflicting_repo(dir);

    // Two sessions at the same starting point each reorder. Their rewrites are
    // held back as conflicts (overlapping edits) and never reach git, and — being
    // independent workspaces — they leave no shared jj state behind.
    let mut a = Repo::open(dir).expect("open a");
    let mut b = Repo::open(dir).expect("open b");
    let _ = reorder_a_to_top(&mut a);
    let _ = reorder_a_to_top(&mut b);

    // A later session reorders without tripping jj's "graph has cycle" panic.
    let mut repo = Repo::open(dir).expect("open after the other sessions");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let mv = repo.plan_reorder(&commits, 1, 0).expect("reorder plan");
    let result = repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip);
    assert!(
        result.is_ok(),
        "independent sessions must not corrupt each other: {result:?}"
    );

    // No commedit metadata leaked into the user's repo, and git is intact.
    assert!(!dir.join(".jj").exists(), "commedit must not create .jj in the repo");
    assert!(repo.head_commit_id().is_some());
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Aborting a conflicted rewrite must not leave a dangling jj operation head.
/// `reload_at` only moves the in-memory view; it never advances the op log, so a
/// naive abort left the discarded (conflicted) operation as a second op head.
/// That stale head is "the old jj state" that resurfaces — merged into divergent
/// commits — when the repo is next loaded at head. Abort should collapse the op
/// log back to a single head.
#[test]
fn abort_leaves_a_single_op_head() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    conflicting_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let outcome = reorder_a_to_top(&mut repo);
    assert!(matches!(outcome, SaveOutcome::Conflicts { .. }));
    repo.abort().expect("abort");

    // The next edit forks a new operation off the rolled-back op. With a naive
    // `reload_at` abort the discarded conflicted operation was never removed from
    // the op-heads store, so committing this edit leaves *two* divergent heads —
    // which the next load-at-head merges into the stale, conflicted state.
    let head = repo.head_commit_id().expect("head");
    let commits = history(&repo.repo, &head).expect("history");
    let b = commits.iter().find(|c| c.subject == "B").expect("B").id.clone();
    repo.rewrite_message(&b, "B edited").expect("edit B");

    let heads = pollster::block_on(repo.repo.op_heads_store().get_op_heads())
        .expect("op heads");
    assert_eq!(
        heads.len(),
        1,
        "edit after abort should leave one op head, found {}: the discarded \
         conflicted operation is a dangling divergent head",
        heads.len()
    );
}
