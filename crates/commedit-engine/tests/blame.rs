//! End-to-end for two blames. The drag-to-squash hint
//! ([`Repo::blame_single_source`]): when every line a commit removes traces back
//! to one single commit, it returns that commit's display row, else `None`. The
//! diff-viewer's old-side annotation ([`Repo::blame_old_side`]): each pre-image
//! line maps to the commit that last touched it. Both are built on real git
//! repos so the walks read actual trees, like the GTK drag / diff view do.

mod common;

use commedit_engine::blame::{BlameOrigins, FileBlame};
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

/// The subject of the commit blamed for old-file line `line`, or `None` at a
/// boundary.
fn origin_subject(fb: &FileBlame, line: usize) -> Option<&str> {
    fb.lines
        .get(line)
        .copied()
        .flatten()
        .map(|i| fb.origins[i].subject.as_str())
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

#[test]
fn old_side_blame_attributes_each_line_to_its_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // A introduces lines 1-3; B appends 4-5; C rewrites line 2. The diff shown
    // for C has B's tree as its old side ("1\n2\n3\n4\n5\n"), so blaming that
    // pre-image must attribute 1-3 to A and 4-5 to B (B never touched 1-3, A
    // never touched 4-5). `other.txt` proves the per-file path filter works.
    common::init_repo(
        dir,
        &[
            ("file.txt", "1\n2\n3\n", "A"),
            ("other.txt", "x\n", "A2"),
            ("file.txt", "1\n2\n3\n4\n5\n", "B"),
            ("file.txt", "1\nTWO\n3\n4\n5\n", "C"),
        ],
    );
    let repo = Repo::open(dir).expect("open");
    let commits = commit_list(&repo);
    let c = &commits[index_of(&commits, "C")];

    let blame = repo
        .blame_old_side(std::slice::from_ref(&c.id))
        .expect("blame");
    let fb = blame
        .iter()
        .find(|b| b.path == "file.txt")
        .expect("file.txt blamed");

    assert_eq!(fb.lines.len(), 5, "five old-side lines");
    assert_eq!(origin_subject(fb, 0), Some("A"));
    assert_eq!(origin_subject(fb, 1), Some("A"));
    assert_eq!(origin_subject(fb, 2), Some("A"));
    assert_eq!(origin_subject(fb, 3), Some("B"));
    assert_eq!(origin_subject(fb, 4), Some("B"));
}

#[test]
fn old_side_blame_path_scopes_to_one_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Same fixture as the whole-diff test: A intro 1-3, B appends 4-5, C rewrites
    // line 2. C's diff only touches file.txt, so other.txt is not in it.
    common::init_repo(
        dir,
        &[
            ("file.txt", "1\n2\n3\n", "A"),
            ("other.txt", "x\n", "A2"),
            ("file.txt", "1\n2\n3\n4\n5\n", "B"),
            ("file.txt", "1\nTWO\n3\n4\n5\n", "C"),
        ],
    );
    let repo = Repo::open(dir).expect("open");
    let commits = commit_list(&repo);
    let c = &commits[index_of(&commits, "C")];

    // The scoped blame of file.txt must match the file.txt entry of the full one.
    let full = repo
        .blame_old_side(std::slice::from_ref(&c.id))
        .expect("blame");
    let full_file = full
        .iter()
        .find(|b| b.path == "file.txt")
        .expect("file.txt blamed");
    let scoped = repo
        .blame_old_side_path(std::slice::from_ref(&c.id), "file.txt")
        .expect("scoped blame")
        .expect("file.txt has an old side");
    assert_eq!(scoped.lines, full_file.lines);

    // other.txt is not in C's diff, so there is nothing to blame there.
    assert!(
        repo.blame_old_side_path(std::slice::from_ref(&c.id), "other.txt")
            .expect("scoped blame")
            .is_none(),
        "other.txt is not in C's diff"
    );
}

#[test]
fn nothing_to_blame_on_a_root_commits_old_side() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // The first commit's old side is empty (its parent is the virtual root), so
    // every file is a pure addition with no pre-image to blame.
    common::init_repo(dir, &[("file.txt", "1\n2\n", "A")]);
    let repo = Repo::open(dir).expect("open");
    let commits = commit_list(&repo);
    let a = &commits[index_of(&commits, "A")];

    let blame = repo
        .blame_old_side(std::slice::from_ref(&a.id))
        .expect("blame");
    assert!(blame.is_empty(), "no old side on the root commit");
}

/// `(subject, removed-line count)` for each ranked candidate — assert on origins
/// by subject rather than churny row indices.
fn origin_counts<'a>(commits: &'a [CommitInfo], o: &BlameOrigins) -> Vec<(&'a str, usize)> {
    o.candidates
        .iter()
        .map(|&(row, n)| (commits[row].subject.as_str(), n))
        .collect()
}

#[test]
fn change_origins_attributes_a_single_source_like_the_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // C rewrites lines 2 and 3, both introduced by A (B never touched file.txt).
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
    let c = &commits[index_of(&commits, "C")];

    let o = repo.blame_change_origins(c, &commits);
    assert_eq!(origin_counts(&commits, &o), vec![("A", 2)]);
    assert_eq!(o.unattributed, 0);
    // A single clean origin agrees with the strict drag-to-squash hint.
    assert_eq!(
        Some(o.candidates[0].0),
        repo.blame_single_source(&commits, index_of(&commits, "C"))
    );
}

#[test]
fn change_origins_ranks_lines_spanning_two_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // A introduces "2"; B appends "4"/"5"; C rewrites line 2 (from A) and line 4
    // (from B) — one removed line each, from two distinct sources.
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
    let c = &commits[index_of(&commits, "C")];

    let o = repo.blame_change_origins(c, &commits);
    let mut counts = origin_counts(&commits, &o);
    counts.sort();
    // Both origins, one line each — exactly where the strict hint gives up.
    assert_eq!(counts, vec![("A", 1), ("B", 1)]);
    assert_eq!(o.unattributed, 0);
    assert_eq!(
        repo.blame_single_source(&commits, index_of(&commits, "C")),
        None
    );
}

#[test]
fn change_origins_is_empty_for_a_pure_addition() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // B only appends — it removes nothing, so there is nothing to attribute.
    common::init_repo(
        dir,
        &[("file.txt", "1\n", "A"), ("file.txt", "1\n2\n", "B")],
    );
    let repo = Repo::open(dir).expect("open");
    let commits = commit_list(&repo);
    let b = &commits[index_of(&commits, "B")];

    assert_eq!(
        repo.blame_change_origins(b, &commits),
        BlameOrigins::default()
    );
}

#[test]
fn change_origins_is_empty_for_a_merge_source() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);
    let repo = Repo::open(dir).expect("open");
    let commits = commit_list(&repo);
    let m = &commits[index_of(&commits, "merge")];

    // Two parents — ambiguous by construction, like blame_single_source.
    assert_eq!(
        repo.blame_change_origins(m, &commits),
        BlameOrigins::default()
    );
}
