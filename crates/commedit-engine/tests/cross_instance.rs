//! Cross-instance support: editing one repository in several commedit windows
//! (one per branch). The shared object store gives each window the same stable
//! [`Repo::object_store_key`], and a commit on one branch can be cherry-picked
//! into another — the engine half of dragging a commit between windows.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::graph::compute_graph;
use commedit_engine::history::history;
use commedit_engine::repo::Repo;

/// Every view of one repository — the checked-out branch and an off-worktree
/// branch alike — reports the same object-store key, while a different repo
/// reports a different one. This is the identity a frontend uses to decide
/// whether a dragged-in commit can be cherry-picked from the shared ODB.
#[test]
fn object_store_key_is_shared_across_branch_views_and_unique_per_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    // A second branch on the same repo, opened off-worktree.
    common::git(dir, &["branch", "feature"]);

    let on_main = Repo::open(dir).expect("open main");
    let on_feature = Repo::open_branch(
        dir,
        commedit_engine::index_cache::IndexCache::Disabled,
        Some("feature"),
    )
    .expect("open feature off-worktree");

    let key_main = on_main.object_store_key().expect("main key");
    let key_feature = on_feature.object_store_key().expect("feature key");
    assert_eq!(
        key_main, key_feature,
        "two branch views of one repo share the object store, hence the key"
    );

    // A wholly separate repository must key differently.
    let other = tempfile::tempdir().unwrap();
    common::init_repo(other.path(), &[("z.txt", "z\n", "Z")]);
    let on_other = Repo::open(other.path()).expect("open other");
    assert_ne!(
        key_main,
        on_other.object_store_key().expect("other key"),
        "different repositories have different object stores, hence different keys"
    );
}

/// The engine half of dragging a commit between windows of one repo: a commit
/// living only on a sibling branch is found in the shared object store, its slot
/// at the top of the current branch is planned, and it is cherry-picked in —
/// rebasing onto the current tip while the sibling branch stays put.
#[test]
fn cherry_pick_a_commit_from_a_sibling_branch_into_the_current_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    // A sibling branch off "A" carrying its own commit "Feature", touching a
    // file the current branch never has — a clean pick.
    common::git(dir, &["checkout", "-q", "-b", "feature", "main~1"]);
    std::fs::write(dir.join("f.txt"), "f\n").unwrap();
    common::git(dir, &["add", "f.txt"]);
    common::git(dir, &["commit", "-q", "-m", "Feature"]);
    let feature_tip = common::git(dir, &["rev-parse", "feature"]);
    common::git(dir, &["checkout", "-q", "main"]);

    let mut repo = Repo::open(dir).expect("open main");

    // The sibling commit isn't on main, but is reachable in the shared ODB.
    let target = repo
        .lookup_commit_in_store(&feature_tip)
        .expect("the sibling commit is in the shared object store");

    // Plan its slot at the top of main (gap 0) and cherry-pick it in.
    let commits = history(&repo.repo, &repo.head_commit_id().unwrap()).expect("history");
    let layout = compute_graph(&commits, &repo.root_commit_id());
    let mut cands = repo.plan_cherry_pick_candidates(&commits, &layout, &target, 0);
    assert_eq!(
        cands.len(),
        1,
        "one line crosses the top gap on a linear main"
    );
    let mv = cands.remove(0).mv;
    let outcome = repo
        .cherry_pick_commit(&mv.target, mv.new_parents, mv.new_children, None)
        .expect("cherry-pick");
    assert!(
        matches!(outcome, SaveOutcome::Clean),
        "a disjoint pick is clean"
    );

    // main grew the picked commit on top; the sibling branch is untouched.
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["Feature", "B", "A"],
        "the cherry-picked commit lands on top of main"
    );
    assert_eq!(
        common::git(dir, &["rev-parse", "feature"]),
        feature_tip,
        "the source branch is a copy source only — it never moves"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}
