//! End-to-end: a rewrite that rebases a commit across a file rename follows the
//! rename the way `git rebase` does (re-merged through `git merge-tree`), rather
//! than leaving jj's path-keyed modify/delete conflict. A genuine conflict across
//! a rename is still held for manual resolution, and a commit that carries more
//! than a rebase is never re-merged.

mod common;

use std::path::Path;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use commedit_engine::squash::SquashMode;
use commedit_engine::tree::StrReplace;

/// The lines 1..=20, with line 10 reading `line10`.
fn numbers(line10: &str) -> String {
    (1..=20)
        .map(|i| match i {
            10 => format!("{line10}\n"),
            _ => format!("{i}\n"),
        })
        .collect()
}

fn init(dir: &Path) {
    common::git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
}

fn commit_files(dir: &Path, files: &[(&str, &str)], message: &str) {
    for (file, contents) in files {
        std::fs::write(dir.join(file), contents).unwrap();
        common::git(dir, &["add", file]);
    }
    common::git(dir, &["commit", "-q", "-m", message]);
}

fn commit_rename(dir: &Path, from: &str, to: &str, message: &str) {
    common::git(dir, &["mv", from, to]);
    common::git(dir, &["commit", "-q", "-m", message]);
}

/// Line 10 of `path` at `rev`.
fn line10(dir: &Path, rev: &str, path: &str) -> String {
    let text = common::git(dir, &["show", &format!("{rev}:{path}")]);
    text.lines().nth(9).expect("line 10").to_string()
}

/// The paths in `rev`'s tree, sorted and newline-joined.
fn files_at(dir: &Path, rev: &str) -> String {
    common::git(dir, &["ls-tree", "--name-only", rev])
}

fn row(repo: &Repo, subject: &str) -> usize {
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    commits
        .iter()
        .position(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("no commit {subject}"))
}

/// Plan and perform "drag display row `from` to the top gap".
fn reorder_row_to_top(repo: &mut Repo, from: usize) -> SaveOutcome {
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let mv = common::plan_reorder_single(repo, &commits, from, 0);
    repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder")
}

/// The subjects of the conflicted commits, without the working copy: an empty
/// `@` on a conflicted tip inherits the tip's conflict.
fn conflicted_subjects(outcome: &SaveOutcome) -> Vec<String> {
    match outcome {
        SaveOutcome::Conflicts { commits } => commits
            .iter()
            .map(|c| c.subject.clone())
            .filter(|s| s != "Uncommitted changes")
            .collect(),
        SaveOutcome::Clean => panic!("expected conflicts, got a clean save"),
    }
}

/// The plain-git view stays an ordinary, clean, attached checkout.
fn assert_transparent(dir: &Path) {
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn moving_a_rename_below_an_edit_follows_the_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // base <- X (edits a.txt) <- R (renames a.txt to b.txt). Moving R below X
    // rebases both across the rename: R' no longer sees X's edit, and X' edits a
    // file that is now called b.txt. jj conflicts both as modify/delete.
    init(dir);
    commit_files(dir, &[("a.txt", &numbers("10"))], "base");
    commit_files(dir, &[("a.txt", &numbers("ten"))], "X");
    commit_rename(dir, "a.txt", "b.txt", "R");
    let mut repo = Repo::open(dir).expect("open");

    let from = row(&repo, "X");
    let outcome = reorder_row_to_top(&mut repo, from);

    assert!(
        matches!(outcome, SaveOutcome::Clean),
        "expected the rename to be followed, got {outcome:?}"
    );
    assert!(!repo.is_pending());
    assert_eq!(common::git_log_subjects(dir), vec!["X", "R", "base"]);
    // R' renames the untouched file, X' carries its edit over to b.txt.
    assert_eq!(files_at(dir, "HEAD~1"), "b.txt");
    assert_eq!(line10(dir, "HEAD~1", "b.txt"), "10");
    assert_eq!(files_at(dir, "HEAD"), "b.txt");
    assert_eq!(line10(dir, "HEAD", "b.txt"), "ten");
    assert_transparent(dir);
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        numbers("ten")
    );
    assert!(!dir.join("a.txt").exists());
}

#[test]
fn moving_an_edit_below_a_rename_follows_the_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // base <- R (renames a.txt to b.txt) <- X (edits b.txt). Moving X below R
    // puts the edit on a.txt, and R' renames the edited file.
    init(dir);
    commit_files(dir, &[("a.txt", &numbers("10"))], "base");
    commit_rename(dir, "a.txt", "b.txt", "R");
    commit_files(dir, &[("b.txt", &numbers("ten"))], "X");
    let mut repo = Repo::open(dir).expect("open");

    let from = row(&repo, "R");
    let outcome = reorder_row_to_top(&mut repo, from);

    assert!(
        matches!(outcome, SaveOutcome::Clean),
        "expected the rename to be followed, got {outcome:?}"
    );
    assert_eq!(common::git_log_subjects(dir), vec!["R", "X", "base"]);
    assert_eq!(files_at(dir, "HEAD~1"), "a.txt");
    assert_eq!(line10(dir, "HEAD~1", "a.txt"), "ten");
    assert_eq!(files_at(dir, "HEAD"), "b.txt");
    assert_eq!(line10(dir, "HEAD", "b.txt"), "ten");
    assert_transparent(dir);
}

#[test]
fn editing_a_file_below_its_rename_follows_the_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Editing a.txt in base rebases R, which renamed the unedited a.txt, onto it.
    init(dir);
    commit_files(dir, &[("a.txt", &numbers("10"))], "base");
    commit_rename(dir, "a.txt", "b.txt", "R");
    let mut repo = Repo::open(dir).expect("open");

    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let base = commits[row(&repo, "base")].id.clone();
    let outcome = repo
        .replace_in_files(
            &base,
            &[StrReplace {
                path: "a.txt".to_string(),
                old: "10\n".to_string(),
                new: "ten\n".to_string(),
                all: false,
            }],
        )
        .expect("replace");

    assert!(
        matches!(outcome, SaveOutcome::Clean),
        "expected the rename to be followed, got {outcome:?}"
    );
    assert_eq!(common::git_log_subjects(dir), vec!["R", "base"]);
    assert_eq!(line10(dir, "HEAD~1", "a.txt"), "ten");
    assert_eq!(files_at(dir, "HEAD"), "b.txt");
    assert_eq!(line10(dir, "HEAD", "b.txt"), "ten");
    assert_transparent(dir);
}

#[test]
fn a_genuine_conflict_across_a_rename_is_still_held() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // R renames a.txt *and* rewrites the line X edited. Moving R below X makes
    // the two edits of line 10 collide, which git can't merge either.
    init(dir);
    commit_files(dir, &[("a.txt", &numbers("10"))], "base");
    commit_files(dir, &[("a.txt", &numbers("ten"))], "X");
    common::git(dir, &["mv", "a.txt", "b.txt"]);
    commit_files(dir, &[("b.txt", &numbers("TEN"))], "R");
    let head = common::git(dir, &["rev-parse", "HEAD"]);
    let mut repo = Repo::open(dir).expect("open");

    let from = row(&repo, "X");
    let outcome = reorder_row_to_top(&mut repo, from);

    assert!(conflicted_subjects(&outcome).contains(&"R".to_string()));
    assert!(repo.is_pending());
    // git never saw the conflicted rewrite.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head);
    assert_eq!(common::git_log_subjects(dir), vec!["R", "X", "base"]);
}

#[test]
fn only_the_genuinely_conflicting_commit_is_left_after_a_drop() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Dropping X rebases R across X's edits (healable, git follows the rename)
    // and Y onto a c.txt line it no longer agrees with (a real conflict).
    init(dir);
    commit_files(
        dir,
        &[("a.txt", &numbers("10")), ("c.txt", "c1\nc2\nc3\n")],
        "base",
    );
    commit_files(
        dir,
        &[("a.txt", &numbers("ten")), ("c.txt", "cX\nc2\nc3\n")],
        "X",
    );
    commit_rename(dir, "a.txt", "b.txt", "R");
    commit_files(dir, &[("c.txt", "cY\nc2\nc3\n")], "Y");
    let head = common::git(dir, &["rev-parse", "HEAD"]);
    let mut repo = Repo::open(dir).expect("open");

    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let target = repo
        .plan_drop(&commits, row(&repo, "X"))
        .expect("droppable");
    let outcome = repo.abandon_commit(&target).expect("drop");

    assert_eq!(conflicted_subjects(&outcome), vec!["Y"]);
    assert!(repo.is_pending());
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head);
}

#[test]
fn a_commit_carrying_more_than_a_rebase_is_not_re_merged() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Squashing A into the younger B both moves B (onto R') and folds A's edit of
    // a.txt into it, where the file is already called b.txt. Re-merging B as a
    // plain rebase would drop A's edit, so B stays conflicted for the user. R
    // itself is a pure rebase and is re-merged. B also adds a file, which keeps
    // the spurious resolver (it only peels modifications) out of the way.
    init(dir);
    commit_files(dir, &[("a.txt", &numbers("10"))], "base");
    commit_files(dir, &[("a.txt", &numbers("ten"))], "A");
    commit_rename(dir, "a.txt", "b.txt", "R");
    let mut b = numbers("ten");
    b.replace_range(0..1, "one");
    commit_files(dir, &[("b.txt", &b), ("d.txt", "d\n")], "B");
    let mut repo = Repo::open(dir).expect("open");

    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let (source, dest) = repo
        .plan_squash(&commits, row(&repo, "A"), row(&repo, "B"))
        .expect("plan");
    let outcome = repo
        .squash_into(&source, &dest, SquashMode::Fixup, None)
        .expect("squash");

    assert_eq!(conflicted_subjects(&outcome), vec!["B"]);
    assert!(repo.is_pending());
}
