//! Snapshot the on-disk working copy into jj's `@` commit and materialize it
//! back out — the round-trip the rewrite pipeline relies on to preserve
//! uncommitted changes.

mod common;

use commedit_engine::history::history;
use commedit_engine::repo::Repo;

fn subject_id(repo: &Repo, subject: &str) -> commedit_engine::history::CommitInfo {
    history(&repo.repo, &repo.head_commit_id().expect("head"))
        .expect("history")
        .into_iter()
        .find(|c| c.subject == subject)
        .expect("commit present")
}

#[test]
fn snapshots_disk_into_working_copy_and_materializes_it_back() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")]);

    let mut repo = Repo::open(dir).expect("open");
    let head = repo.head_commit_id().expect("head");

    // Local uncommitted state: edit a tracked file and add an untracked one.
    std::fs::write(dir.join("a.txt"), "a\nlocal edit\n").unwrap();
    std::fs::write(dir.join("new.txt"), "brand new\n").unwrap();

    // Snapshotting records that state into the working-copy commit @.
    repo.snapshot_working_copy().expect("snapshot");
    let wc = repo.working_copy_commit_id().expect("@ present");
    assert_ne!(wc, head, "@ should be a distinct commit on top of HEAD");

    // Checking out clean HEAD reverts the working tree: the tracked edit is
    // undone and the (now-tracked) new file is removed.
    repo.materialize_working_copy(&head).expect("materialize head");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
    assert!(!dir.join("new.txt").exists(), "untracked file cleared by checkout");

    // Checking @ back out restores exactly what we snapshotted, proving the
    // snapshot captured both the edit and the untracked file.
    repo.materialize_working_copy(&wc).expect("materialize @");
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\nlocal edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("new.txt")).unwrap(),
        "brand new\n"
    );
}

#[test]
fn unstaged_edit_to_an_untouched_file_survives_a_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );

    let mut repo = Repo::open(dir).expect("open");
    // Local edit to a.txt, which the rewrite of B does not touch.
    std::fs::write(dir.join("a.txt"), "a\nlocal edit\n").unwrap();

    let target = subject_id(&repo, "B").id;
    repo.rewrite_message(&target, "B (edited)").expect("rewrite");

    // History rewritten, descendants preserved.
    assert_eq!(common::git_log_subjects(dir), vec!["C", "B (edited)", "A"]);
    // The local edit is still on disk, shown by git as an unstaged modification.
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\nlocal edit\n"
    );
    // (the common::git helper trims, so the porcelain " M a.txt" loses its lead)
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M a.txt");
    // Transparency holds: HEAD attached, no jj keep-ref clutter, repo intact.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(
        common::git(dir, &["for-each-ref", "--format=%(refname)", "refs/jj/keep/"]),
        ""
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn untracked_file_survives_a_rewrite_and_jj_dir_never_leaks() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");
    std::fs::write(dir.join("new.txt"), "brand new\n").unwrap();

    // Snapshotting must capture the untracked file but never jj's own .jj dir.
    repo.snapshot_working_copy().expect("snapshot");
    let wc = repo.working_copy_commit_id().expect("@").to_string();
    let tracked = common::git(dir, &["ls-tree", "-r", "--name-only", &wc]);
    assert!(tracked.lines().any(|l| l == "new.txt"), "untracked file captured");
    assert!(
        !tracked.lines().any(|l| l.starts_with(".jj")),
        ".jj must never be snapshotted into @, got: {tracked}"
    );

    let target = subject_id(&repo, "A").id;
    repo.rewrite_message(&target, "A (edited)").expect("rewrite");

    // The untracked file is still on disk and still untracked.
    assert_eq!(
        std::fs::read_to_string(dir.join("new.txt")).unwrap(),
        "brand new\n"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "?? new.txt");
}

#[test]
fn non_overlapping_edit_to_a_rewritten_file_is_merged_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("f.txt", "1\n2\n3\n4\n5\n", "base"), ("g.txt", "g\n", "top")],
    );

    let mut repo = Repo::open(dir).expect("open");
    // Local edit to the last line of f.txt...
    std::fs::write(dir.join("f.txt"), "1\n2\n3\n4\n5-local\n").unwrap();

    // ...while the rewrite changes the first line of f.txt in the base commit.
    let base = subject_id(&repo, "base").id;
    repo.rewrite_file(&base, "f.txt", "1-rewritten\n2\n3\n4\n5\n")
        .expect("rewrite file");

    // jj's 3-way merge carries the local edit onto the rewritten content: the
    // working tree ends up with both changes.
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "1-rewritten\n2\n3\n4\n5-local\n"
    );
    // The committed history has the rewrite but not the uncommitted edit.
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "1-rewritten\n2\n3\n4\n5");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn index_only_staged_content_is_backed_up_across_a_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");

    // Stage content into a.txt, then revert the working tree to HEAD: the staged
    // version now lives ONLY in the git index, invisible to jj's disk snapshot.
    std::fs::write(dir.join("a.txt"), "staged-only\n").unwrap();
    common::git(dir, &["add", "a.txt"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();

    let target = subject_id(&repo, "B").id;
    repo.rewrite_message(&target, "B (edited)").expect("rewrite");

    // The index-only content was pinned to a recoverable backup ref.
    let backups = common::git(
        dir,
        &["for-each-ref", "--format=%(refname)", "refs/commedit/backup/"],
    );
    let backup = backups.lines().next().expect("an index backup ref exists");
    assert!(backup.starts_with("refs/commedit/backup/index-"));
    assert_eq!(
        common::git(dir, &["show", &format!("{backup}:a.txt")]),
        "staged-only"
    );
}

#[test]
fn identical_index_only_content_dedups_to_one_backup_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");
    let stage_only = |dir: &std::path::Path| {
        std::fs::write(dir.join("a.txt"), "staged-only\n").unwrap();
        common::git(dir, &["add", "a.txt"]);
        std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    };

    // Two rewrites, each preceded by the *same* index-only staged content.
    stage_only(dir);
    let b = subject_id(&repo, "B").id;
    repo.rewrite_message(&b, "B v2").expect("rewrite 1");
    stage_only(dir);
    let b = subject_id(&repo, "B v2").id;
    repo.rewrite_message(&b, "B v3").expect("rewrite 2");

    // The backup ref is named after the index tree, so identical content reuses
    // one ref rather than piling up.
    let backups = common::git(
        dir,
        &["for-each-ref", "--format=%(refname)", "refs/commedit/backup/"],
    );
    assert_eq!(backups.lines().count(), 1, "expected a single deduped backup ref, got: {backups}");
}

#[test]
fn stale_backup_refs_are_pruned_to_one_on_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    // Seed several backup refs, as if left behind by earlier sessions.
    let tree = common::git(dir, &["rev-parse", "HEAD^{tree}"]);
    for tag in ["aaa", "bbb", "ccc"] {
        let commit = common::git(dir, &["commit-tree", &tree, "-m", &format!("stale backup {tag}")]);
        common::git(dir, &["update-ref", &format!("refs/commedit/backup/index-{tag}"), &commit]);
    }

    let mut repo = Repo::open(dir).expect("open");
    let target = subject_id(&repo, "B").id;
    repo.rewrite_message(&target, "B (edited)").expect("rewrite");

    // The rewrite prunes the pile-up down to a single most-recent backup ref.
    let backups = common::git(
        dir,
        &["for-each-ref", "--format=%(refname)", "refs/commedit/backup/"],
    );
    assert_eq!(
        backups.lines().count(),
        1,
        "stale backups should prune to one, got: {backups}"
    );
}

#[test]
fn a_plain_unstaged_edit_creates_no_backup_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");
    // Unstaged edit only: it lives on disk, so it needs no index backup.
    std::fs::write(dir.join("a.txt"), "a\nlocal\n").unwrap();

    let target = subject_id(&repo, "B").id;
    repo.rewrite_message(&target, "B (edited)").expect("rewrite");

    assert_eq!(
        common::git(dir, &["for-each-ref", "--format=%(refname)", "refs/commedit/backup/"]),
        ""
    );
}

#[test]
fn working_copy_info_is_some_only_when_dirty() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");
    // Clean tree right after open: no working-copy row.
    assert!(repo.working_copy_info().is_none());

    std::fs::write(dir.join("a.txt"), "a\nlocal\n").unwrap();
    repo.snapshot_working_copy().expect("snapshot");
    let info = repo.working_copy_info().expect("dirty");
    assert_eq!(info.changed_files, 1);
    assert!(!info.has_conflict);
}

#[test]
fn overlapping_edit_defers_as_a_conflict_then_resolves() {
    use commedit_engine::conflict::SaveOutcome;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("f.txt", "1\n2\n3\n", "base"), ("g.txt", "g\n", "top")],
    );

    let mut repo = Repo::open(dir).expect("open");
    // Local edit to line 2...
    std::fs::write(dir.join("f.txt"), "1\n2-local\n3\n").unwrap();

    // ...and the rewrite changes the very same line 2 of the base commit.
    let base = subject_id(&repo, "base").id;
    let outcome = repo
        .rewrite_file(&base, "f.txt", "1\n2-rewritten\n3\n")
        .expect("rewrite");

    // The overlap surfaces @ ("Uncommitted changes") as a conflicted commit and
    // the whole rewrite defers — git is left completely untouched.
    let SaveOutcome::Conflicts { commits } = outcome else {
        panic!("expected the overlap to defer as a conflict");
    };
    let wc = commits
        .iter()
        .find(|c| c.subject == "Uncommitted changes")
        .expect("@ is among the conflicts");
    assert_eq!(common::git_log_subjects(dir), vec!["top", "base"]);
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "1\n2\n3");
    assert!(
        !std::fs::read_to_string(dir.join("f.txt")).unwrap().contains("<<<<<<<"),
        "git/worktree must be untouched while the conflict is pending"
    );

    // Resolve @ in the pane, exactly like a commit conflict: read the markers,
    // write back a resolution.
    let cf = repo.read_conflict(&wc.change_id_hex(), "f.txt").expect("read conflict");
    let outcome = repo
        .resolve_conflict(&wc.change_id_hex(), "f.txt", "1\n2-resolved\n3\n", cf.marker_len)
        .expect("resolve");

    // Now the rewrite applies to git, and the resolved working copy lands on disk.
    assert!(matches!(outcome, SaveOutcome::Clean));
    assert_eq!(common::git(dir, &["show", "HEAD~1:f.txt"]), "1\n2-rewritten\n3");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "1\n2-resolved\n3\n"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn editing_the_working_copy_file_updates_the_worktree_not_history() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");
    // An uncommitted edit, then refine it through the diff pane.
    std::fs::write(dir.join("a.txt"), "a\nlocal\n").unwrap();
    repo.edit_working_copy_file(None, "a.txt", "a\npane edit\n")
        .expect("edit working copy");

    // The working tree reflects the pane edit...
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\npane edit\n"
    );
    // ...while committed history is untouched, and @ is still dirty.
    assert_eq!(common::git(dir, &["show", "HEAD:a.txt"]), "a");
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A"]);
    assert!(repo.working_copy_info().is_some());
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M a.txt");
    common::git(dir, &["fsck", "--no-progress"]);
}
