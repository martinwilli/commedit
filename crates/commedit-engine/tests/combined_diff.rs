//! `diff::combined_changes` — the single minimal diff shown for a multi-commit
//! selection: each commit's change re-applied onto the oldest's parent tree, or
//! `None` when the selection's changes overlap and can't be combined.

mod common;

use commedit_engine::diff::{combined_changes, commit_changes, ChangeKind};
use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use jj_lib::backend::CommitId;

/// A linear repo whose single file `f.txt` is rewritten on every commit, so any
/// two non-adjacent commits touch the same line. Returns `(c1, c2, c3)` commit
/// ids (after "first" creates the file).
fn rewrite_chain(dir: &std::path::Path) -> (Repo, CommitId, CommitId, CommitId) {
    common::init_repo(
        dir,
        &[
            ("f.txt", "base\n", "first"),
            ("f.txt", "c1\n", "c1"),
            ("f.txt", "c2\n", "c2"),
            ("f.txt", "c3\n", "c3"),
        ],
    );
    let repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let by = |subject: &str| {
        commits
            .iter()
            .find(|c| c.subject == subject)
            .unwrap_or_else(|| panic!("commit {subject}"))
            .id
            .clone()
    };
    let (c1, c2, c3) = (by("c1"), by("c2"), by("c3"));
    (repo, c1, c2, c3)
}

#[test]
fn single_commit_matches_commit_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _c1, c2, _c3) = rewrite_chain(tmp.path());

    let combined = combined_changes(&repo.repo, std::slice::from_ref(&c2))
        .expect("combined")
        .expect("representable");
    let single = commit_changes(&repo.repo, &c2).expect("single");

    assert_eq!(combined.len(), 1);
    assert_eq!(single.len(), 1);
    assert_eq!(combined[0].path, single[0].path);
    assert_eq!(combined[0].kind, single[0].kind);
    assert_eq!(combined[0].old_text, single[0].old_text);
    assert_eq!(combined[0].new_text, single[0].new_text);
}

#[test]
fn contiguous_chain_is_cumulative() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _c1, c2, c3) = rewrite_chain(tmp.path());

    // Select c2 + c3 (oldest first). The minimal combined diff goes from c2's
    // parent (c1's tree, "c1\n") straight to c3's tree ("c3\n") — c2's
    // intermediate state does not appear.
    let combined = combined_changes(&repo.repo, &[c2, c3])
        .expect("combined")
        .expect("representable");

    assert_eq!(combined.len(), 1, "one file changed: {combined:?}");
    assert_eq!(combined[0].path, "f.txt");
    assert_eq!(combined[0].kind, ChangeKind::Modified);
    assert_eq!(combined[0].old_text.as_deref(), Some("c1\n"));
    assert_eq!(combined[0].new_text.as_deref(), Some("c3\n"));
}

#[test]
fn overlapping_non_contiguous_is_not_representable() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, c1, _c2, c3) = rewrite_chain(tmp.path());

    // Select c1 + c3, skipping c2. Re-applying c3's change (c2 → c3) onto c1's
    // tree is a 3-way merge whose base (c2) differs from both sides on the same
    // line, so it conflicts and the combination is not representable.
    let combined = combined_changes(&repo.repo, &[c1, c3]).expect("combined");
    assert!(
        combined.is_none(),
        "overlapping non-contiguous selection must not be representable: {combined:?}"
    );
}

#[test]
fn empty_selection_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _c1, _c2, _c3) = rewrite_chain(tmp.path());

    let combined = combined_changes(&repo.repo, &[])
        .expect("combined")
        .expect("representable");
    assert!(combined.is_empty());
}
