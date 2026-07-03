//! One-shot absorb: route each uncommitted hunk into the commit that introduced
//! the lines it touches, in a single rewrite. Asserts against plain `git`.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::repo::Repo;

/// A three-commit history where each commit appends a block bracketed by stable
/// anchor lines (`a1`/`a2`, `b1`/`b2`, `c1`/`c2`). Editing the middle line of a
/// block blames unambiguously to its introducing commit, and the anchors give
/// jj's 3-way merge a common token on each side so the fold lands clean.
fn init_layered(dir: &std::path::Path) {
    common::init_repo(
        dir,
        &[
            ("f.txt", "a1\nAAA\na2\n", "A"),
            ("f.txt", "a1\nAAA\na2\nb1\nBBB\nb2\n", "B"),
            ("f.txt", "a1\nAAA\na2\nb1\nBBB\nb2\nc1\nCCC\nc2\n", "C"),
        ],
    );
}

#[test]
fn absorb_routes_each_hunk_to_its_origin() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_layered(dir);

    let mut repo = Repo::open(dir).expect("open");
    // Edit A's block (AAA) and C's block (CCC); leave B's alone.
    std::fs::write(dir.join("f.txt"), "a1\nXA\na2\nb1\nBBB\nb2\nc1\nXC\nc2\n").unwrap();

    let outcome = repo.absorb_working_copy(&[], false).expect("absorb");

    // Both hunks routed; nothing left uncommitted.
    assert!(
        matches!(outcome.applied, Some(SaveOutcome::Clean)),
        "absorb should land clean, got {:?}",
        outcome.applied
    );
    assert!(!outcome.remaining, "everything should be absorbed");
    assert!(outcome.skipped.is_empty());

    // Plan is ancestors-first: A then C, each a single one-line-modify hunk.
    let subjects: Vec<&str> = outcome
        .plan
        .iter()
        .map(|e| e.target.subject.as_str())
        .collect();
    assert_eq!(subjects, vec!["A", "C"]);
    let a = &outcome.plan[0].files;
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].path, "f.txt");
    assert_eq!((a[0].added, a[0].removed, a[0].hunks), (1, 1, 1));

    // The subjects are untouched and descendants rebased in place.
    assert_eq!(common::git_log_subjects(dir), vec!["C", "B", "A"]);
    // Each commit now owns its edited line.
    assert_eq!(common::git(dir, &["show", "main~2:f.txt"]), "a1\nXA\na2");
    assert_eq!(
        common::git(dir, &["show", "main~1:f.txt"]),
        "a1\nXA\na2\nb1\nBBB\nb2"
    );
    assert_eq!(
        common::git(dir, &["show", "main:f.txt"]),
        "a1\nXA\na2\nb1\nBBB\nb2\nc1\nXC\nc2"
    );
    // Working tree is clean — the changes moved into history.
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "a1\nXA\na2\nb1\nBBB\nb2\nc1\nXC\nc2\n"
    );

    // Transparency: HEAD attached, no jj keep-ref clutter, repo intact.
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(
        common::git(
            dir,
            &["for-each-ref", "--format=%(refname)", "refs/jj/keep/"]
        ),
        ""
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn dry_run_previews_without_touching_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_layered(dir);

    let mut repo = Repo::open(dir).expect("open");
    std::fs::write(dir.join("f.txt"), "a1\nXA\na2\nb1\nBBB\nb2\nc1\nXC\nc2\n").unwrap();

    let head_before = common::git(dir, &["rev-parse", "HEAD"]);
    let outcome = repo.absorb_working_copy(&[], true).expect("dry run");

    // The plan is computed, but nothing is applied.
    assert!(outcome.applied.is_none(), "dry run applies nothing");
    assert_eq!(outcome.plan.len(), 2);
    assert!(!outcome.remaining);

    // git history and the working tree are exactly as they were.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git_log_subjects(dir), vec!["C", "B", "A"]);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M f.txt");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "a1\nXA\na2\nb1\nBBB\nb2\nc1\nXC\nc2\n"
    );
}

#[test]
fn ambiguous_hunk_stays_uncommitted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_layered(dir);

    let mut repo = Repo::open(dir).expect("open");
    // A pure insertion on the boundary between B's block and C's block is
    // ambiguous (it could belong to either), so absorb must leave it alone.
    std::fs::write(
        dir.join("f.txt"),
        "a1\nAAA\na2\nb1\nBBB\nb2\nMID\nc1\nCCC\nc2\n",
    )
    .unwrap();

    let outcome = repo.absorb_working_copy(&[], false).expect("absorb");

    assert!(outcome.plan.is_empty(), "nothing attributable");
    assert!(
        outcome.applied.is_none(),
        "no transaction when nothing routes"
    );
    assert!(outcome.remaining, "the insertion stays uncommitted");

    // History untouched; the change is still in the working tree.
    assert_eq!(common::git_log_subjects(dir), vec!["C", "B", "A"]);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M f.txt");
}

#[test]
fn paths_filter_restricts_the_absorb() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("x.txt", "x1\nXXX\nx2\n", "A"),
            ("y.txt", "y1\nYYY\ny2\n", "B"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    std::fs::write(dir.join("x.txt"), "x1\nEDITED\nx2\n").unwrap();
    std::fs::write(dir.join("y.txt"), "y1\nEDITED\ny2\n").unwrap();

    // Restrict to x.txt: only the A-owned edit folds; y.txt stays uncommitted.
    let outcome = repo
        .absorb_working_copy(&["x.txt".to_string()], false)
        .expect("absorb");
    assert!(matches!(outcome.applied, Some(SaveOutcome::Clean)));
    assert_eq!(outcome.plan.len(), 1);
    assert_eq!(outcome.plan[0].target.subject, "A");
    assert!(outcome.remaining, "y.txt is still uncommitted");

    assert_eq!(
        common::git(dir, &["show", "main~1:x.txt"]),
        "x1\nEDITED\nx2"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M y.txt");
}

#[test]
fn refuses_a_clean_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    init_layered(dir);

    let mut repo = Repo::open(dir).expect("open");
    let err = repo.absorb_working_copy(&[], false).unwrap_err();
    assert!(
        err.to_string().contains("no uncommitted changes"),
        "unexpected error: {err}"
    );
}
