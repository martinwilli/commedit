//! The multi-branch DAG read: `Repo::history_multi` walks the union of several
//! branches' ancestries, and `Repo::local_branches` enumerates the dropdown
//! candidates. The extra branches are read-only — folding one into the view must
//! not entangle it in a later rewrite of the edited branch (the `sibling_branch`
//! invariant still holds), because the extra heads are made index-visible only in
//! a transient transaction that is rolled back.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::repo::Repo;

/// Build `main: A-B-C` (checked out) with `feature` branching off `B` and adding
/// its own commit `F`, then return the opened repo.
fn setup(dir: &std::path::Path) {
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "A"),
            ("b.txt", "b\n", "B"),
            ("c.txt", "c\n", "C"),
        ],
    );
    g(&["checkout", "-q", "-b", "feature", "HEAD~1"]); // at B
    std::fs::write(dir.join("f.txt"), "f\n").unwrap();
    g(&["add", "f.txt"]);
    g(&["commit", "-q", "-m", "F"]);
    g(&["checkout", "-q", "main"]); // import `main`, not `feature`
}

#[test]
fn history_multi_unions_branch_ancestries() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);

    let repo = Repo::open(dir).expect("open");

    // local_branches lists both, flags the edited one, and carries readable tips.
    let branches = repo.local_branches();
    let names: Vec<_> = branches.iter().map(|b| b.name.as_str()).collect();
    assert!(
        names.contains(&"main") && names.contains(&"feature"),
        "{names:?}"
    );
    let feature = branches.iter().find(|b| b.name == "feature").unwrap();
    let main = branches.iter().find(|b| b.name == "main").unwrap();
    assert!(main.is_current, "main is the edited branch");
    assert!(!feature.is_current, "feature is a view-only extra branch");

    // Single-head view: only main's chain.
    let head = repo.head_commit_id().expect("head");
    let (main_only, _) = repo
        .history_multi(std::slice::from_ref(&head), 0, usize::MAX)
        .unwrap();
    let mut subj: Vec<_> = main_only.iter().map(|c| c.subject.clone()).collect();
    subj.sort();
    assert_eq!(subj, vec!["A", "B", "C"], "single head walks only main");

    // Multi-head view: union of main and feature — F (only on feature) appears,
    // and a bare unimported-head walk would have failed without the transient
    // add_head, so reaching here at all proves the fix works.
    let (union, has_more) = repo
        .history_multi(&[head, feature.head.clone()], 0, usize::MAX)
        .unwrap();
    let mut subj: Vec<_> = union.iter().map(|c| c.subject.clone()).collect();
    subj.sort();
    assert_eq!(subj, vec!["A", "B", "C", "F"], "union of both branches");
    assert!(!has_more, "whole history fit under the limit");
}

#[test]
fn multi_head_read_does_not_entangle_a_later_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let feature_before = g(&["rev-parse", "feature"]);

    let mut repo = Repo::open(dir).expect("open");
    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;

    // Fold feature into the view (the transient add_head walk)...
    let (union, _) = repo
        .history_multi(&[head, feature_head], 0, usize::MAX)
        .unwrap();
    assert_eq!(union.len(), 4);

    // ...then rewrite a commit on the edited branch. `B` is a shared ancestor of
    // both main and feature; rewriting it must rebase main's descendants but
    // leave feature pinned, exactly as if it had never been folded in.
    let head = repo.head_commit_id().expect("head");
    let target = commedit_engine::history::history(&repo.repo, &head).expect("history")[1]
        .id
        .clone();
    repo.rewrite_message(&target, "B (edited)")
        .expect("rewrite");

    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C", "B (edited)", "A"],
        "main carries the edit"
    );
    assert_eq!(
        g(&["rev-parse", "feature"]),
        feature_before,
        "feature is left exactly where it was — the multi-head read left no residue"
    );
    g(&["fsck", "--no-progress"]);
}

/// Find the imported commit on the multi-branch DAG whose subject is `subject`.
fn find_commit(
    repo: &Repo,
    heads: &[jj_lib::backend::CommitId],
    subject: &str,
) -> jj_lib::backend::CommitId {
    let (union, _) = repo.history_multi(heads, 0, usize::MAX).expect("history");
    union
        .iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| {
            panic!(
                "no commit {subject:?} in {:?}",
                union.iter().map(|c| &c.subject).collect::<Vec<_>>()
            )
        })
        .id
        .clone()
}

/// Editing the editable set: open `main` (checked out) and `feature` (no
/// worktree) as one editable DAG. Rewriting a commit that lives **only** on
/// `feature` moves *that* branch's ref and rebases its descendants, while leaving
/// `main`, HEAD and the launch worktree untouched — the per-branch ref movement
/// the editable set delivers.
#[test]
fn editing_a_worktreeless_branch_moves_only_its_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let main_before = g(&["rev-parse", "main"]);
    let head_before = g(&["rev-parse", "HEAD"]);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");
    assert!(
        repo.is_worktree_bound(),
        "launch branch main is checked out"
    );

    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let heads = [head, feature_head];

    // Rewrite F (lives only on feature, no worktree).
    let f = find_commit(&repo, &heads, "F");
    repo.rewrite_message(&f, "F (edited)").expect("rewrite F");

    // feature carries the edit; main and the worktree are untouched.
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["F (edited)", "B", "A"],
        "feature ref moved to the rewritten F"
    );
    assert_eq!(g(&["rev-parse", "main"]), main_before, "main ref unmoved");
    assert_eq!(g(&["rev-parse", "HEAD"]), head_before, "HEAD frozen");
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C", "B", "A"],
        "the launch worktree branch (main) is untouched"
    );
    g(&["fsck", "--no-progress"]);
}

/// Rewriting a commit that is a **shared ancestor** of two editable branches
/// rewrites it for both: both bookmarks move and both branches' descendants
/// rebase. This is the inherent consequence of editing a unified DAG, and the
/// launch worktree (main) re-materializes since its tip moved.
#[test]
fn editing_a_shared_ancestor_moves_both_branches() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let heads = [head, feature_head];

    // B is the shared ancestor of main (A-B-C) and feature (A-B-F).
    let b = find_commit(&repo, &heads, "B");
    repo.rewrite_message(&b, "B (edited)").expect("rewrite B");

    assert_eq!(
        common::git_log_subjects_of(dir, "main"),
        vec!["C", "B (edited)", "A"],
        "main rebased onto the rewritten ancestor"
    );
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["F", "B (edited)", "A"],
        "feature rebased onto the same rewritten ancestor"
    );
    // The launch worktree tracks main, whose tip moved → HEAD follows.
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C", "B (edited)", "A"],
        "the launch worktree re-materialized onto main's new tip"
    );
    g(&["fsck", "--no-progress"]);
}

/// Open `main` + `feature` as one editable DAG and return the opened repo plus
/// the two branch tips (`[main_head, feature_head]`) the cross-branch planners
/// take. The `setup` layout is `main: A-B-C` (checked out), `feature: A-B-F`.
fn open_two_branch_dag(dir: &std::path::Path) -> (Repo, [jj_lib::backend::CommitId; 2]) {
    let repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");
    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    (repo, [head, feature_head])
}

/// Cross-branch **squash** (Phase 3 GTK center-drop): folding a commit on one
/// branch onto a commit on another lands across the boundary — the source is
/// consumed (its branch loses it), the destination's branch keeps its own tip
/// with the merged change. Drives the engine path the GTK squash arm uses
/// (`plan_squash_multi` → `squash_into`).
#[test]
fn a_cross_branch_squash_lands_and_consumes_the_source() {
    use commedit_engine::squash::SquashMode;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);

    let (mut repo, heads) = open_two_branch_dag(dir);
    // Fold F (feature-only) into C (main-only): different branches, shared
    // ancestor B. The single-head planner refuses it; the multi-head one allows it.
    let (union, _) = repo.history_multi(&heads, 0, usize::MAX).unwrap();
    let f_idx = union.iter().position(|c| c.subject == "F").unwrap();
    let c_idx = union.iter().position(|c| c.subject == "C").unwrap();
    assert!(
        repo.plan_squash(&union, f_idx, c_idx).is_none(),
        "single-head planner refuses the cross-branch squash"
    );
    let (src, dest) = repo
        .plan_squash_multi(&union, f_idx, c_idx)
        .expect("cross-branch squash planned");
    repo.squash_into(&src, &dest, SquashMode::Fixup, None)
        .expect("cross-branch squash");

    // main's C now carries f.txt; feature lost F (its tip is B again).
    assert_eq!(
        common::git_log_subjects_of(dir, "main"),
        vec!["C", "B", "A"],
        "main keeps its own tip C (Fixup keeps the message)"
    );
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["B", "A"],
        "feature lost F — the squash consumed the source"
    );
    assert_eq!(
        g(&["show", "main:f.txt"]),
        "f",
        "F's change folded into main's C"
    );
    g(&["fsck", "--no-progress"]);
}

/// Cross-branch **copy** (Phase 3 GTK Copy): cherry-picking a commit from one
/// branch onto another grows a re-applied copy and leaves the source branch
/// intact. Drives the engine path the GTK Copy popover uses
/// (`plan_cherry_pick_candidates_multi` → `cherry_pick_commit`).
#[test]
fn a_cross_branch_copy_leaves_the_source_intact() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let feature_before = g(&["rev-parse", "feature"]);

    let (mut repo, heads) = open_two_branch_dag(dir);
    let f = find_commit(&repo, &heads, "F");
    let main_head = heads[0].clone();
    // Cherry-pick F on top of main's tip C: a copy, source untouched.
    repo.cherry_pick_commit(&f, vec![main_head], vec![], None)
        .expect("cross-branch cherry-pick");

    assert_eq!(
        common::git_log_subjects_of(dir, "main"),
        vec!["F", "C", "B", "A"],
        "main grew a re-applied copy of F on top of C"
    );
    assert_eq!(
        g(&["rev-parse", "feature"]),
        feature_before,
        "feature is untouched — Copy leaves the source where it is"
    );
    g(&["fsck", "--no-progress"]);
}

/// Cross-branch **move** (Phase 3 GTK Move): reparenting a commit from one branch
/// onto another lifts it out of its old slot and reparents it onto the
/// destination lane. The jj/git-correct semantics: the commit's *own* bookmark
/// rides the rebase to the new location (a reparent doesn't transfer ownership),
/// while the primary bookmark — the explicit `new_tip` anchor — stays put. Drives
/// the engine path the GTK Move popover uses (`reorder_commit` with cross-lane
/// parents). Contrast [`a_cross_branch_copy_leaves_the_source_intact`]: Move
/// consumes the source slot, Copy leaves the original where it was.
#[test]
fn a_cross_branch_move_reparents_and_consumes_the_source() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let main_before = g(&["rev-parse", "main"]);

    let (mut repo, heads) = open_two_branch_dag(dir);
    let f = find_commit(&repo, &heads, "F");
    let main_head = heads[0].clone();
    // Move F (feature's tip, parented on the shared ancestor B) onto main's tip C.
    // F is lifted off B and reparented onto C; feature — the bookmark that owns F
    // — follows it there, so feature becomes F-C-B-A. main (the primary, pinned at
    // new_tip) is unchanged. F's old slot on B is gone: the source is consumed.
    repo.reorder_commit(&f, vec![main_head.clone()], vec![], &main_head)
        .expect("cross-branch move");

    // feature is now F-C-B-A: F was lifted off its old parent B (the source slot
    // is consumed) and reparented onto main's C, its bookmark riding along.
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["F", "C", "B", "A"],
        "F's bookmark (feature) rode the reparent onto main's C"
    );
    assert_eq!(
        g(&["rev-parse", "feature~1"]),
        main_before,
        "F's new parent is main's tip C (it left its old parent B)"
    );
    assert_eq!(
        g(&["rev-parse", "main"]),
        main_before,
        "main (the primary anchor) is left in place"
    );
    g(&["fsck", "--no-progress"]);
}

/// A 1-element editable set (`open_multi` with one branch) is byte-identical to
/// the classic `open_branch`: only that branch's ref moves, no sibling is
/// disturbed. Guards the singleton-equivalence the MCP relies on.
#[test]
fn a_singleton_set_behaves_like_classic_single_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let feature_before = g(&["rev-parse", "feature"]);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into()],
    )
    .expect("open singleton");

    let head = repo.head_commit_id().expect("head");
    let target = commedit_engine::history::history(&repo.repo, &head).expect("history")[0]
        .id
        .clone();
    repo.rewrite_message(&target, "C (edited)")
        .expect("rewrite");

    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C (edited)", "B", "A"],
        "main carries the edit"
    );
    assert_eq!(
        g(&["rev-parse", "feature"]),
        feature_before,
        "the unimported sibling feature is left exactly where it was"
    );
    g(&["fsck", "--no-progress"]);
}

/// Add a linked worktree on `feature` (the `setup` layout: `main: A-B-C` checked
/// out in `dir`, `feature: A-B-F`) at a path *outside* `dir` (git refuses nesting),
/// returning the linked worktree's canonical path. The `parent` TempDir must
/// outlive the worktree. Phase 1b maps it onto its own jj workspace.
fn add_feature_worktree(dir: &std::path::Path, parent: &tempfile::TempDir) -> std::path::PathBuf {
    let wt = parent.path().join("wt");
    common::git(dir, &["worktree", "add", wt.to_str().unwrap(), "feature"]);
    std::fs::canonicalize(&wt).unwrap()
}

/// Phase 1b: a rewrite touching the *second* worktree's branch updates **that**
/// worktree's files and index, while the launch worktree stays clean — and the
/// reverse holds for an edit on the launch branch.
#[test]
fn a_rewrite_materializes_only_the_touched_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let wt_parent = tempfile::tempdir().unwrap();
    let wt = add_feature_worktree(dir, &wt_parent);
    let head_before = g(&["rev-parse", "HEAD"]);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let heads = [head, feature_head];

    // Rewrite F's content (lives only on feature, checked out in the linked wt).
    let f = find_commit(&repo, &heads, "F");
    repo.rewrite_file(&f, "f.txt", "f rewritten\n")
        .expect("rewrite F");

    // The linked worktree followed the rewrite; its index is clean against the tip.
    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "f rewritten\n",
        "the feature worktree's file follows the rewrite"
    );
    assert_eq!(
        common::git(&wt, &["status", "--porcelain"]),
        "",
        "the feature worktree's index was reset to the rewritten tip"
    );
    // The launch worktree (main) is untouched: HEAD frozen, tree clean, f.txt absent.
    assert_eq!(g(&["rev-parse", "HEAD"]), head_before, "launch HEAD frozen");
    assert_eq!(g(&["status", "--porcelain"]), "", "launch worktree clean");
    assert!(!dir.join("f.txt").exists(), "f.txt is not on main");
    g(&["fsck", "--no-progress"]);
}

/// Phase 1b: uncommitted changes in *each* worktree survive a rewrite — every
/// worktree's `@` is snapshotted before the mutation and re-materialized after, so
/// neither the launch worktree's nor the linked worktree's edits are clobbered.
#[test]
fn uncommitted_changes_in_each_worktree_survive_a_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let wt_parent = tempfile::tempdir().unwrap();
    let wt = add_feature_worktree(dir, &wt_parent);

    // Dirty both worktrees on a tracked file before opening.
    std::fs::write(dir.join("a.txt"), "a dirty on main\n").unwrap();
    std::fs::write(wt.join("a.txt"), "a dirty on feature\n").unwrap();

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let heads = [head, feature_head];

    // Rewrite the shared ancestor B: both branches rebase, both worktrees move.
    let b = find_commit(&repo, &heads, "B");
    repo.rewrite_message(&b, "B (edited)").expect("rewrite B");

    // Each worktree's uncommitted edit to a.txt is preserved across the rewrite.
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a dirty on main\n",
        "main's uncommitted change survived"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "a dirty on feature\n",
        "the feature worktree's uncommitted change survived"
    );
    // Both branches carry the rewritten ancestor.
    assert_eq!(
        common::git_log_subjects_of(dir, "main"),
        vec!["C", "B (edited)", "A"]
    );
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["F", "B (edited)", "A"]
    );
    // Both worktrees still report exactly the one dirty file (index synced to tip).
    // (The `common::git` helper trims, so the porcelain " M a.txt" loses its lead.)
    assert_eq!(g(&["status", "--porcelain"]), "M a.txt");
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "M a.txt");
    g(&["fsck", "--no-progress"]);
}

/// Phase 1b: narrowing the editable set back to a single branch (no extra
/// worktree) still works and is byte-identical to the classic single-branch open —
/// the registration machinery cleanly degrades to nothing.
#[test]
fn narrowing_to_one_branch_still_works() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let wt_parent = tempfile::tempdir().unwrap();
    let wt = add_feature_worktree(dir, &wt_parent);
    let feature_before = g(&["rev-parse", "feature"]);
    let wt_status_before = common::git(&wt, &["status", "--porcelain"]);

    // Open just `main` even though `feature` has a worktree: feature is not in the
    // editable set, so no extra worktree is registered and it stays frozen.
    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into()],
    )
    .expect("open singleton");

    let head = repo.head_commit_id().expect("head");
    let c = commedit_engine::history::history(&repo.repo, &head).expect("history")[0]
        .id
        .clone();
    repo.rewrite_message(&c, "C (edited)").expect("rewrite");

    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C (edited)", "B", "A"],
        "main carries the edit"
    );
    assert_eq!(
        g(&["rev-parse", "feature"]),
        feature_before,
        "feature (not in the set) is left where it was"
    );
    assert_eq!(
        common::git(&wt, &["status", "--porcelain"]),
        wt_status_before,
        "the feature worktree is untouched"
    );
    g(&["fsck", "--no-progress"]);
}

/// Phase 2: `set_editable_branches` widens then narrows the set *in place* while
/// preserving the session undo op-log — a dropdown toggle must not reset undo/trash
/// the way a full reopen would. After an edit, widening (tick `feature`) and
/// narrowing back (untick it) leave the recorded op intact, so the edit still
/// undoes; widening also makes the new branch's commit editable, and narrowing
/// freezes it again.
#[test]
fn widen_then_narrow_preserves_the_undo_log() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let g = |args: &[&str]| common::git(dir, args);
    let feature_before = g(&["rev-parse", "feature"]);

    // Open just the launch branch (the GTK default: opened branch only).
    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into()],
    )
    .expect("open singleton");
    assert_eq!(repo.editable_branches(), vec!["main".to_string()]);

    // Land an edit on main, so there is a recorded op to step back to.
    let head = repo.head_commit_id().expect("head");
    let c = commedit_engine::history::history(&repo.repo, &head).expect("history")[0]
        .id
        .clone();
    repo.rewrite_message(&c, "C (edited)").expect("rewrite");
    assert_eq!(repo.session_ops().len(), 1, "one recorded op");
    assert!(repo.can_undo());
    let main_after_edit = g(&["rev-parse", "main"]);

    // Widen: tick `feature`. The op-log is untouched, and feature joins the set.
    repo.set_editable_branches(&["main".into(), "feature".into()])
        .expect("widen");
    assert_eq!(
        repo.editable_branches(),
        vec!["main".to_string(), "feature".to_string()]
    );
    assert_eq!(
        repo.session_ops().len(),
        1,
        "widening did not reset the op-log"
    );
    assert_eq!(g(&["rev-parse", "main"]), main_after_edit, "main unmoved");
    assert_eq!(
        g(&["rev-parse", "feature"]),
        feature_before,
        "feature unmoved"
    );

    // feature's commit F is now editable: rewriting it moves feature's ref.
    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let f = find_commit(&repo, &[head, feature_head], "F");
    repo.rewrite_message(&f, "F (edited)").expect("rewrite F");
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["F (edited)", "B", "A"],
        "feature is editable while ticked"
    );
    assert_eq!(repo.session_ops().len(), 2, "second op recorded");

    // Narrow back to just main: feature leaves the set and is frozen on its
    // (rewritten) tip — further edits to main never touch it.
    let feature_frozen = g(&["rev-parse", "feature"]);
    repo.set_editable_branches(&["main".into()])
        .expect("narrow");
    assert_eq!(repo.editable_branches(), vec!["main".to_string()]);
    assert_eq!(
        repo.session_ops().len(),
        2,
        "narrowing did not reset the op-log"
    );

    // The op-log survived both toggles: undo still steps the recorded edits back.
    repo.undo().expect("undo F");
    repo.undo().expect("undo C");
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["C", "B", "A"],
        "undo walked back the recorded edits across the toggles"
    );
    assert_eq!(
        g(&["rev-parse", "feature"]),
        feature_frozen,
        "feature, out of the set, stayed frozen on its tip"
    );
    g(&["fsck", "--no-progress"]);
}

/// Phase 2: the last-branch rule at the engine boundary — `set_editable_branches`
/// refuses to empty the set, mirroring the MCP's "the last session can't be closed".
#[test]
fn the_editable_set_cannot_be_emptied() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    let err = repo.set_editable_branches(&[]).unwrap_err().to_string();
    assert!(
        err.contains("cannot be emptied"),
        "refusal mentions the last-branch rule: {err}"
    );
    // The set is unchanged after the refusal.
    assert_eq!(
        repo.editable_branches(),
        vec!["main".to_string(), "feature".to_string()]
    );
}

/// `worktree_uncommitted` surfaces the launch worktree's `@` chain *and* every
/// extra worktree's dirty `@`, each keyed by its branch's short-name, launch
/// first. Dirtying happens before open so the open-time snapshot captures both
/// worktrees (`snapshot_working_copy` snapshots the extras first).
#[test]
fn worktree_uncommitted_lists_the_launch_and_each_dirty_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let wt_parent = tempfile::tempdir().unwrap();
    let wt = add_feature_worktree(dir, &wt_parent);

    std::fs::write(dir.join("a.txt"), "a dirty on main\n").unwrap();
    std::fs::write(wt.join("a.txt"), "a dirty on feature\n").unwrap();

    let repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    let wc = repo.worktree_uncommitted();
    assert_eq!(
        wc.iter().map(|(b, _)| b.as_str()).collect::<Vec<_>>(),
        vec!["main", "feature"],
        "the launch worktree comes first, then each extra worktree"
    );
    for (branch, entries) in &wc {
        assert_eq!(entries.len(), 1, "{branch}: a single dirty @");
        assert_eq!(
            entries[0].file_names,
            vec!["a.txt".to_string()],
            "{branch}: the one dirtied tracked file"
        );
        assert_eq!(entries[0].changed_files, 1, "{branch}: one changed file");
        assert!(!entries[0].has_conflict, "{branch}: a clean snapshot");
    }
}

/// A clean extra worktree contributes nothing — only the dirty launch worktree
/// is listed.
#[test]
fn worktree_uncommitted_skips_a_clean_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let wt_parent = tempfile::tempdir().unwrap();
    let _wt = add_feature_worktree(dir, &wt_parent); // feature checked out, left clean

    std::fs::write(dir.join("a.txt"), "a dirty on main\n").unwrap();

    let repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    assert_eq!(
        repo.worktree_uncommitted()
            .iter()
            .map(|(b, _)| b.as_str())
            .collect::<Vec<_>>(),
        vec!["main"],
        "the clean feature worktree is absent"
    );
}

/// An editable branch with no worktree (a pure ref-move) has no `@`, so it
/// contributes nothing even while it is in the editable set.
#[test]
fn worktree_uncommitted_ignores_a_worktreeless_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir); // `feature` exists as a ref only — never checked out anywhere

    std::fs::write(dir.join("a.txt"), "a dirty on main\n").unwrap();

    let repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    assert_eq!(
        repo.worktree_uncommitted()
            .iter()
            .map(|(b, _)| b.as_str())
            .collect::<Vec<_>>(),
        vec!["main"],
        "feature has no worktree, so no uncommitted @"
    );
}

/// A clean tree across every worktree yields no entries at all.
#[test]
fn worktree_uncommitted_is_empty_when_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    setup(dir);
    let wt_parent = tempfile::tempdir().unwrap();
    let _wt = add_feature_worktree(dir, &wt_parent);

    let repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    assert!(
        repo.worktree_uncommitted().is_empty(),
        "no uncommitted changes anywhere"
    );
}

/// Multi-head conflict *detection*: a rewrite of a *shared* ancestor that
/// conflicts only on a **sibling** branch's tip — the primary stays clean — must
/// still defer (git frozen) rather than silently exporting the conflicted
/// sibling. Before detection went multi-head it scanned only the primary chain
/// plus the launch `@`, so this exact case exported a conflicted commit to git.
#[test]
fn a_conflict_on_a_sibling_tip_defers_the_whole_export() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    // main: A(x.txt) - B(adds m.txt) - C(adds c.txt). `feature` branches at B and
    // edits x.txt's middle line.
    common::init_repo(
        dir,
        &[
            ("x.txt", "a\nb\nc\n", "A"),
            ("m.txt", "m\n", "B"),
            ("c.txt", "c\n", "C"),
        ],
    );
    g(&["checkout", "-q", "-b", "feature", "HEAD~1"]); // at B
    std::fs::write(dir.join("x.txt"), "a\nF\nc\n").unwrap();
    g(&["add", "x.txt"]);
    g(&["commit", "-q", "-m", "F"]);
    g(&["checkout", "-q", "main"]);

    let main_before = g(&["rev-parse", "main"]);
    let feature_before = g(&["rev-parse", "feature"]);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let heads = [head, feature_head];

    // Rewrite the shared ancestor B's x.txt middle line: main's C (touches only
    // c.txt) rebases clean, but feature's F (also edited that line) conflicts —
    // on the *sibling* tip, not the primary's.
    let b = find_commit(&repo, &heads, "B");
    let outcome = repo
        .rewrite_file(&b, "x.txt", "a\nB\nc\n")
        .expect("rewrite shared ancestor B");

    assert!(
        matches!(outcome, SaveOutcome::Conflicts { .. }),
        "a conflict on the sibling tip must defer, not export dirty"
    );
    assert!(
        repo.is_pending(),
        "the rewrite is held back pending resolution"
    );
    // git stays frozen: neither branch's ref moved while the chain is conflicted.
    assert_eq!(g(&["rev-parse", "main"]), main_before, "main ref frozen");
    assert_eq!(
        g(&["rev-parse", "feature"]),
        feature_before,
        "feature ref frozen"
    );
    g(&["fsck", "--no-progress"]);
}

/// Multi-head spurious auto-resolve (the data-loss-critical path): a *spurious*
/// drop on a **sibling** branch checked out in another worktree auto-resolves
/// cleanly, and that worktree's own dirty `@` rides onto the rebuilt tip —
/// reconstructed from *its* pre-rewrite `@`, not the launch worktree's.
#[test]
fn spurious_drop_on_a_sibling_worktree_auto_resolves_and_preserves_its_at() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    // base(f=foo) on main; `feature` branches there with the spurious shape
    // +bar / +baz (dropping `bar` leaves the well-defined foo/baz). main then adds
    // an unrelated commit so the two branches diverge above `base`.
    common::init_repo(dir, &[("f.txt", "foo\n", "base")]);
    g(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("f.txt"), "foo\nbar\n").unwrap();
    g(&["commit", "-aqm", "C1-bar"]);
    std::fs::write(dir.join("f.txt"), "foo\nbar\nbaz\n").unwrap();
    g(&["commit", "-aqm", "C2-baz"]);
    g(&["checkout", "-q", "main"]);
    std::fs::write(dir.join("m.txt"), "m\n").unwrap();
    g(&["add", "m.txt"]);
    g(&["commit", "-qm", "M"]);

    // Check feature out in a second worktree and dirty its @ (append `local`).
    let wt_parent = tempfile::tempdir().unwrap();
    let wt = add_feature_worktree(dir, &wt_parent);
    std::fs::write(wt.join("f.txt"), "foo\nbar\nbaz\nlocal\n").unwrap();

    let main_before = g(&["rev-parse", "main"]);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let heads = [head, feature_head];

    // Drop C1-bar on feature — a sibling-only spurious drop.
    let c1bar = find_commit(&repo, &heads, "C1-bar");
    let outcome = repo.abandon_commit(&c1bar).expect("drop C1-bar on feature");

    assert!(
        matches!(outcome, SaveOutcome::Clean),
        "a spurious sibling drop must auto-resolve, got {outcome:?}"
    );
    assert!(!repo.is_pending(), "nothing left pending");
    // feature: `bar` dropped, `baz` kept; tip is the well-defined foo/baz.
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["C2-baz", "base"]
    );
    assert_eq!(common::git(dir, &["show", "feature:f.txt"]), "foo\nbaz");
    // The sibling worktree's uncommitted delta (+local) rode onto the rebuilt tip —
    // replayed from feature's own pre-rewrite @, not main's launch @.
    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "foo\nbaz\nlocal\n",
        "the sibling worktree's uncommitted change survived the auto-resolve"
    );
    // main is untouched and the launch worktree stays clean and transparent.
    assert_eq!(g(&["rev-parse", "main"]), main_before, "main frozen");
    assert_eq!(g(&["status", "--porcelain"]), "", "launch worktree clean");
    g(&["fsck", "--no-progress"]);
}

/// A *genuine* conflict on a sibling drop must still fall back to manual: the
/// per-head auto-resolve bails (it can't prove the result well-defined), so the
/// whole rewrite defers with git frozen — no half-applied sibling rewrite.
#[test]
fn a_genuine_conflict_on_a_sibling_drop_falls_back_to_manual() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    // feature rewrites the SAME single line each commit, so dropping C1 leaves C2's
    // edit un-applicable onto the base — a genuine conflict.
    common::init_repo(dir, &[("f.txt", "one\n", "base")]);
    g(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("f.txt"), "two\n").unwrap();
    g(&["commit", "-aqm", "C1"]);
    std::fs::write(dir.join("f.txt"), "three\n").unwrap();
    g(&["commit", "-aqm", "C2"]);
    g(&["checkout", "-q", "main"]);

    let wt_parent = tempfile::tempdir().unwrap();
    let _wt = add_feature_worktree(dir, &wt_parent);
    let feature_before = g(&["rev-parse", "feature"]);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let c1 = find_commit(&repo, &[head, feature_head], "C1");
    let outcome = repo.abandon_commit(&c1).expect("drop C1 on feature");

    assert!(
        matches!(outcome, SaveOutcome::Conflicts { .. }),
        "a genuine sibling conflict must defer, got {outcome:?}"
    );
    assert!(repo.is_pending(), "held pending for manual resolution");
    assert_eq!(
        g(&["rev-parse", "feature"]),
        feature_before,
        "feature ref frozen — no half-applied rewrite"
    );
    g(&["fsck", "--no-progress"]);
}

/// Open `main` + `feature` (feature in a second worktree, dirty) and return the
/// opened repo plus the worktree path. Shared by the sibling working-copy
/// mutation tests below.
fn open_with_dirty_feature_worktree(
    dir: &std::path::Path,
    wt_parent: &tempfile::TempDir,
    f_contents: &str,
) -> (Repo, std::path::PathBuf) {
    setup(dir);
    let wt = add_feature_worktree(dir, wt_parent);
    std::fs::write(wt.join("f.txt"), f_contents).unwrap();
    let repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");
    (repo, wt)
}

/// A `@`-only edit to a *sibling* worktree's working copy rewrites that
/// worktree's file on disk, leaves its branch tip put, and never touches main.
#[test]
fn editing_a_sibling_worktrees_uncommitted_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let wt_parent = tempfile::tempdir().unwrap();
    let (mut repo, wt) = open_with_dirty_feature_worktree(dir, &wt_parent, "f edited on disk\n");

    let target = repo
        .wc_target_for_branch("feature")
        .expect("feature has a worktree");
    repo.edit_working_copy_file_at(target, None, "f.txt", Some("f set by edit\n"))
        .expect("edit the sibling @");

    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "f set by edit\n",
        "the sibling worktree's file reflects the edit"
    );
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "M f.txt");
    // The @-only edit leaves feature's tip and main untouched.
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["F", "B", "A"]
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Discarding a sibling worktree's uncommitted changes resets *its* `@` to its
/// tip (jj recreates the per-worktree `@`) and re-materializes that worktree.
#[test]
fn discarding_a_sibling_worktrees_uncommitted_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let wt_parent = tempfile::tempdir().unwrap();
    let (mut repo, wt) = open_with_dirty_feature_worktree(dir, &wt_parent, "f dirty\n");

    let target = repo.wc_target_for_branch("feature").unwrap();
    repo.drop_working_copy_at(target, None)
        .expect("discard the sibling @");

    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "f\n",
        "the sibling worktree reverts to its tip"
    );
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Folding a sibling worktree's uncommitted changes into one of its commits
/// rewrites that commit, re-materializes the worktree onto the rewritten tip, and
/// leaves main alone.
#[test]
fn folding_a_sibling_worktrees_uncommitted_changes_into_its_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let wt_parent = tempfile::tempdir().unwrap();
    let (mut repo, wt) = open_with_dirty_feature_worktree(dir, &wt_parent, "f\nmore\n");

    let head = repo.head_commit_id().unwrap();
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let f = find_commit(&repo, &[head, feature_head], "F");
    let target = repo.wc_target_for_branch("feature").unwrap();
    let outcome = repo
        .squash_working_copy_into_at(target, None, &f, None)
        .expect("fold the sibling @ into F");

    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");
    assert_eq!(common::git(dir, &["show", "feature:f.txt"]), "f\nmore");
    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "f\nmore\n",
        "the worktree sits clean on the rewritten tip"
    );
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "");
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["F", "B", "A"],
        "F amended in place"
    );
    assert_eq!(
        common::git_log_subjects_of(dir, "main"),
        vec!["C", "B", "A"]
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Committing a sibling worktree's uncommitted changes crystallizes a new commit
/// on *that* branch's tip, leaves its worktree clean, and never moves main.
#[test]
fn committing_a_sibling_worktrees_uncommitted_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let wt_parent = tempfile::tempdir().unwrap();
    let (mut repo, wt) = open_with_dirty_feature_worktree(dir, &wt_parent, "f\nnew\n");

    let target = repo.wc_target_for_branch("feature").unwrap();
    let outcome = repo
        .commit_working_copy_at(target, "feature WIP", None)
        .expect("commit the sibling @");

    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["feature WIP", "F", "B", "A"],
        "a new commit on feature's tip"
    );
    assert_eq!(common::git(dir, &["show", "feature:f.txt"]), "f\nnew");
    assert_eq!(
        common::git(&wt, &["status", "--porcelain"]),
        "",
        "the worktree is clean after committing its @"
    );
    assert_eq!(
        common::git_log_subjects_of(dir, "main"),
        vec!["C", "B", "A"]
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Bug 3: a *clean* `@`-only edit on a sibling worktree is recorded as a session
/// op, so it can be undone — the record decision keys on the mutated worktree's
/// `@` (clean here), not the launch `@`. Before the fix `record_working_copy_op`
/// inspected only the launch chain, so a sibling edit's op recording was decided
/// by the wrong working copy.
#[test]
fn a_clean_sibling_edit_is_undoable() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let wt_parent = tempfile::tempdir().unwrap();
    let (mut repo, _wt) = open_with_dirty_feature_worktree(dir, &wt_parent, "f dirty\n");

    assert!(!repo.can_undo(), "no recorded op before the edit");
    let target = repo.wc_target_for_branch("feature").unwrap();
    repo.edit_working_copy_file_at(target, None, "f.txt", Some("f set by edit\n"))
        .expect("edit the sibling @");

    assert!(
        repo.can_undo(),
        "the clean sibling edit was recorded as a session op"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Bug 1: an `@`-only discard on a sibling worktree (its tip does not move)
/// followed by undo/redo keeps that worktree's files in sync with the DAG. The
/// rewind path re-materializes a sibling whose `@` changed, not just one whose
/// branch tip moved — before the fix undo restored the sibling `@` in jj but
/// left the worktree's files stale on disk.
#[test]
fn an_at_only_sibling_drop_then_undo_redo_keeps_the_worktree_in_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let wt_parent = tempfile::tempdir().unwrap();
    let (mut repo, wt) = open_with_dirty_feature_worktree(dir, &wt_parent, "f dirty\n");

    // Discard the sibling @: the worktree reverts to its tip content (passes today).
    let target = repo.wc_target_for_branch("feature").unwrap();
    repo.drop_working_copy_at(target, None)
        .expect("discard the sibling @");
    assert_eq!(std::fs::read_to_string(wt.join("f.txt")).unwrap(), "f\n");
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "");

    // Undo: the discarded uncommitted change must come back on disk AND in the
    // worktree's index (this is what failed before the fix).
    repo.undo().expect("undo the discard");
    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "f dirty\n",
        "undo restored the sibling worktree's uncommitted change on disk"
    );
    assert_eq!(
        common::git(&wt, &["status", "--porcelain"]),
        "M f.txt",
        "and its git index reflects the restored change"
    );

    // Redo: the discard reapplies, reverting the worktree again.
    repo.redo().expect("redo the discard");
    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "f\n",
        "redo reverted the sibling worktree again"
    );
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "");
    // main never moved through any of this.
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Bug 1: an `@`-only edit on a sibling worktree, then `revert_all()` (→
/// `set_op_cursor(0)` → `rewind_to_op`) resets that worktree on disk back to its
/// session-start uncommitted state.
#[test]
fn an_at_only_sibling_edit_then_revert_all_resets_the_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let wt_parent = tempfile::tempdir().unwrap();
    // Session starts with the sibling worktree dirty at "f start\n".
    let (mut repo, wt) = open_with_dirty_feature_worktree(dir, &wt_parent, "f start\n");

    let target = repo.wc_target_for_branch("feature").unwrap();
    repo.edit_working_copy_file_at(target, None, "f.txt", Some("f edited\n"))
        .expect("edit the sibling @");
    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "f edited\n"
    );

    repo.revert_all().expect("revert the whole session");
    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "f start\n",
        "revert_all reset the sibling worktree to its session-start @"
    );
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "M f.txt");
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Bug 1 guard: a *tip-moving* sibling op (fold its `@` into a commit) still
/// undoes/redoes correctly — the new `@`-aware rewind gate must not regress the
/// existing tip-move materialize path (which `export_and_sync` already handles).
#[test]
fn undo_redo_across_a_tip_moving_sibling_op_still_works() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let wt_parent = tempfile::tempdir().unwrap();
    let (mut repo, wt) = open_with_dirty_feature_worktree(dir, &wt_parent, "f\nmore\n");

    let head = repo.head_commit_id().unwrap();
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let f = find_commit(&repo, &[head, feature_head], "F");
    let target = repo.wc_target_for_branch("feature").unwrap();
    repo.squash_working_copy_into_at(target, None, &f, None)
        .expect("fold the sibling @ into F");
    assert_eq!(common::git(dir, &["show", "feature:f.txt"]), "f\nmore");
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "");

    // Undo the fold: F reverts and the worktree's uncommitted change comes back.
    repo.undo().expect("undo the fold");
    assert_eq!(common::git(dir, &["show", "feature:f.txt"]), "f");
    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "f\nmore\n",
        "undo restored the worktree's uncommitted change on the original tip"
    );
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "M f.txt");

    // Redo the fold: back to the rewritten tip with a clean worktree.
    repo.redo().expect("redo the fold");
    assert_eq!(common::git(dir, &["show", "feature:f.txt"]), "f\nmore");
    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "f\nmore\n"
    );
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Bug 2: a *genuine* conflict on a sibling-branch drop is now resolvable in-app,
/// not abort-only. The drop defers (the auto-resolve bails on a real conflict);
/// the conflicted commit lives on the *sibling* branch, so the resolver must
/// search every editable head, not just the primary — before the fix
/// `read_conflict`/`resolve_conflict` bailed "not on the current branch chain".
#[test]
fn a_genuine_conflict_on_a_sibling_drop_resolves_manually() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    // feature rewrites the SAME single line each commit, so dropping C1 leaves C2's
    // edit un-applicable onto the base — a genuine conflict on the sibling.
    common::init_repo(dir, &[("f.txt", "one\n", "base")]);
    g(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("f.txt"), "two\n").unwrap();
    g(&["commit", "-aqm", "C1"]);
    std::fs::write(dir.join("f.txt"), "three\n").unwrap();
    g(&["commit", "-aqm", "C2"]);
    g(&["checkout", "-q", "main"]);

    let wt_parent = tempfile::tempdir().unwrap();
    let _wt = add_feature_worktree(dir, &wt_parent);
    let main_before = g(&["rev-parse", "main"]);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    let c1 = find_commit(&repo, &[head, feature_head], "C1");
    let outcome = repo.abandon_commit(&c1).expect("drop C1 on feature");
    let conflicted = match outcome {
        SaveOutcome::Conflicts { commits } => commits,
        other => panic!("expected a deferred conflict, got {other:?}"),
    };
    // The conflict is C2, on the sibling branch — its change id drives resolution.
    let c2_change = conflicted
        .iter()
        .find(|c| c.subject == "C2")
        .expect("C2 is the conflicted commit")
        .change_id_hex();

    // read_conflict reaches the sibling commit and returns Git-style markers
    // (this bailed before the resolver searched every editable head).
    let cf = repo
        .read_conflict(&c2_change, "f.txt")
        .expect("read the sibling conflict");
    assert!(
        cf.text.contains("<<<<<<<") && cf.text.contains(">>>>>>>"),
        "materialized conflict markers: {:?}",
        cf.text
    );

    // Resolve to the intended content; the whole chain goes clean and exports.
    let outcome = repo
        .resolve_conflict(&c2_change, "f.txt", "three\n", cf.marker_len)
        .expect("resolve the sibling conflict");
    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");
    assert!(!repo.is_pending());
    assert_eq!(
        common::git_log_subjects_of(dir, "feature"),
        vec!["C2", "base"],
        "C1 dropped, C2 resolved onto base"
    );
    assert_eq!(common::git(dir, &["show", "feature:f.txt"]), "three");
    assert_eq!(g(&["rev-parse", "main"]), main_before, "main frozen");
    g(&["fsck", "--no-progress"]);
}

/// Bug 2: a conflicted *sibling worktree `@`* (not a branch commit) is resolvable.
/// Rewriting a sibling commit so re-applying that worktree's uncommitted `@` no
/// longer merges leaves the `@` itself conflicted; the resolver locates it among
/// the worktree `@`s and the resolved content lands on disk in that worktree.
#[test]
fn a_conflicted_sibling_at_is_resolvable() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    // main stays at base; feature adds F editing line 1.
    common::init_repo(dir, &[("f.txt", "a\nb\nc\n", "base")]);
    g(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("f.txt"), "A\nb\nc\n").unwrap();
    g(&["commit", "-aqm", "F"]);
    g(&["checkout", "-q", "main"]);

    // feature in a second worktree, with an uncommitted edit to the SAME line.
    let wt_parent = tempfile::tempdir().unwrap();
    let wt = add_feature_worktree(dir, &wt_parent);
    std::fs::write(wt.join("f.txt"), "AA\nb\nc\n").unwrap();
    let main_before = g(&["rev-parse", "main"]);

    let mut repo = Repo::open_multi(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        &["main".into(), "feature".into()],
    )
    .expect("open multi");

    let head = repo.head_commit_id().expect("head");
    let feature_head = repo
        .local_branches()
        .into_iter()
        .find(|b| b.name == "feature")
        .unwrap()
        .head;
    // Rewrite F's line 1 too: the sibling worktree's `@` (line 1 "A"->"AA") can no
    // longer be re-applied onto the rewritten tip ("X"), so the `@` conflicts.
    let f = find_commit(&repo, &[head, feature_head], "F");
    let outcome = repo
        .rewrite_file(&f, "f.txt", "X\nb\nc\n")
        .expect("rewrite F");
    let conflicted = match outcome {
        SaveOutcome::Conflicts { commits } => commits,
        other => panic!("expected a deferred conflict, got {other:?}"),
    };
    // The single conflict is the sibling worktree's uncommitted `@`.
    assert_eq!(
        conflicted.len(),
        1,
        "only the @ conflicts, F is a clean rewrite"
    );
    assert_eq!(conflicted[0].subject, "Uncommitted changes");
    let at_change = conflicted[0].change_id_hex();

    let cf = repo
        .read_conflict(&at_change, "f.txt")
        .expect("read the sibling @ conflict");
    assert!(
        cf.text.contains("<<<<<<<"),
        "materialized markers: {:?}",
        cf.text
    );

    let outcome = repo
        .resolve_conflict(&at_change, "f.txt", "RESOLVED\nb\nc\n", cf.marker_len)
        .expect("resolve the sibling @");
    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");
    assert!(!repo.is_pending());
    // The resolved content is materialized into that worktree on disk.
    assert_eq!(
        std::fs::read_to_string(wt.join("f.txt")).unwrap(),
        "RESOLVED\nb\nc\n",
        "the sibling worktree holds the resolved @ content"
    );
    assert_eq!(common::git(dir, &["show", "feature:f.txt"]), "X\nb\nc");
    assert_eq!(g(&["rev-parse", "main"]), main_before, "main frozen");
    g(&["fsck", "--no-progress"]);
}
