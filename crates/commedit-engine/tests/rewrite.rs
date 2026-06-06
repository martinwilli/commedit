//! End-to-end: rewrite a middle commit's message and confirm plain `git` sees
//! the rewritten history (descendants rebased, branch moved).

mod common;

use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use commedit_engine::rewrite::Identity;

#[test]
fn rewrites_middle_commit_message_visible_to_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");

    // Find the middle commit ("second").
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let target = commits
        .iter()
        .find(|c| c.subject == "second")
        .expect("second commit present")
        .id
        .clone();

    repo.rewrite_message(&target, "second (edited)")
        .expect("rewrite message");

    // Plain git must see the rewritten message with descendants preserved.
    let subjects = common::git_log_subjects(dir);
    assert_eq!(subjects, vec!["third", "second (edited)", "first"]);

    // Transparency invariants: HEAD attached to the original branch, and a
    // clean working tree — a plain-git user sees nothing unusual.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");

    // Repository must remain intact.
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn rewrites_author_and_committer_identity_visible_to_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let target = commits
        .iter()
        .find(|c| c.subject == "first")
        .expect("first commit present")
        .id
        .clone();

    let id = Identity {
        author_name: "Ada Lovelace".to_string(),
        author_email: "ada@example.com".to_string(),
        author_time: "2026-06-05 14:30:00 +0200".to_string(),
        committer_name: "Grace Hopper".to_string(),
        committer_email: "grace@example.com".to_string(),
        committer_time: "2026-06-06 09:00:00 +0000".to_string(),
    };
    repo.rewrite_identity(&target, &id).expect("rewrite identity");

    // Plain git must see the rewritten author/committer and dates. The rewritten
    // commit is the history root, so resolve it via the first-parent chain.
    let root = common::git(dir, &["rev-list", "--max-parents=0", "HEAD"]);
    let fmt = "%an|%ae|%ad|%cn|%ce|%cd";
    let line = common::git(
        dir,
        &["show", "-s", &format!("--format={fmt}"), "--date=format:%Y-%m-%d %H:%M:%S %z", &root],
    );
    let fields: Vec<&str> = line.split('|').collect();
    assert_eq!(fields[0], "Ada Lovelace");
    assert_eq!(fields[1], "ada@example.com");
    assert_eq!(fields[2], "2026-06-05 14:30:00 +0200");
    assert_eq!(fields[3], "Grace Hopper");
    assert_eq!(fields[4], "grace@example.com");
    assert_eq!(fields[5], "2026-06-06 09:00:00 +0000");

    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn reorders_commit_to_a_new_position_visible_to_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history"); // [third, second, first]
    let by = |s: &str| {
        commits
            .iter()
            .find(|c| c.subject == s)
            .unwrap_or_else(|| panic!("{s} commit present"))
    };
    let third = by("third");
    let second = by("second");
    let first = by("first");

    // Move "third" (the tip) down to the oldest position: parent the root, with
    // "first" rebased on top of it, so "second" becomes the new head.
    repo.reorder_commit(
        &third.id,
        first.parents.clone(),
        vec![first.id.clone()],
        &second.id,
    )
    .expect("reorder");

    // The branch now reads second <- first <- third <- root, and the diffs were
    // re-applied (distinct files commute, so nothing is empty or conflicted).
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["second", "first", "third"]
    );

    // Transparency invariants: HEAD attached, clean tree, intact repo.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn history_has_no_duplicate_rows_after_a_reorder() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "A"),
            ("b.txt", "b\n", "B"),
            ("c.txt", "c\n", "C"),
            ("d.txt", "d\n", "D"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let from = commits.iter().position(|c| c.subject == "D").unwrap();
    let mv = repo
        .plan_reorder(&commits, from, commits.len())
        .expect("plan");
    repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder");

    // The reorder abandons the pre-reorder commits, which jj keeps pinned (via
    // `git_head` and `refs/jj/keep/*`). The history view must not surface those
    // as duplicate rows — exactly four distinct commits, one per subject.
    let after = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let mut subjects: Vec<_> = after.iter().map(|c| c.subject.clone()).collect();
    subjects.sort();
    assert_eq!(subjects, vec!["A", "B", "C", "D"]);
    let unique_ids: std::collections::HashSet<_> = after.iter().map(|c| c.id.clone()).collect();
    assert_eq!(unique_ids.len(), after.len());

    // The plain-git side must be clean too: no `refs/jj/keep/*` clutter and no
    // unreachable pre-reorder commits surfacing in `git log --all`.
    assert_eq!(
        common::git(dir, &["for-each-ref", "--format=%(refname)", "refs/jj/keep/"]),
        ""
    );
    let mut all_subjects: Vec<_> = common::git(dir, &["log", "--all", "--format=%s"])
        .lines()
        .map(str::to_string)
        .collect();
    all_subjects.sort();
    assert_eq!(all_subjects, vec!["A", "B", "C", "D"]);
}

#[test]
fn keep_ref_for_a_manual_jj_anonymous_head_is_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );

    // Simulate a manual jj user's un-bookmarked work: a commit off B with no
    // branch pointing at it, protected only by a `refs/jj/keep/*` ref — exactly
    // how jj guards anonymous heads from `git gc`.
    g(&["checkout", "-q", "-b", "tmp", "main~1"]);
    std::fs::write(dir.join("x.txt"), "x\n").unwrap();
    g(&["add", "."]);
    g(&["commit", "-q", "-m", "anonymous work"]);
    let anon = g(&["rev-parse", "HEAD"]);
    g(&["checkout", "-q", "main"]);
    g(&["branch", "-q", "-D", "tmp"]);
    g(&["update-ref", &format!("refs/jj/keep/{anon}"), &anon]);

    // commedit reorders its own branch.
    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let from = commits.iter().position(|c| c.subject == "C").unwrap();
    let mv = repo.plan_reorder(&commits, from, commits.len()).expect("plan");
    repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder");

    // The anonymous head's keep-ref (and thus the commit) must still be there:
    // commedit only prunes its own history's keep-refs.
    assert_eq!(
        common::git(dir, &["rev-parse", "--verify", &format!("refs/jj/keep/{anon}")]),
        anon
    );
}

#[test]
fn reorder_works_on_a_linear_branch_with_a_divergent_side_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);

    // A linear main A <- B <- C, plus a divergent branch `side` (with commit X
    // off A) left as a ref. This is the davici shape: the edited branch is
    // linear, but the gitk-style view also shows the side branch.
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );
    g(&["checkout", "-q", "-b", "side", "main~2"]);
    std::fs::write(dir.join("x.txt"), "x\n").unwrap();
    g(&["add", "."]);
    g(&["commit", "-q", "-m", "X"]);
    g(&["checkout", "-q", "main"]);

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    // The view is a DAG (side diverges), yet reordering the linear main branch
    // must still work — this is what the over-strict whole-view gate broke.
    let third = commits.iter().find(|c| c.subject == "C").expect("C present");
    let from = commits.iter().position(|c| c.id == third.id).unwrap();
    let mv = repo
        .plan_reorder(&commits, from, commits.len())
        .expect("a reorder plan for the linear branch");
    repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder");

    // main is rearranged and stays linear (no spurious merge), the side branch
    // is untouched, and the repo is intact.
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A", "C"]);
    assert_eq!(common::git(dir, &["rev-list", "--merges", "--count", "main"]), "0");
    assert_eq!(common::git(dir, &["log", "--format=%s", "side"]).lines().next(), Some("X"));
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn drops_middle_commit_visible_to_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history"); // [third, second, first]
    let from = commits.iter().position(|c| c.subject == "second").unwrap();
    let target = repo.plan_drop(&commits, from).expect("droppable");
    repo.abandon_commit(&target).expect("drop");

    // "second" is gone; its descendant "third" rebased onto "first".
    assert_eq!(common::git_log_subjects(dir), vec!["third", "first"]);

    // Transparency invariants: HEAD attached, clean tree, intact repo.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn drops_then_restores_commit_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history"); // [third, second, first]
    let from = commits.iter().position(|c| c.subject == "second").unwrap();
    // Remember the pre-drop commit (its id stays resolvable) like the trash does.
    let second = commits[from].clone();
    let target = repo.plan_drop(&commits, from).expect("droppable");
    repo.abandon_commit(&target).expect("drop");
    assert_eq!(common::git_log_subjects(dir), vec!["third", "first"]);

    // Graft it back between "third" and "first" (gap 1), reproducing the original
    // order. This proves a dropped commit is still resolvable and re-graftable.
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history"); // [third, first]
    let mv = repo.plan_restore(&commits, &second, 1).expect("restore plan");
    repo.restore_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("restore");

    assert_eq!(common::git_log_subjects(dir), vec!["third", "second", "first"]);
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn drops_branch_tip_moving_the_branch_to_its_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history"); // [third, second, first]
    let target = repo.plan_drop(&commits, 0).expect("droppable"); // the tip "third"
    repo.abandon_commit(&target).expect("drop");

    // The branch bookmark followed to the parent "second"; the tree is clean.
    assert_eq!(common::git_log_subjects(dir), vec!["second", "first"]);
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}
