//! The git-level backstop that guarantees a rewrite only moves the checked-out
//! branch: whatever nudges an unrelated branch, `restore_unrelated_heads`
//! reverts it before the user sees it.

mod common;

use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use commedit_engine::transparency::{
    local_head_oids, ref_decorations, restore_unrelated_heads, RefKind,
};

/// jj's own refs — its `refs/jj/keep/*` GC anchors above all — must never appear
/// in the user's repository: not after a rewrite, and not even during a
/// browse-only session. jj writes its refs into a session-local git dir whose
/// object store alone is shared with the user's repo (see `Repo::init_detached`),
/// so the rewritten *objects* reach the user's ODB while the refs stay out. The
/// one branch ref jj moves is mirrored out explicitly (`bridge_branch_to_git`).
#[test]
fn jj_refs_never_appear_in_the_user_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );
    let assert_no_jj_refs = |when: &str| {
        assert_eq!(
            common::git(dir, &["for-each-ref", "--format=%(refname)", "refs/jj/"]),
            "",
            "refs/jj/* leaked into the user repo {when}"
        );
        assert!(!dir.join(".jj").exists(), ".jj leaked into the user repo {when}");
    };

    let mut repo = Repo::open(dir).expect("open");
    // Open snapshots the working copy into jj's @ (a keep-ref in jj's git dir);
    // the user's repo must still be clean of refs/jj on a browse-only session.
    assert_no_jj_refs("right after open (browse-only)");

    // A real rewrite: objects land in the user's shared ODB (git sees the new
    // history below) but still no refs/jj.
    let head = repo.head_commit_id().expect("head");
    let target = history(&repo.repo, &head)
        .expect("history")
        .into_iter()
        .find(|c| c.subject == "B")
        .expect("B present")
        .id;
    repo.rewrite_message(&target, "B (edited)").expect("rewrite");

    assert_eq!(common::git_log_subjects(dir), vec!["C", "B (edited)", "A"]);
    assert_no_jj_refs("after a rewrite");
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    common::git(dir, &["fsck", "--no-progress"]);

    // Nothing persists once the session closes (jj's temp git dir is removed).
    drop(repo);
    assert_no_jj_refs("after the session closed");
}

#[test]
fn restores_an_unrelated_branch_but_leaves_the_current_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );
    g(&["branch", "backup"]); // at the tip, like main

    // Before-image: both branches at C.
    let before = local_head_oids(dir);
    let tip = g(&["rev-parse", "main"]);

    // Simulate the leak the backstop exists to undo: an unrelated branch
    // dragged back, *and* a legitimate move of the current branch.
    g(&["update-ref", "refs/heads/backup", "main~1"]);
    g(&["update-ref", "refs/heads/main", "main~2"]);

    let restored = restore_unrelated_heads(dir, Some("refs/heads/main"), &before);

    // Only the unrelated branch is reverted; the current branch keeps its move.
    assert_eq!(restored, vec!["refs/heads/backup".to_string()]);
    assert_eq!(g(&["rev-parse", "backup"]), tip, "backup restored to its tip");
    assert_ne!(g(&["rev-parse", "main"]), tip, "current branch left alone");
}

#[test]
fn recreates_a_branch_that_was_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    g(&["branch", "backup"]);
    let before = local_head_oids(dir);
    let tip = g(&["rev-parse", "backup"]);

    g(&["branch", "-D", "backup"]);
    let restored = restore_unrelated_heads(dir, Some("refs/heads/main"), &before);

    assert_eq!(restored, vec!["refs/heads/backup".to_string()]);
    assert_eq!(g(&["rev-parse", "backup"]), tip, "deleted branch recreated");
}

#[test]
fn no_op_when_nothing_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(dir, &[("a.txt", "a\n", "A")]);
    g(&["branch", "backup"]);
    let before = local_head_oids(dir);

    let restored = restore_unrelated_heads(dir, Some("refs/heads/main"), &before);
    assert!(restored.is_empty(), "untouched branches need no restoring");
}

/// `ref_decorations` groups every local branch and tag by the commit it points
/// at, peeling annotated tags to their target commit — the data behind the
/// history view's ref pills. jj's branch-scoped import can't supply this, so it
/// reads the user's git refs directly.
#[test]
fn ref_decorations_group_branches_and_tags_by_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    g(&["branch", "feature", "main~1"]);
    g(&["tag", "light", "main~1"]); // lightweight: points at the commit itself
    g(&["tag", "-a", "-m", "release", "v1.0", "main"]); // annotated: needs peeling
    let tip = g(&["rev-parse", "main"]);
    let below = g(&["rev-parse", "main~1"]);

    let decorations = ref_decorations(dir);

    let names = |oid: &str| -> Vec<(String, RefKind)> {
        decorations
            .get(oid)
            .map(|ds| ds.iter().map(|d| (d.name.clone(), d.kind)).collect())
            .unwrap_or_default()
    };
    assert_eq!(
        names(&tip),
        vec![("main".to_string(), RefKind::Branch), ("v1.0".to_string(), RefKind::Tag)]
    );
    assert_eq!(
        names(&below),
        vec![("feature".to_string(), RefKind::Branch), ("light".to_string(), RefKind::Tag)]
    );
}

/// `Repo::commit_refs` reads the refs live, so after a clean rewrite the
/// checked-out branch's pill follows the branch to the rewritten tip while a
/// tag stays on the commit it named — which has left the visible history.
#[test]
fn commit_refs_track_a_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    g(&["tag", "v1.0", "main"]);
    let old_tip = g(&["rev-parse", "main"]);

    let mut repo = Repo::open(dir).expect("open");
    let head = repo.head_commit_id().expect("head");
    repo.rewrite_message(&head, "B (edited)").expect("rewrite");

    let refs = repo.commit_refs();
    let new_tip = g(&["rev-parse", "main"]);
    assert_ne!(new_tip, old_tip);
    let kinds = |oid: &str| -> Vec<(String, RefKind)> {
        refs.get(oid)
            .map(|ds| ds.iter().map(|d| (d.name.clone(), d.kind)).collect())
            .unwrap_or_default()
    };
    assert_eq!(kinds(&new_tip), vec![("main".to_string(), RefKind::Branch)]);
    assert_eq!(kinds(&old_tip), vec![("v1.0".to_string(), RefKind::Tag)]);
}

/// `Repo::commit_refs` flags only the checked-out branch's pill as `current`,
/// so the UI can colour it distinctly — a sibling branch and a tag stay plain.
#[test]
fn commit_refs_flag_the_checked_out_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    g(&["branch", "feature", "main~1"]); // a sibling branch, not checked out
    g(&["tag", "v1.0", "main"]);

    let repo = Repo::open(dir).expect("open");
    let refs = repo.commit_refs();

    let current = |name: &str| -> bool {
        refs.values()
            .flatten()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("no decoration {name}"))
            .current
    };
    assert!(current("main"), "checked-out branch flagged");
    assert!(!current("feature"), "sibling branch not flagged");
    assert!(!current("v1.0"), "tag not flagged");
}
