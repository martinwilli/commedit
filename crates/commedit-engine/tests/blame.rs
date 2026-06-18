//! End-to-end for the drag-to-squash blame hint: when every line a commit
//! removes traces back to one single commit, [`Repo::blame_single_source`]
//! returns that commit's display row; otherwise `None`. Built on real git repos
//! so the walk reads actual trees, like the GTK drag does.

mod common;

use commedit_engine::history::{history, CommitInfo};
use commedit_engine::repo::Repo;

/// The newest-first history of the current branch (the rows the UI lists).
fn commit_list(repo: &Repo) -> Vec<CommitInfo> {
    history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history")
}

fn index_of(commits: &[CommitInfo], subject: &str) -> usize {
    commits
        .iter()
        .position(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("commit {subject:?} present"))
}

#[test]
fn blames_modified_lines_to_their_single_introducing_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // C rewrites lines 2 and 3 of file.txt, both introduced by A. Subjects are
    // plain (no `fixup!`) — the hint works for *any* single drag.
    common::init_repo(
        dir,
        &[
            ("file.txt", "1\n2\n3\n4\n5\n", "A"),
            ("other.txt", "x\n", "B"),
            ("file.txt", "1\nTWO\nTHREE\n4\n5\n", "C"),
        ],
    );
    let repo = Repo::open(dir).expect("open");
    let commits = commit_list(&repo);

    let from = index_of(&commits, "C");
    let blamed = repo.blame_single_source(&commits, from);
    // Walks past B (which never touched file.txt) and lands on A.
    assert_eq!(blamed, Some(index_of(&commits, "A")));
}

#[test]
fn no_blame_when_removed_lines_span_two_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // A introduces "2"; B appends "4"; C rewrites both — two distinct sources.
    common::init_repo(
        dir,
        &[
            ("file.txt", "1\n2\n3\n", "A"),
            ("file.txt", "1\n2\n3\n4\n5\n", "B"),
            ("file.txt", "1\nTWO\n3\nFOUR\n5\n", "C"),
        ],
    );
    let repo = Repo::open(dir).expect("open");
    let commits = commit_list(&repo);

    assert_eq!(
        repo.blame_single_source(&commits, index_of(&commits, "C")),
        None
    );
}

#[test]
fn no_blame_for_a_pure_addition() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // B only appends a line — it removes nothing, so there is nothing to blame.
    common::init_repo(
        dir,
        &[("file.txt", "1\n", "A"), ("file.txt", "1\n2\n", "B")],
    );
    let repo = Repo::open(dir).expect("open");
    let commits = commit_list(&repo);

    assert_eq!(
        repo.blame_single_source(&commits, index_of(&commits, "B")),
        None
    );
}

#[test]
fn a_fixup_prefixed_commit_blames_just_like_a_plain_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // The blame is content-derived: the `fixup!` prefix is irrelevant to it.
    common::init_repo(
        dir,
        &[
            ("file.txt", "alpha\nbeta\ngamma\n", "feature"),
            ("file.txt", "alpha\nBETA\ngamma\n", "fixup! feature"),
        ],
    );
    let repo = Repo::open(dir).expect("open");
    let commits = commit_list(&repo);

    let from = index_of(&commits, "fixup! feature");
    assert_eq!(
        repo.blame_single_source(&commits, from),
        Some(index_of(&commits, "feature"))
    );
}

#[test]
fn a_merge_commit_has_no_single_blame() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);
    let repo = Repo::open(dir).expect("open");
    let commits = commit_list(&repo);

    // The merge has two parents — its removed lines are ambiguous by construction.
    let from = index_of(&commits, "merge");
    assert_eq!(repo.blame_single_source(&commits, from), None);
}
