//! End-to-end: rewriting *merge* commits. Message, identity and (evil-merge)
//! content edits must preserve both parents and the merge topology; an ancestor
//! rewrite must keep a downstream merge a merge; and the structural operations
//! that have no single-parent splice (reorder/drop/squash) must stay refused.
//! Each test asserts the transparency triple and that no `.jjconflict-*` residue
//! leaks into git.

mod common;

use commedit_engine::diff::{commit_changes, ChangeKind};
use commedit_engine::history::{history, CommitInfo};
use commedit_engine::repo::Repo;
use commedit_engine::rewrite::Identity;

/// Look up a commit in `commits` by its subject line.
fn by<'a>(commits: &'a [CommitInfo], subject: &str) -> &'a CommitInfo {
    commits
        .iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("{subject:?} commit present"))
}

/// The current history of `repo` (ancestors of HEAD, newest first).
fn current(repo: &Repo) -> Vec<CommitInfo> {
    history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history")
}

/// The transparency triple every save must restore for a plain-git user, plus the
/// "no conflict residue in the tree" invariant.
fn assert_transparent(dir: &std::path::Path) {
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    let tree = common::git(dir, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree.contains(".jjconflict"), "no .jjconflict-* in the tree: {tree}");
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Subjects of HEAD's parents (first-parent, then second-parent, …), sorted.
fn parent_subjects(dir: &std::path::Path) -> Vec<String> {
    let count = common::parent_count(dir, "HEAD");
    let mut subjects: Vec<String> = (1..=count)
        .map(|p| common::git(dir, &["log", "-1", "--format=%s", &format!("HEAD^{p}")]))
        .collect();
    subjects.sort();
    subjects
}

#[test]
fn message_edit_preserves_both_parents() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "merge").id.clone();
    repo.rewrite_message(&merge, "merge (edited)").expect("rewrite message");

    // The tip is still the merge, now with the new subject and both parents.
    assert_eq!(common::git(dir, &["log", "-1", "--format=%s", "HEAD"]), "merge (edited)");
    assert!(common::is_merge(dir, "HEAD"), "tip stays a 2-parent merge");
    assert_eq!(parent_subjects(dir), vec!["main-1", "side-1"]);
    assert_transparent(dir);
}

#[test]
fn identity_edit_preserves_both_parents() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "merge").id.clone();
    let id = Identity {
        author_name: "Ada Lovelace".to_string(),
        author_email: "ada@example.com".to_string(),
        author_time: "2026-06-05 14:30:00 +0200".to_string(),
        committer_name: "Grace Hopper".to_string(),
        committer_email: "grace@example.com".to_string(),
        committer_time: "2026-06-06 09:00:00 +0000".to_string(),
    };
    repo.rewrite_identity(&merge, &id).expect("rewrite identity");

    // git sees the rewritten author/committer on the merge tip, both parents kept.
    let fmt = "%an|%ae|%ad|%cn|%ce|%cd";
    let line = common::git(
        dir,
        &["show", "-s", &format!("--format={fmt}"), "--date=format:%Y-%m-%d %H:%M:%S %z", "HEAD"],
    );
    let fields: Vec<&str> = line.split('|').collect();
    assert_eq!(fields[0], "Ada Lovelace");
    assert_eq!(fields[1], "ada@example.com");
    assert_eq!(fields[2], "2026-06-05 14:30:00 +0200");
    assert_eq!(fields[3], "Grace Hopper");
    assert_eq!(fields[4], "grace@example.com");
    assert_eq!(fields[5], "2026-06-06 09:00:00 +0000");
    assert!(common::is_merge(dir, "HEAD"), "tip stays a 2-parent merge");
    assert_eq!(parent_subjects(dir), vec!["main-1", "side-1"]);
    assert_transparent(dir);
}

#[test]
fn evil_merge_content_edit_keeps_both_parents() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_evil_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "evil-merge").id.clone();

    // Edit the merge's remerge delta (the evil change to base.txt).
    repo.rewrite_file(&merge, "base.txt", "1\nEVIL-EDITED\n3\n")
        .expect("rewrite file");

    assert_eq!(common::git(dir, &["show", "HEAD:base.txt"]), "1\nEVIL-EDITED\n3");
    assert!(common::is_merge(dir, "HEAD"), "tip stays a 2-parent merge");
    assert_eq!(parent_subjects(dir), vec!["main-1", "side-1"]);
    assert_transparent(dir);
}

#[test]
fn editing_a_non_merge_ancestor_keeps_the_merge_a_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    // "side-1" is reachable only via the merge's *second* parent — the classic
    // rebase-topology case. Its change id is stable, so we can re-find the merge.
    let commits = current(&repo);
    let side = by(&commits, "side-1").id.clone();
    let merge_change = by(&commits, "merge").change_id_hex();
    repo.rewrite_message(&side, "side-1 (edited)").expect("rewrite");

    // The merge survived the rebase as a 2-parent merge, now over the edited side.
    let after = current(&repo);
    let merge = after
        .iter()
        .find(|c| c.change_id_hex() == merge_change)
        .expect("merge still present");
    assert_eq!(merge.parents.len(), 2, "merge keeps both parents after the rebase");
    assert!(common::is_merge(dir, "HEAD"));
    assert_eq!(parent_subjects(dir), vec!["main-1", "side-1 (edited)"]);
    assert_transparent(dir);
}

#[test]
fn merge_survives_unrelated_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let commits = current(&repo);
    let main1 = by(&commits, "main-1").id.clone();
    let merge_change = by(&commits, "merge").change_id_hex();

    // Edit a first-parent mainline ancestor; the merge must stay reachable and a
    // merge (it is never abandoned — only rebased through the rewrite).
    repo.rewrite_message(&main1, "main-1 (edited)").expect("rewrite");

    assert_eq!(
        common::git(dir, &["rev-list", "--merges", "--count", "HEAD"]),
        "1",
        "exactly one merge survives in the rewritten history"
    );
    assert!(
        current(&repo).iter().any(|c| c.change_id_hex() == merge_change && c.parents.len() == 2),
        "the merge is still reachable from the new tip with both parents"
    );
    assert_transparent(dir);
}

#[test]
fn reorder_drop_squash_on_a_merge_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);
    // Put two linear commits on top of the merge, so the branch chain is the
    // run [tip-2, tip-1] above the merge — a merge that is *not* the branch tip.
    for (file, msg) in [("t1.txt", "tip-1"), ("t2.txt", "tip-2")] {
        std::fs::write(dir.join(file), "x\n").unwrap();
        common::git(dir, &["add", file]);
        common::git(dir, &["commit", "-q", "-m", msg]);
    }

    let repo = Repo::open(dir).expect("open");
    let commits = current(&repo);
    let pos = |s: &str| commits.iter().position(|c| c.subject == s).unwrap();
    let (merge, tip1, tip2) = (pos("merge"), pos("tip-1"), pos("tip-2"));

    // The merge sits off the editable single-parent chain, so every structural
    // op that needs a single-parent splice refuses to touch it…
    assert_eq!(repo.plan_drop(&commits, merge), None, "merge is not droppable");
    assert_eq!(repo.plan_reorder(&commits, merge, 0), None, "merge is not reorderable");
    assert_eq!(repo.plan_squash(&commits, merge, tip1), None, "merge is not a squash source");
    assert_eq!(repo.plan_squash(&commits, tip1, merge), None, "merge is not a squash target");

    // …while the linear commits above it remain fully operable — proving the
    // refusal is specific to the merge, not a blanket failure on a merge repo.
    assert!(repo.plan_drop(&commits, tip1).is_some(), "a linear commit is droppable");
    assert!(repo.plan_squash(&commits, tip2, tip1).is_some(), "linear commits squash");
}

#[test]
fn clean_merge_has_no_remerge_delta() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "merge").id.clone();
    // A clean merge's tree equals the auto-merge of its parents — nothing to edit.
    let changes = commit_changes(&repo.repo, &merge).expect("changes");
    assert!(changes.is_empty(), "clean merge has an empty remerge delta: {changes:?}");
}

#[test]
fn evil_merge_exposes_its_remerge_delta() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_evil_merge_repo(dir);

    let repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "evil-merge").id.clone();
    let changes = commit_changes(&repo.repo, &merge).expect("changes");

    // The merge's only delta vs. its (clean) auto-merged base is the evil edit.
    let base = changes.iter().find(|c| c.path == "base.txt").expect("base.txt delta");
    assert_eq!(base.kind, ChangeKind::Modified);
    assert_eq!(base.old_text.as_deref(), Some("1\n2\n3\n"));
    assert_eq!(base.new_text.as_deref(), Some("1\nEVIL\n3\n"));
    assert!(!base.conflicted_base, "a clean auto-merge has a resolvable base");
}

#[test]
fn conflicted_merge_base_is_flagged_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_conflicted_merge_repo(dir);

    let repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "conflict-merge").id.clone();
    let changes = commit_changes(&repo.repo, &merge).expect("changes");

    // The parents disagree at base.txt, so the auto-merged base is conflicted:
    // there is no single old side, hence the file is flagged not-editable.
    let base = changes.iter().find(|c| c.path == "base.txt").expect("base.txt delta");
    assert!(base.conflicted_base, "a disagreeing merge base is flagged conflicted");
    assert_eq!(base.new_text.as_deref(), Some("1\nRESOLVED\n3\n"));
}
