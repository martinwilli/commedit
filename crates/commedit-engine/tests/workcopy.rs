//! Snapshot the on-disk working copy into jj's `@` commit and materialize it
//! back out — the round-trip the rewrite pipeline relies on to preserve
//! uncommitted changes.

mod common;

use std::path::Path;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::history;
use commedit_engine::repo::Repo;
use commedit_engine::workcopy::PartialSelection;

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
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );

    let mut repo = Repo::open(dir).expect("open");
    let head = repo.head_commit_id().expect("head");

    // Local uncommitted state: edit a tracked file and add an untracked one.
    std::fs::write(dir.join("a.txt"), "a\nlocal edit\n").unwrap();
    std::fs::write(dir.join("new.txt"), "brand new\n").unwrap();

    // Snapshotting records the tracked edit into @ but leaves the untracked
    // file out — it's not part of the uncommitted-changes set.
    repo.snapshot_working_copy().expect("snapshot");
    let wc = repo.working_copy_commit_id().expect("@ present");
    assert_ne!(wc, head, "@ should be a distinct commit on top of HEAD");

    // Checking out clean HEAD reverts the tracked edit, but the untracked file
    // is left alone (jj never tracked it) — it stays alive on disk.
    repo.materialize_working_copy(&head)
        .expect("materialize head");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
    assert_eq!(
        std::fs::read_to_string(dir.join("new.txt")).unwrap(),
        "brand new\n",
        "untracked file survives a checkout"
    );

    // Checking @ back out restores exactly what we snapshotted — the tracked
    // edit. The untracked file is still present, never having been removed.
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
        &[
            ("a.txt", "a\n", "A"),
            ("b.txt", "b\n", "B"),
            ("c.txt", "c\n", "C"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    // Local edit to a.txt, which the rewrite of B does not touch.
    std::fs::write(dir.join("a.txt"), "a\nlocal edit\n").unwrap();

    let target = subject_id(&repo, "B").id;
    repo.rewrite_message(&target, "B (edited)")
        .expect("rewrite");

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
fn untracked_file_is_excluded_from_at_but_survives_a_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");
    std::fs::write(dir.join("new.txt"), "brand new\n").unwrap();

    // Snapshotting must NOT capture the untracked file into @ (it's not part of
    // the uncommitted-changes set), and never jj's own .jj dir either.
    repo.snapshot_working_copy().expect("snapshot");
    let wc = repo.working_copy_commit_id().expect("@").to_string();
    let tracked = common::git(dir, &["ls-tree", "-r", "--name-only", &wc]);
    assert!(
        !tracked.lines().any(|l| l == "new.txt"),
        "untracked file must stay out of @, got: {tracked}"
    );
    assert!(
        !tracked.lines().any(|l| l.starts_with(".jj")),
        ".jj must never be snapshotted into @, got: {tracked}"
    );

    let target = subject_id(&repo, "A").id;
    repo.rewrite_message(&target, "A (edited)")
        .expect("rewrite");

    // The rewrite went through, and the untracked file is still on disk and
    // still untracked — it was never managed by jj, so it stays alive.
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A (edited)"]);
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
        &[
            ("f.txt", "1\n2\n3\n4\n5\n", "base"),
            ("g.txt", "g\n", "top"),
        ],
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
    assert_eq!(
        common::git(dir, &["show", "HEAD~1:f.txt"]),
        "1-rewritten\n2\n3\n4\n5"
    );
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
    repo.rewrite_message(&target, "B (edited)")
        .expect("rewrite");

    // The index-only content was pinned to a recoverable backup ref.
    let backups = common::git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/commedit/backup/",
        ],
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
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/commedit/backup/",
        ],
    );
    assert_eq!(
        backups.lines().count(),
        1,
        "expected a single deduped backup ref, got: {backups}"
    );
}

#[test]
fn stale_backup_refs_are_pruned_to_one_on_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    // Seed several backup refs, as if left behind by earlier sessions.
    let tree = common::git(dir, &["rev-parse", "HEAD^{tree}"]);
    for tag in ["aaa", "bbb", "ccc"] {
        let commit = common::git(
            dir,
            &["commit-tree", &tree, "-m", &format!("stale backup {tag}")],
        );
        common::git(
            dir,
            &[
                "update-ref",
                &format!("refs/commedit/backup/index-{tag}"),
                &commit,
            ],
        );
    }

    let mut repo = Repo::open(dir).expect("open");
    let target = subject_id(&repo, "B").id;
    repo.rewrite_message(&target, "B (edited)")
        .expect("rewrite");

    // The rewrite prunes the pile-up down to a single most-recent backup ref.
    let backups = common::git(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/commedit/backup/",
        ],
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
    repo.rewrite_message(&target, "B (edited)")
        .expect("rewrite");

    assert_eq!(
        common::git(
            dir,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/commedit/backup/"
            ]
        ),
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
        !std::fs::read_to_string(dir.join("f.txt"))
            .unwrap()
            .contains("<<<<<<<"),
        "git/worktree must be untouched while the conflict is pending"
    );

    // Resolve @ in the pane, exactly like a commit conflict: read the markers,
    // write back a resolution.
    let cf = repo
        .read_conflict(&wc.change_id_hex(), "f.txt")
        .expect("read conflict");
    let outcome = repo
        .resolve_conflict(
            &wc.change_id_hex(),
            "f.txt",
            "1\n2-resolved\n3\n",
            cf.marker_len,
        )
        .expect("resolve");

    // Now the rewrite applies to git, and the resolved working copy lands on disk.
    assert!(matches!(outcome, SaveOutcome::Clean));
    assert_eq!(
        common::git(dir, &["show", "HEAD~1:f.txt"]),
        "1\n2-rewritten\n3"
    );
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
    repo.edit_working_copy_file(None, "a.txt", Some("a\npane edit\n"))
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

#[test]
fn dropping_the_working_copy_discards_all_uncommitted_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");
    let head_before = common::git(dir, &["rev-parse", "HEAD"]);

    // Some uncommitted changes, then discard the lot by dropping the entry.
    std::fs::write(dir.join("a.txt"), "a\nlocal\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\nlocal\n").unwrap();
    repo.drop_working_copy(None).expect("drop working copy");

    // The tree is clean again: no uncommitted entry, disk reverted to HEAD.
    assert!(repo.working_copy_info().is_none(), "tree clean after drop");
    assert!(repo.working_copy_chain().is_empty());
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b\n");

    // git is untouched: same tip, same branch, clean status.
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A"]);
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn discarding_uncommitted_changes_keeps_an_untracked_file_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");

    // A tracked edit plus a brand-new untracked file.
    std::fs::write(dir.join("a.txt"), "a\nlocal\n").unwrap();
    std::fs::write(dir.join("new.txt"), "brand new\n").unwrap();

    // Dropping discards the tracked edit (a.txt reverts to HEAD) by checking out a
    // clean tree — the untracked file must NOT be swept up with it, since jj never
    // tracked it.
    repo.drop_working_copy(None).expect("drop working copy");

    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
    assert_eq!(
        std::fs::read_to_string(dir.join("new.txt")).unwrap(),
        "brand new\n",
        "untracked file survives discarding the uncommitted changes"
    );
    // The discard left only the untracked file behind, as git sees it.
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "?? new.txt");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn dropping_one_split_chain_entry_discards_only_its_slice() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");

    // Two uncommitted changes, peeled into two entries: the edited entry keeps
    // the a.txt change, the leaf carries the b.txt change.
    std::fs::write(dir.join("a.txt"), "a\nAA\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\nBB\n").unwrap();
    repo.split_working_copy(None, &[("b.txt".to_string(), "b\n".to_string())])
        .expect("split working copy");
    let chain = repo.working_copy_chain();
    assert_eq!(chain.len(), 2);

    // Drop the leaf (the b.txt slice); the a.txt slice must survive.
    let leaf = chain[0].info.change_id_hex();
    repo.drop_working_copy(Some(&leaf)).expect("drop leaf");

    // One entry left, holding only the a.txt change; b.txt is back to HEAD.
    let chain = repo.working_copy_chain();
    assert_eq!(chain.len(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\nAA\n"
    );
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b\n");

    // git is untouched throughout.
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A"]);
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M a.txt");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn tracked_file_in_ignored_directory_is_not_a_phantom_change() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A")]);

    // A common pattern: ignore a directory's generated contents but force-track a
    // `.keep` so the directory exists in history. git keeps tracking `m4/.keep`
    // even though `m4/` is ignored, because ignore rules never apply to files
    // already in the index.
    std::fs::write(dir.join(".gitignore"), "m4/\n").unwrap();
    common::git(dir, &["add", ".gitignore"]);
    std::fs::create_dir(dir.join("m4")).unwrap();
    std::fs::write(dir.join("m4/.keep"), "").unwrap();
    common::git(dir, &["add", "-f", "m4/.keep"]);
    common::git(dir, &["commit", "-q", "-m", "keep m4"]);

    // An actually-ignored, untracked file in the same directory: it must stay
    // excluded from @ (the untracked-files rule), so widening the snapshot to see
    // the tracked `.keep` doesn't drag the ignored siblings in.
    std::fs::write(dir.join("m4/generated.m4"), "noise\n").unwrap();

    let repo = Repo::open(dir).expect("open");

    // Nothing changed on disk relative to HEAD, so the working copy is clean —
    // `m4/.keep` must not surface as a (phantom, deleted) uncommitted change just
    // because it lives inside an ignored directory.
    assert!(
        repo.working_copy_info().is_none(),
        "tracked file in an ignored directory must not show as an uncommitted change"
    );
    assert!(repo.working_copy_chain().is_empty());

    // git agrees the tree is clean — the generated file is ignored (`m4/`), so
    // plain `git status` is empty; commedit must match that, not over-report.
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    assert_eq!(
        common::git(dir, &["status", "--porcelain", "--ignored"]),
        "!! m4/generated.m4",
        "the ignored sibling stays ignored — widening the snapshot didn't track it"
    );
}

// ---------------------------------------------------------------------------
// Partial working-copy commit (commit_working_copy_partial)
// ---------------------------------------------------------------------------

#[test]
fn partial_commit_paths_tier_commits_only_listed_files() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );
    let mut repo = Repo::open(dir).expect("open");

    // Edit two tracked files; commit only a.txt whole.
    std::fs::write(dir.join("a.txt"), "a\nedit-a\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\nedit-b\n").unwrap();

    let paths = vec!["a.txt".to_string()];
    let sel = PartialSelection {
        paths: &paths,
        hunks: &[],
        patches: &[],
    };
    let outcome = repo
        .commit_working_copy_partial(sel, "commit a only", None)
        .expect("partial commit");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // The new commit holds the edited a.txt and the *original* b.txt.
    assert_eq!(
        common::git_log_subjects(dir),
        ["commit a only", "second", "first"]
    );
    assert_eq!(common::git(dir, &["show", "HEAD:a.txt"]), "a\nedit-a");
    assert_eq!(common::git(dir, &["show", "HEAD:b.txt"]), "b");

    // Disk is byte-identical for both files — only b.txt's edit stays uncommitted.
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\nedit-a\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "b\nedit-b\n"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M b.txt");

    // One remaining chain entry (the remainder); transparency holds.
    assert_eq!(repo.working_copy_chain().len(), 1);
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn partial_commit_hunks_tier_commits_only_the_selected_hunk() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let base: String = (1..=20).map(|n| format!("l{n}\n")).collect();
    common::init_repo(dir, &[("f.txt", &base, "first")]);
    let mut repo = Repo::open(dir).expect("open");

    // Two far-apart edits → two independent hunks (3 lines of context can't bridge
    // a 13-line gap), so hunk 0 is the line-3 change and hunk 1 the line-17 change.
    let edited: String = (1..=20)
        .map(|n| match n {
            3 => "L3\n".to_string(),
            17 => "L17\n".to_string(),
            _ => format!("l{n}\n"),
        })
        .collect();
    std::fs::write(dir.join("f.txt"), &edited).unwrap();

    let hunks = vec![("f.txt".to_string(), vec![0usize])];
    let sel = PartialSelection {
        paths: &[],
        hunks: &hunks,
        patches: &[],
    };
    let outcome = repo
        .commit_working_copy_partial(sel, "first hunk", None)
        .expect("partial commit");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // Committed content keeps hunk 0 (L3) but reverts hunk 1 (l17 stays original).
    let committed = common::git(dir, &["show", "HEAD:f.txt"]);
    assert!(
        committed.contains("\nL3\n"),
        "hunk 0 committed, got: {committed}"
    );
    assert!(
        committed.contains("\nl17\n"),
        "hunk 1 not committed, got: {committed}"
    );

    // Disk is unchanged (both edits present); the remainder is hunk 1.
    assert_eq!(std::fs::read_to_string(dir.join("f.txt")).unwrap(), edited);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M f.txt");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn partial_commit_patches_tier_commits_a_sub_hunk_and_rejects_a_corrupt_patch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("f.txt", "1\n2\n3\n", "first")]);
    let mut repo = Repo::open(dir).expect("open");

    // Add two lines on disk; an edited patch (à la `git add -p` → e) commits only
    // the `+A` line, leaving `+B` uncommitted.
    std::fs::write(dir.join("f.txt"), "1\n2\nA\nB\n3\n").unwrap();
    let patch = "@@ -2,2 +2,3 @@\n 2\n+A\n 3\n";
    let patches = vec![("f.txt".to_string(), patch.to_string())];
    let sel = PartialSelection {
        paths: &[],
        hunks: &[],
        patches: &patches,
    };
    repo.commit_working_copy_partial(sel, "add A only", None)
        .expect("partial commit");
    assert_eq!(common::git(dir, &["show", "HEAD:f.txt"]), "1\n2\nA\n3");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "1\n2\nA\nB\n3\n"
    );

    // A patch whose context doesn't match the file is rejected, not mis-applied.
    let bad = vec![(
        "f.txt".to_string(),
        "@@ -1,1 +1,2 @@\n NOPE\n+X\n".to_string(),
    )];
    let sel = PartialSelection {
        paths: &[],
        hunks: &[],
        patches: &bad,
    };
    assert!(repo.commit_working_copy_partial(sel, "bad", None).is_err());
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn partial_commit_of_everything_matches_commit_working_copy() {
    // Two identical repos edited the same way: one whole-commits, the other
    // partial-commits every changed path. The resulting trees must be identical.
    let edit = |dir: &Path| {
        common::init_repo(
            dir,
            &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
        );
        std::fs::write(dir.join("a.txt"), "a\nx\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b\ny\n").unwrap();
    };

    let whole_tmp = tempfile::tempdir().unwrap();
    let whole = whole_tmp.path();
    edit(whole);
    Repo::open(whole)
        .expect("open")
        .commit_working_copy("all", None)
        .expect("commit wc");

    let part_tmp = tempfile::tempdir().unwrap();
    let part = part_tmp.path();
    edit(part);
    let mut prepo = Repo::open(part).expect("open");
    let paths = vec!["a.txt".to_string(), "b.txt".to_string()];
    let sel = PartialSelection {
        paths: &paths,
        hunks: &[],
        patches: &[],
    };
    prepo
        .commit_working_copy_partial(sel, "all", None)
        .expect("partial commit");

    assert_eq!(
        common::git(whole, &["rev-parse", "HEAD^{tree}"]),
        common::git(part, &["rev-parse", "HEAD^{tree}"]),
        "selecting every path equals committing the whole working copy"
    );
    // The remainder is empty, so the partial side's tree is clean again.
    assert!(prepo.working_copy_info().is_none());
    assert_eq!(common::git(part, &["status", "--porcelain"]), "");
}

#[test]
fn partial_commit_with_an_empty_selection_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );
    let mut repo = Repo::open(dir).expect("open");

    // Edit a.txt, but select the *unmodified* b.txt → the commit would be empty.
    std::fs::write(dir.join("a.txt"), "a\nedit\n").unwrap();
    let paths = vec!["b.txt".to_string()];
    let sel = PartialSelection {
        paths: &paths,
        hunks: &[],
        patches: &[],
    };
    let err = repo
        .commit_working_copy_partial(sel, "nope", None)
        .unwrap_err();
    assert!(err.to_string().contains("commits nothing"), "got: {err}");

    // History untouched and a.txt's edit is still uncommitted.
    assert_eq!(common::git_log_subjects(dir), ["second", "first"]);
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M a.txt");
}

#[test]
fn partial_commit_paths_tier_commits_a_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );
    let mut repo = Repo::open(dir).expect("open");

    // Delete a.txt on disk and edit b.txt; commit only the deletion.
    std::fs::remove_file(dir.join("a.txt")).unwrap();
    std::fs::write(dir.join("b.txt"), "b\nedit\n").unwrap();
    let paths = vec!["a.txt".to_string()];
    let sel = PartialSelection {
        paths: &paths,
        hunks: &[],
        patches: &[],
    };
    repo.commit_working_copy_partial(sel, "drop a", None)
        .expect("partial commit");

    // a.txt is gone from HEAD; b.txt is still original there.
    assert!(common::git(dir, &["ls-tree", "--name-only", "HEAD"])
        .lines()
        .all(|l| l != "a.txt"));
    assert_eq!(common::git(dir, &["show", "HEAD:b.txt"]), "b");
    // Disk: a.txt stays deleted, b.txt's edit remains uncommitted.
    assert!(!dir.join("a.txt").exists());
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M b.txt");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn partial_commit_value_splice_preserves_exec_bit_and_rejects_binary_text_tiers() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("keep.txt", "k\n", "first")]);
    // Commit an executable script and a binary file.
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("data.bin"), [0u8, 159, 146, 150]).unwrap();
    common::git(dir, &["add", "run.sh", "data.bin"]);
    common::git(dir, &["commit", "-q", "-m", "tools"]);

    let mut repo = Repo::open(dir).expect("open");
    // Edit the script's content (mode kept) and the binary's bytes.
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\necho bye\n").unwrap();
    std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("data.bin"), [1u8, 2, 159, 3]).unwrap();

    // The binary can't be addressed by hunk (or patch) — text tiers reject it.
    let hunks = vec![("data.bin".to_string(), vec![0usize])];
    let sel = PartialSelection {
        paths: &[],
        hunks: &hunks,
        patches: &[],
    };
    let err = repo
        .commit_working_copy_partial(sel, "x", None)
        .unwrap_err();
    assert!(err.to_string().contains("binary"), "got: {err}");

    // The executable commits whole via the paths tier, keeping its 100755 mode.
    let paths = vec!["run.sh".to_string()];
    let sel = PartialSelection {
        paths: &paths,
        hunks: &[],
        patches: &[],
    };
    repo.commit_working_copy_partial(sel, "update script", None)
        .expect("partial commit");
    assert_eq!(
        common::git(dir, &["show", "HEAD:run.sh"]),
        "#!/bin/sh\necho bye"
    );
    assert!(
        common::git(dir, &["ls-tree", "HEAD", "run.sh"]).starts_with("100755"),
        "the executable bit is preserved by the value-splice"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn partial_commit_collapses_a_split_working_copy_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );
    let mut repo = Repo::open(dir).expect("open");

    // Two uncommitted edits peeled into a two-entry chain.
    std::fs::write(dir.join("a.txt"), "a\nAA\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\nBB\n").unwrap();
    repo.split_working_copy(None, &[("b.txt".to_string(), "b\n".to_string())])
        .expect("split");
    assert_eq!(repo.working_copy_chain().len(), 2);

    // A partial commit reads the leaf's full tree, collapsing the chain; the
    // remainder is a single entry.
    let paths = vec!["a.txt".to_string()];
    let sel = PartialSelection {
        paths: &paths,
        hunks: &[],
        patches: &[],
    };
    let outcome = repo
        .commit_working_copy_partial(sel, "commit a", None)
        .expect("partial commit");
    assert!(matches!(outcome, SaveOutcome::Clean));

    assert_eq!(common::git(dir, &["show", "HEAD:a.txt"]), "a\nAA");
    assert_eq!(
        repo.working_copy_chain().len(),
        1,
        "chain collapsed to the remainder"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "b\nBB\n"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M b.txt");
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
fn commit_working_copy_entry_commits_only_the_selected_slice() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);

    let mut repo = Repo::open(dir).expect("open");

    // Two uncommitted changes peeled into two entries: the edited entry keeps the
    // a.txt change, the leaf carries the b.txt change.
    std::fs::write(dir.join("a.txt"), "a\nAA\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\nBB\n").unwrap();
    repo.split_working_copy(None, &[("b.txt".to_string(), "b\n".to_string())])
        .expect("split working copy");
    let chain = repo.working_copy_chain();
    assert_eq!(chain.len(), 2);
    assert_eq!(
        chain[0].file_names,
        ["b.txt"],
        "leaf carries the b.txt slice"
    );
    assert_eq!(
        chain[1].file_names,
        ["a.txt"],
        "edited entry the a.txt slice"
    );

    // Commit the edited entry (the a.txt slice) — what its diff would display.
    let a_slice = chain[1].info.change_id_hex();
    let outcome = repo
        .commit_working_copy_entry(Some(&a_slice), "commit a slice", None)
        .expect("commit entry");
    assert!(matches!(outcome, SaveOutcome::Clean));

    // The new commit holds only the a.txt change; b.txt stays at its committed value.
    assert_eq!(common::git_log_subjects(dir), ["commit a slice", "B", "A"]);
    assert_eq!(common::git(dir, &["show", "HEAD:a.txt"]), "a\nAA");
    assert_eq!(common::git(dir, &["show", "HEAD:b.txt"]), "b");

    // The b.txt slice is still uncommitted — one chain entry left, disk unchanged.
    let chain = repo.working_copy_chain();
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].file_names, ["b.txt"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\nAA\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "b\nBB\n"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M b.txt");
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn commit_working_copy_entry_lone_entry_commits_everything_and_cleans() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "A")]);

    let mut repo = Repo::open(dir).expect("open");

    // A single uncommitted entry: committing it (leaf fallback, no change id)
    // crystallizes the whole tree and leaves a clean working copy, exactly like
    // `commit_working_copy`.
    std::fs::write(dir.join("a.txt"), "a\nlocal\n").unwrap();
    std::fs::write(dir.join("b.txt"), "brand new\n").unwrap();
    let outcome = repo
        .commit_working_copy_entry(None, "commit it all", None)
        .expect("commit entry");
    assert!(matches!(outcome, SaveOutcome::Clean));

    assert_eq!(common::git_log_subjects(dir), ["commit it all", "A"]);
    assert_eq!(common::git(dir, &["show", "HEAD:a.txt"]), "a\nlocal");
    // The untracked b.txt is never auto-tracked, so it stays uncommitted on disk.
    assert!(repo.working_copy_chain().is_empty(), "working copy clean");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "?? b.txt");
    common::git(dir, &["fsck", "--no-progress"]);
}
