//! Cross-instance support: editing one repository in several commedit windows
//! (one per branch). The shared object store gives each window the same stable
//! [`Repo::object_store_key`], and a commit on one branch can be cherry-picked
//! into another — the engine half of dragging a commit between windows.

mod common;

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
