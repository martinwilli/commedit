//! The multi-branch DAG read: `Repo::history_multi` walks the union of several
//! branches' ancestries, and `Repo::local_branches` enumerates the dropdown
//! candidates. The extra branches are read-only — folding one into the view must
//! not entangle it in a later rewrite of the edited branch (the `sibling_branch`
//! invariant still holds), because the extra heads are made index-visible only in
//! a transient transaction that is rolled back.

mod common;

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
