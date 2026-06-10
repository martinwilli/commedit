//! End-to-end: a reorder whose intermediate commits conflict only *spuriously*
//! (adjacent but independent edits) is auto-resolved silently — the branch tip is
//! identical to the original, so the chain is rebuilt clean and exported without
//! any pending resolution. A reorder that *truly* conflicts still falls back to
//! manual resolution.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use commedit_engine::squash::SquashMode;

/// Plan and perform "drag display row `from` to the top gap".
fn reorder_row_to_top(repo: &mut Repo, from: usize) -> SaveOutcome {
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let mv = common::plan_reorder_single(repo, &commits, from, 0);
    repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder")
}

#[test]
fn spurious_reorder_conflict_is_auto_resolved_silently() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // foo / +bar / +baz : C1 inserts `bar`, C2 inserts the adjacent `baz`. The two
    // are independent, so reordering C2 below C1 yields the same final file — but
    // jj's 3-way merge conflicts the intermediate.
    common::init_repo(
        dir,
        &[
            ("f.txt", "foo\n", "base"),
            ("f.txt", "foo\nbar\n", "C1-bar"),
            ("f.txt", "foo\nbar\nbaz\n", "C2-baz"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");

    // Move C1-bar (display row 1) to the top, i.e. apply C2-baz first.
    let outcome = reorder_row_to_top(&mut repo, 1);

    // The spurious intermediate conflict is resolved without bothering the user.
    assert!(
        matches!(outcome, SaveOutcome::Clean),
        "expected the spurious reorder to auto-resolve, got {outcome:?}"
    );
    assert!(!repo.is_pending(), "nothing should be left pending");

    // git sees the reordered, conflict-free history: base <- C2-baz <- C1-bar.
    assert_eq!(common::git_log_subjects(dir), vec!["C1-bar", "C2-baz", "base"]);
    // The tip is byte-identical to the original combined result...
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "foo\nbar\nbaz");
    // ...and each commit keeps its own change: C2-baz introduces just `baz` onto
    // the base (correct per-commit attribution, not an empty or absorbed commit).
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "foo\nbaz");
    assert_eq!(common::git(dir, &["show", "HEAD~2:f.txt"]), "foo");

    // Transparency invariants: HEAD attached, clean tree, intact repo, no residue.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
    // No leftover conflict markers anywhere in the working tree.
    assert!(!std::fs::read_to_string(dir.join("f.txt"))
        .unwrap()
        .contains("<<<<<<<"));
}

#[test]
fn spurious_reorder_resolves_across_multiple_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Two files, each with the same adjacent-insertion shape. Reordering exercises
    // the per-file peel for every conflicted path of the intermediate commit.
    common::init_repo(dir, &[("f.txt", "f0\n", "base-f")]);
    std::fs::write(dir.join("g.txt"), "g0\n").unwrap();
    common::git(dir, &["add", "g.txt"]);
    common::git(dir, &["commit", "-q", "-m", "base-g"]);
    std::fs::write(dir.join("f.txt"), "f0\nf1\n").unwrap();
    std::fs::write(dir.join("g.txt"), "g0\ng1\n").unwrap();
    common::git(dir, &["commit", "-aqm", "C1"]);
    std::fs::write(dir.join("f.txt"), "f0\nf1\nf2\n").unwrap();
    std::fs::write(dir.join("g.txt"), "g0\ng1\ng2\n").unwrap();
    common::git(dir, &["commit", "-aqm", "C2"]);

    let mut repo = Repo::open(dir).expect("open");
    let outcome = reorder_row_to_top(&mut repo, 1); // apply C2 first

    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");
    assert!(!repo.is_pending());
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "f0\nf1\nf2");
    assert_eq!(common::git(dir, &["show", "HEAD:g.txt"]), "g0\ng1\ng2");
    // The new bottom commit (C2) carries just its own additions on each file.
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "f0\nf2");
    assert_eq!(common::git(dir, &["show", "HEAD~1:g.txt"]), "g0\ng2");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn spurious_reorder_auto_resolves_and_preserves_uncommitted_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Same spurious shape as above, plus a dirty working tree: an uncommitted edit
    // to the reordered file and to an unrelated one — the real-world case.
    common::init_repo(
        dir,
        &[
            ("f.txt", "foo\n", "base"),
            ("f.txt", "foo\nbar\n", "C1-bar"),
            ("f.txt", "foo\nbar\nbaz\n", "C2-baz"),
        ],
    );
    // Uncommitted: append to the reordered file and add a second, unrelated file.
    std::fs::write(dir.join("f.txt"), "foo\nbar\nbaz\nlocal\n").unwrap();
    std::fs::write(dir.join("other.txt"), "scratch\n").unwrap();

    let mut repo = Repo::open(dir).expect("open");
    let outcome = reorder_row_to_top(&mut repo, 1); // apply C2 first

    // Auto-resolves despite the dirty tree...
    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");
    assert!(!repo.is_pending());
    assert_eq!(common::git_log_subjects(dir), vec!["C1-bar", "C2-baz", "base"]);
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "foo\nbaz");
    // ...and the uncommitted changes are preserved on disk and still uncommitted.
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "foo\nbar\nbaz\nlocal\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("other.txt")).unwrap(),
        "scratch\n"
    );
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "foo\nbar\nbaz");
    let status = common::git(dir, &["status", "--porcelain"]);
    assert!(status.contains("f.txt") && status.contains("other.txt"), "status: {status:?}");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn spurious_squash_across_an_interior_commit_is_auto_resolved() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // foo / +bar / +baz / +qux: each commit appends one adjacent line. Squashing
    // C (+qux) into A (+bar), *skipping* B (+baz), re-applies qux's change where
    // baz is absent — so A' conflicts spuriously. But B re-adds baz on top,
    // cancelling the conflict, so the tip is clean and equals the original; the
    // interior conflict is auto-resolved without bothering the user.
    common::init_repo(
        dir,
        &[
            ("f.txt", "foo\n", "base"),
            ("f.txt", "foo\nbar\n", "A"),
            ("f.txt", "foo\nbar\nbaz\n", "B"),
            ("f.txt", "foo\nbar\nbaz\nqux\n", "C"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");

    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let from = commits.iter().position(|c| c.subject == "C").unwrap();
    let onto = commits.iter().position(|c| c.subject == "A").unwrap();
    let (source, dest) = repo.plan_squash(&commits, from, onto).expect("plan");
    let outcome = repo
        .squash_into(&source, &dest, SquashMode::Fixup)
        .expect("squash");

    assert!(
        matches!(outcome, SaveOutcome::Clean),
        "expected the spurious squash to auto-resolve, got {outcome:?}"
    );
    assert!(!repo.is_pending(), "nothing should be left pending");

    // C folded into A (Fixup keeps A's message); the tip is the original combined
    // file, and A' now carries `qux` (re-attributed) but not `baz`.
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A", "base"]);
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "foo\nbar\nbaz\nqux");
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "foo\nbar\nqux");

    // Transparency invariants.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
    assert!(!std::fs::read_to_string(dir.join("f.txt"))
        .unwrap()
        .contains("<<<<<<<"));
}

#[test]
fn spurious_drop_conflict_is_auto_resolved() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // foo / +bar / +baz: C1 inserts `bar`, C2 inserts the adjacent `baz`. Dropping
    // C1 rebases C2 onto the base where `bar` is gone, so jj conflicts the (now
    // tip) C2 spuriously — yet "the surviving change with C1's removed" is
    // well-defined. The conflict lands on the tip itself, which the Drop strategy
    // tolerates.
    common::init_repo(
        dir,
        &[
            ("f.txt", "foo\n", "base"),
            ("f.txt", "foo\nbar\n", "C1-bar"),
            ("f.txt", "foo\nbar\nbaz\n", "C2-baz"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");

    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let from = commits.iter().position(|c| c.subject == "C1-bar").unwrap();
    let target = repo.plan_drop(&commits, from).expect("droppable");
    let outcome = repo.abandon_commit(&target).expect("drop");

    assert!(
        matches!(outcome, SaveOutcome::Clean),
        "expected the spurious drop to auto-resolve, got {outcome:?}"
    );
    assert!(!repo.is_pending(), "nothing should be left pending");

    // C1 is gone; C2's `baz` survives on top of the base, without `bar`.
    assert_eq!(common::git_log_subjects(dir), vec!["C2-baz", "base"]);
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "foo\nbaz");
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "foo");

    // Transparency invariants.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
    assert!(!std::fs::read_to_string(dir.join("f.txt"))
        .unwrap()
        .contains("<<<<<<<"));
}

#[test]
fn spurious_drop_then_restore_round_trips_via_auto_resolve() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Same spurious shape as the drop test. Drop C1 (auto-resolves to foo/baz),
    // then restore it to its original slot: re-inserting C1 below C2 re-applies
    // `bar` under C2's `baz`, conflicting the (now tip) C2 spuriously again. The
    // Restore strategy computes the expected tip by applying C1's change forward
    // onto the post-drop tip, recovering the original history.
    common::init_repo(
        dir,
        &[
            ("f.txt", "foo\n", "base"),
            ("f.txt", "foo\nbar\n", "C1-bar"),
            ("f.txt", "foo\nbar\nbaz\n", "C2-baz"),
        ],
    );
    let mut repo = Repo::open(dir).expect("open");

    // Drop C1-bar; remember its CommitInfo (its id stays resolvable, like the trash).
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let from = commits.iter().position(|c| c.subject == "C1-bar").unwrap();
    let c1 = commits[from].clone();
    let target = repo.plan_drop(&commits, from).expect("droppable");
    assert!(matches!(repo.abandon_commit(&target).expect("drop"), SaveOutcome::Clean));
    assert_eq!(common::git_log_subjects(dir), vec!["C2-baz", "base"]);

    // Restore C1-bar into the gap between C2-baz and base (its original slot).
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let mv = common::plan_restore_single(&repo, &commits, &c1, 1);
    let outcome = repo
        .restore_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("restore");

    assert!(
        matches!(outcome, SaveOutcome::Clean),
        "expected the spurious restore to auto-resolve, got {outcome:?}"
    );
    assert!(!repo.is_pending(), "nothing should be left pending");

    // Back to the original history, byte-for-byte.
    assert_eq!(common::git_log_subjects(dir), vec!["C2-baz", "C1-bar", "base"]);
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "foo\nbar\nbaz");
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "foo\nbar");

    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
    assert!(!std::fs::read_to_string(dir.join("f.txt"))
        .unwrap()
        .contains("<<<<<<<"));
}

#[test]
fn a_conflicted_rewrite_spanning_a_merge_falls_back_to_manual() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // base / +bar on main; `side` adds side.txt off base; the merge is *evil* —
    // it hand-inserts `baz` under main-1's `bar`. Dropping main-1 re-applies that
    // remerge delta where `bar` is gone, so the rebased merge itself conflicts.
    // The auto-resolver's rebuild would rewrite the conflicted range as a
    // single-parent chain — linearizing the 2-parent merge — so it must bail and
    // hand the conflict to the manual flow, leaving git untouched.
    common::init_repo(
        dir,
        &[
            ("f.txt", "foo\n", "base"),
            ("f.txt", "foo\nbar\n", "main-1"),
        ],
    );
    common::git(dir, &["checkout", "-q", "-b", "side", "main~1"]);
    std::fs::write(dir.join("side.txt"), "side\n").unwrap();
    common::git(dir, &["add", "side.txt"]);
    common::git(dir, &["commit", "-q", "-m", "side-1"]);
    common::git(dir, &["checkout", "-q", "main"]);
    common::git_allow_failure(dir, &["merge", "--no-ff", "--no-commit", "side"]);
    std::fs::write(dir.join("f.txt"), "foo\nbar\nbaz\n").unwrap();
    common::git(dir, &["add", "f.txt"]);
    common::git(dir, &["commit", "-q", "-m", "merge"]);
    let head_before = common::git(dir, &["rev-parse", "HEAD"]);
    let mut repo = Repo::open(dir).expect("open");

    // Drop main-1 by raw id (the merge sits between it and the tip, so no linear
    // plan exists — the engine API is what a DAG-aware caller would use).
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let main1 = commits.iter().find(|c| c.subject == "main-1").unwrap().id.clone();
    let outcome = repo.abandon_commit(&main1).expect("drop");

    assert!(
        matches!(outcome, SaveOutcome::Conflicts { .. }),
        "a conflicted range spanning a merge must go to manual resolution, got {outcome:?}"
    );
    assert!(repo.is_pending(), "the held-back rewrite leaves a pending resolution");
    // git is untouched while pending: same tip, the merge still a 2-parent merge.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert!(common::is_merge(dir, "HEAD"), "the merge keeps both parents");

    // Aborting rolls jj back; git never moved.
    repo.abort().expect("abort");
    assert!(!repo.is_pending());
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn a_true_drop_conflict_still_falls_back_to_manual() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // C1 and C2 both rewrite the *same* middle line, so removing C1 leaves C2's
    // edit dangling on content it never saw — a genuine conflict auto-resolution
    // must not paper over.
    common::init_repo(
        dir,
        &[
            ("f.txt", "1\n2\n3\n", "base"),
            ("f.txt", "1\nA\n3\n", "C1"),
            ("f.txt", "1\nB\n3\n", "C2"),
        ],
    );
    let head_before = common::git(dir, &["rev-parse", "HEAD"]);
    let mut repo = Repo::open(dir).expect("open");

    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let from = commits.iter().position(|c| c.subject == "C1").unwrap();
    let target = repo.plan_drop(&commits, from).expect("droppable");
    let outcome = repo.abandon_commit(&target).expect("drop");

    assert!(
        matches!(outcome, SaveOutcome::Conflicts { .. }),
        "a true conflict must still be held back for manual resolution, got {outcome:?}"
    );
    assert!(repo.is_pending(), "a true conflict leaves a pending resolution");
    // git is untouched while pending.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git_log_subjects(dir), vec!["C2", "C1", "base"]);
}

#[test]
fn a_true_reorder_conflict_still_falls_back_to_manual() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // A and B both rewrite the *same* middle line, so reordering genuinely
    // conflicts — the tip can't be the original and auto-resolution must not fire.
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

    let outcome = reorder_row_to_top(&mut repo, 1);

    assert!(
        matches!(outcome, SaveOutcome::Conflicts { .. }),
        "a true conflict must still be held back for manual resolution, got {outcome:?}"
    );
    assert!(repo.is_pending(), "a true conflict leaves a pending resolution");
    // git is untouched while pending.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A", "base"]);
}
