//! End-to-end: a reorder that doesn't commute produces a conflict the engine
//! holds back from git, the conflict is resolved in jj, and only then does plain
//! `git` see the rewritten — conflict-free — history. Plus the abort path.

mod common;

use commedit_engine::conflict::{ConflictEdit, FileResolution, SaveOutcome};
use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use commedit_engine::tree::ReplaceError;

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
    let cf = repo
        .read_conflict(&intermediate, "f.txt")
        .expect("read conflict");
    let outcome = repo
        .resolve_conflict(&intermediate, "f.txt", "1\nRESOLVED\n3\n", cf.marker_len)
        .expect("resolve");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // The rewrite now applies to git and the resolved uncommitted changes land on
    // disk (both the resolved f.txt and the still-uncommitted g.txt edit).
    assert_eq!(
        common::git(dir, &["show", "HEAD~1:f.txt"]),
        "1\nREWRITTEN\n3"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "1\nRESOLVED\n3\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("g.txt")).unwrap(),
        "g\nGG\n"
    );
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

/// Extract the full conflict block from materialized marker text: from the first
/// `<<<<<<<` line through the trailing newline of the `>>>>>>>` line. Both are
/// found by their 7-char prefix — which the real (possibly longer) marker starts
/// with — and it's safe in these tests because no *earlier* content run of the
/// same marker char reaches 7 (jj inflates the real marker past any content run).
fn conflict_block(text: &str) -> &str {
    let start = text.find("<<<<<<<").expect("opening marker");
    let close = start + text[start..].find(">>>>>>>").expect("closing marker");
    let end = text[close..]
        .find('\n')
        .map(|n| close + n + 1)
        .unwrap_or(text.len());
    &text[start..end]
}

/// Resolve `path` on the commit with change id `change_hex` by patching the
/// materialized conflict text: replace the whole conflict block with `new`,
/// leaving every surrounding line byte-identical — the surgical counterpart to
/// resolving with a whole rewritten file.
fn resolve_by_patch(repo: &mut Repo, change_hex: &str, path: &str, new: &str) -> SaveOutcome {
    let file = repo.read_conflict(change_hex, path).expect("read conflict");
    let old = conflict_block(&file.text).to_string();
    repo.resolve_conflicts_ext(
        change_hex,
        &[(
            path.to_string(),
            FileResolution::Patch {
                edits: vec![ConflictEdit {
                    old,
                    new: new.to_string(),
                    all: false,
                }],
            },
        )],
    )
    .expect("resolve by patch")
}

/// Plan and perform "drag A (display row 1) to the top".
fn reorder_a_to_top(repo: &mut Repo) -> SaveOutcome {
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let mv = common::plan_reorder_single(repo, &commits, 1, 0);
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
        let file = repo
            .read_conflict(&change_hex, &path_str)
            .expect("read conflict");
        assert!(
            file.text.contains("<<<<<<<"),
            "opening marker: {}",
            file.text
        );
        assert!(file.text.contains("======="), "separator: {}", file.text);
        assert!(
            file.text.contains(">>>>>>>"),
            "closing marker: {}",
            file.text
        );
        assert!(
            !file.text.contains("|||||||"),
            "2-way markers omit the base"
        );

        outcome = repo
            .resolve_conflict(&change_hex, &path_str, "1\nR\n3\n", file.marker_len)
            .expect("resolve");
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }
    assert!(!repo.is_pending(), "no pending resolution after finalize");

    // Plain git now sees the reordered, conflict-free history.
    assert_eq!(common::git_log_subjects(dir), vec!["A", "B", "base"]);
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "1\nR\n3");
    // No conflict residue leaked into the tree, and the repo is intact.
    let tree = common::git(dir, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        !tree.contains(".jjconflict"),
        "no .jjconflict-* in the tree: {tree}"
    );
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
                let cf = repo
                    .read_conflict(&change_hex, &path)
                    .expect("read conflict");
                let resolved = if path == "f.txt" {
                    "1\nR\n3\n"
                } else {
                    "x\nR\nz\n"
                };
                (path, resolved.to_string(), cf.marker_len)
            })
            .collect();
        max_files = max_files.max(files.len());
        outcome = repo
            .resolve_conflicts(&change_hex, &files)
            .expect("resolve");
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }
    assert!(!repo.is_pending());
    assert!(
        max_files >= 2,
        "a commit with two conflicted files was resolved at once"
    );

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
        let file = repo
            .read_conflict(&change_hex, &path)
            .expect("read conflict");
        assert!(
            file.text.contains("<<<<<<<"),
            "opening marker: {}",
            file.text
        );
        assert!(
            !file.text.contains("|||||||"),
            "2-way markers omit the base"
        );
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
        let file = repo
            .read_conflict(&change_hex, &path)
            .expect("read conflict");
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
    assert!(
        subjects[2].starts_with("base"),
        "base commit: {}",
        subjects[2]
    );
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
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
    let mv = common::plan_reorder_single(&repo, &commits, 1, 0);
    let result = repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip);
    assert!(
        result.is_ok(),
        "independent sessions must not corrupt each other: {result:?}"
    );

    // No commedit metadata leaked into the user's repo, and git is intact.
    assert!(
        !dir.join(".jj").exists(),
        "commedit must not create .jj in the repo"
    );
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
    let b = commits
        .iter()
        .find(|c| c.subject == "B")
        .expect("B")
        .id
        .clone();
    repo.rewrite_message(&b, "B edited").expect("edit B");

    let heads = pollster::block_on(repo.repo.op_heads_store().get_op_heads()).expect("op heads");
    assert_eq!(
        heads.len(),
        1,
        "edit after abort should leave one op head, found {}: the discarded \
         conflicted operation is a dangling divergent head",
        heads.len()
    );
}

/// §1 round-trip guard: read_conflict → resolve must carry multibyte UTF-8
/// (emoji, umlaut, CJK) through byte-for-byte. The reported 🚢→🚀 swap, if it
/// lived in the tool, would surface here; the path is a strict `String::from_utf8`
/// plus line copies, so this pins the round-trip clean and blames the reported
/// corruption on the upstream whole-file retype, not read_conflict.
#[test]
fn conflict_read_resolve_preserves_multibyte_utf8() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // A context line carrying the multibyte codepoints, unchanged across
    // base/A/B, plus a middle line A and B both edit so the reorder can't commute.
    let ctx = "Schöne Grüße 🚢 你好世界";
    let base = format!("{ctx}\n2\n3\n");
    let a = format!("{ctx}\nA\n3\n");
    let b = format!("{ctx}\nB\n3\n");
    common::init_repo(
        dir,
        &[
            ("f.txt", base.as_str(), "base"),
            ("f.txt", a.as_str(), "A"),
            ("f.txt", b.as_str(), "B"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let mut outcome = reorder_a_to_top(&mut repo);
    assert!(matches!(outcome, SaveOutcome::Conflicts { .. }));

    let resolved = format!("{ctx}\nR\n3\n");
    let mut steps = 0;
    let mut checked = false;
    while let SaveOutcome::Conflicts { commits } = outcome {
        let oldest = commits.into_iter().next().expect("a conflicted commit");
        let change_hex = oldest.change_id_hex();
        let path = oldest
            .files
            .iter()
            .find(|f| f.resolvable)
            .expect("a resolvable file")
            .path_str();
        let file = repo
            .read_conflict(&change_hex, &path)
            .expect("read conflict");
        if !checked {
            // The multibyte context survives materialization verbatim — no swap.
            assert!(
                file.text.contains('🚢'),
                "materialized text keeps 🚢: {}",
                file.text
            );
            assert!(
                !file.text.contains('🚀'),
                "no 🚢→🚀 corruption: {}",
                file.text
            );
            assert!(
                file.text.contains(ctx),
                "the whole multibyte line survives: {}",
                file.text
            );
            checked = true;
        }
        outcome = repo
            .resolve_conflict(&change_hex, &path, &resolved, file.marker_len)
            .expect("resolve");
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }
    assert!(checked, "at least one conflict was read");
    assert!(!repo.is_pending());

    // The committed blob is byte-identical to the intended resolution (git trims
    // the trailing newline, so compare against the trimmed form).
    let expected = format!("{ctx}\nR\n3");
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), expected);
}

/// Marker-hazard guard: content lines that *look* like conflict markers — a
/// setext `=======` underline and a deep `>>>>>>> …` blockquote — must survive
/// read_conflict → resolve verbatim. jj's choose_materialized_conflict_marker_len
/// inflates the marker length past any marker-char run in the file's sides, so
/// strip_base_sections/simplify_marker_labels never mistake these 7-char content
/// runs for real markers even though they classify position-blind. This pins that
/// guarantee (and, with the marker_len assertions, records *why* it holds).
#[test]
fn conflict_read_resolve_preserves_marker_like_content() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Marker-like lines bracket the conflicting middle line as unchanged context,
    // so they land in the materialized output outside the real conflict markers.
    let setext = "======="; // 7 '=' — a setext-style heading underline
    let quote = ">>>>>>> quoted text"; // 7 '>' — a deep blockquote line
    let make = |mid: &str| format!("# Title\n{setext}\n{mid}\n{quote}\nend\n");
    let base = make("2");
    let a = make("A");
    let b = make("B");
    common::init_repo(
        dir,
        &[
            ("doc.md", base.as_str(), "base"),
            ("doc.md", a.as_str(), "A"),
            ("doc.md", b.as_str(), "B"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let mut outcome = reorder_a_to_top(&mut repo);
    assert!(matches!(outcome, SaveOutcome::Conflicts { .. }));

    let resolved = make("R");
    let mut steps = 0;
    let mut seen_marker_len = 0;
    while let SaveOutcome::Conflicts { commits } = outcome {
        let oldest = commits.into_iter().next().expect("a conflicted commit");
        let change_hex = oldest.change_id_hex();
        let path = oldest
            .files
            .iter()
            .find(|f| f.resolvable)
            .expect("a resolvable file")
            .path_str();
        let file = repo
            .read_conflict(&change_hex, &path)
            .expect("read conflict");
        seen_marker_len = seen_marker_len.max(file.marker_len);
        // jj inflated the marker length past the 7-char content runs, which is the
        // reason the position-blind classifier can't touch these lines.
        assert!(
            file.marker_len > 7,
            "marker_len inflated past content runs: {}",
            file.marker_len
        );
        // The marker-like content lines appear verbatim, bounded by newlines so
        // the assertion can't accidentally match the (longer) real marker lines.
        assert!(
            file.text.contains(&format!("\n{setext}\n")),
            "setext underline survives read: {}",
            file.text
        );
        assert!(
            file.text.contains(&format!("\n{quote}\n")),
            "blockquote line survives read: {}",
            file.text
        );
        outcome = repo
            .resolve_conflict(&change_hex, &path, &resolved, file.marker_len)
            .expect("resolve");
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }
    assert!(!repo.is_pending());
    assert!(
        seen_marker_len >= 11,
        "7-char content runs force marker_len 7+4: {seen_marker_len}"
    );

    // The committed blob keeps the marker-like content lines byte-for-byte.
    let expected = make("R");
    let expected = expected.strip_suffix('\n').unwrap();
    assert_eq!(common::git(dir, &["show", "HEAD:doc.md"]), expected);
}

/// §2 happy path: a conflict is resolved with a small `old`→`new` patch against
/// the materialized marker text (replace just the conflict block), not a whole
/// rewritten file. The surrounding context lines are never resent yet come out
/// byte-identical, and plain `git` sees the reordered, conflict-free history.
#[test]
fn patch_resolves_conflict_with_a_small_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Context lines bracket the conflicting middle line; a whole-file resolution
    // would have to resend all of them, a patch touches only the middle.
    let make = |mid: &str| format!("a\nb\nc\n{mid}\nx\ny\nz\n");
    common::init_repo(
        dir,
        &[
            ("f.txt", make("2").as_str(), "base"),
            ("f.txt", make("A").as_str(), "A"),
            ("f.txt", make("B").as_str(), "B"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let mut outcome = reorder_a_to_top(&mut repo);
    assert!(matches!(outcome, SaveOutcome::Conflicts { .. }));

    let mut steps = 0;
    while let SaveOutcome::Conflicts { commits } = outcome {
        let oldest = commits.into_iter().next().expect("a conflicted commit");
        let path = oldest
            .files
            .iter()
            .find(|f| f.resolvable)
            .expect("a resolvable file")
            .path_str();
        // Replace the whole conflict block with the single resolved line "R".
        outcome = resolve_by_patch(&mut repo, &oldest.change_id_hex(), &path, "R\n");
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }
    assert!(!repo.is_pending());

    // The reorder landed and the resolved blob is exactly context + "R", proving
    // the untouched lines round-tripped through the patch byte-for-byte.
    assert_eq!(common::git_log_subjects(dir), vec!["A", "B", "base"]);
    assert_eq!(
        common::git(dir, &["show", "HEAD:f.txt"]),
        make("R").strip_suffix('\n').unwrap()
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

/// A patch whose `old` is absent (or ambiguous) in the conflict text fails
/// exactly like `replace_in_file`: it returns a downcastable [`ReplaceError`], the
/// pending resolution is untouched, and git stays frozen. This is what lets the
/// MCP layer report a bad patch as a caller error rather than an internal one.
#[test]
fn patch_resolution_reports_not_found_and_ambiguous() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // "dup" appears twice as context, so it is an ambiguous match target; "nope"
    // is nowhere in the file, so it is a not-found target.
    let make = |mid: &str| format!("dup\n1\n{mid}\ndup\n3\n");
    common::init_repo(
        dir,
        &[
            ("f.txt", make("2").as_str(), "base"),
            ("f.txt", make("A").as_str(), "A"),
            ("f.txt", make("B").as_str(), "B"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let outcome = reorder_a_to_top(&mut repo);
    let SaveOutcome::Conflicts { commits } = outcome else {
        panic!("expected a conflict");
    };
    let oldest = &commits[0];
    let change_hex = oldest.change_id_hex();
    let path = oldest
        .files
        .iter()
        .find(|f| f.resolvable)
        .expect("a resolvable file")
        .path_str();
    // read_conflict succeeds — the miss is purely about the patch's `old`.
    repo.read_conflict(&change_hex, &path)
        .expect("read conflict");

    let patch = |old: &str| {
        vec![(
            path.clone(),
            FileResolution::Patch {
                edits: vec![ConflictEdit {
                    old: old.to_string(),
                    new: "whatever".to_string(),
                    all: false,
                }],
            },
        )]
    };

    // Absent `old` → NotFound, carrying the path and a hint.
    let err = repo
        .resolve_conflicts_ext(&change_hex, &patch("nope-not-in-conflict\n"))
        .expect_err("absent old must error");
    match err.downcast_ref::<ReplaceError>() {
        Some(ReplaceError::NotFound { path: p, .. }) => assert_eq!(p, &path),
        other => panic!("expected ReplaceError::NotFound, got {other:?} / {err:#}"),
    }

    // Duplicated `old` → Ambiguous with the occurrence count.
    let err = repo
        .resolve_conflicts_ext(&change_hex, &patch("dup\n"))
        .expect_err("ambiguous old must error");
    match err.downcast_ref::<ReplaceError>() {
        Some(ReplaceError::Ambiguous { count, .. }) => assert_eq!(*count, 2),
        other => panic!("expected ReplaceError::Ambiguous, got {other:?} / {err:#}"),
    }

    // A failed patch never touches the held rewrite: still pending, git frozen.
    assert!(
        repo.is_pending(),
        "failed patch must leave the resolution pending"
    );
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A", "base"]);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
}

/// The marker-lookalike guard, but resolved via a *patch*: content lines that look
/// like conflict markers (a setext `=======`, a deep `>>>>>>>` blockquote) survive
/// a patch resolution verbatim. The patch replaces only the real conflict block,
/// which jj marks with an inflated marker length, so the 7-char content runs are
/// never in the block the edit touches.
#[test]
fn patch_resolution_preserves_marker_like_content() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let setext = "======="; // 7 '=' — a setext-style heading underline
    let quote = ">>>>>>> quoted text"; // 7 '>' — a deep blockquote line
    let make = |mid: &str| format!("# Title\n{setext}\n{mid}\n{quote}\nend\n");
    common::init_repo(
        dir,
        &[
            ("doc.md", make("2").as_str(), "base"),
            ("doc.md", make("A").as_str(), "A"),
            ("doc.md", make("B").as_str(), "B"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let mut outcome = reorder_a_to_top(&mut repo);
    assert!(matches!(outcome, SaveOutcome::Conflicts { .. }));

    let mut steps = 0;
    while let SaveOutcome::Conflicts { commits } = outcome {
        let oldest = commits.into_iter().next().expect("a conflicted commit");
        let path = oldest
            .files
            .iter()
            .find(|f| f.resolvable)
            .expect("a resolvable file")
            .path_str();
        outcome = resolve_by_patch(&mut repo, &oldest.change_id_hex(), &path, "R\n");
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }
    assert!(!repo.is_pending());

    // The marker-like content lines outside the conflict block are byte-for-byte.
    assert_eq!(
        common::git(dir, &["show", "HEAD:doc.md"]),
        make("R").strip_suffix('\n').unwrap()
    );
}

/// A *clean* working copy still inherits the conflict its rebased parent picked
/// up, so it is reported among the conflicted commits while having no diff — and
/// therefore no `worktree_chain_entries()` row — of its own. Only
/// `worktree_chain_change_ids()` names it, which is how the GTK conflict view
/// tells such a badge apart from a real branch conflict.
#[test]
fn a_clean_working_copy_conflict_is_named_by_change_id_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // A and B both rewrite f.txt's middle line, so rewriting A's conflicts B.
    common::init_repo(
        dir,
        &[("f.txt", "1\n2\n3\n", "A"), ("f.txt", "1\nB\n3\n", "B")],
    );
    let mut repo = Repo::open(dir).expect("open");
    assert!(
        repo.worktree_chain_entries().is_empty(),
        "the worktree is clean, so there are no entries to start with"
    );

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
        panic!("expected the rewrite to conflict B");
    };
    let wc: Vec<_> = commits
        .iter()
        .filter(|c| c.subject == "Uncommitted changes")
        .collect();
    assert_eq!(wc.len(), 1, "the clean @ inherits B's conflict");

    // Still no entry — the `@` changes no file of its own ...
    assert!(repo.worktree_chain_entries().is_empty());
    // ... but its change id is listed, so the badge is attributable.
    assert!(
        repo.worktree_chain_change_ids()
            .contains(&wc[0].change_id_hex()),
        "the conflicted @ must be named among the worktree chain change ids"
    );
}
