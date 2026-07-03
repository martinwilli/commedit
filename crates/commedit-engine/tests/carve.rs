//! Carve a dirty working copy into several commits in one transaction, each
//! defined by a partial selection, remainder left uncommitted. Asserts against
//! plain `git`.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::repo::Repo;
use commedit_engine::workcopy::{CarveEntry, PartialSelection};

fn empty_sel(paths: &[String]) -> PartialSelection<'_> {
    PartialSelection {
        paths,
        hunks: &[],
        patches: &[],
    }
}

#[test]
fn carve_splits_a_dirty_tree_into_commits_by_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("base.txt", "base\n", "base")]);

    let mut repo = Repo::open(dir).expect("open");
    // Three unrelated new files on disk (untracked → must be added via the
    // selection's paths + add_paths), plus an edit to a tracked file.
    std::fs::write(dir.join("base.txt"), "base\nedit\n").unwrap();
    std::fs::write(dir.join("feat.txt"), "feature\n").unwrap();
    std::fs::write(dir.join("doc.txt"), "docs\n").unwrap();

    // Track the new files first (like the MCP layer's add_paths).
    repo.snapshot_working_copy_tracking(&["feat.txt".into(), "doc.txt".into()])
        .expect("snapshot");

    let feat = ["feat.txt".to_string()];
    let doc = ["doc.txt".to_string()];
    let base = ["base.txt".to_string()];
    let entries = vec![
        CarveEntry {
            message: "feat: add feature",
            identity: None,
            selection: empty_sel(&feat),
        },
        CarveEntry {
            message: "doc: add docs",
            identity: None,
            selection: empty_sel(&doc),
        },
        CarveEntry {
            message: "base: tweak base",
            identity: None,
            selection: empty_sel(&base),
        },
    ];

    let (outcome, change_ids) = repo.carve_working_copy(&entries).expect("carve");
    assert!(matches!(outcome, SaveOutcome::Clean));
    assert_eq!(change_ids.len(), 3);

    // Three commits, oldest-first as given, on top of base.
    assert_eq!(
        common::git_log_subjects(dir),
        vec![
            "base: tweak base",
            "doc: add docs",
            "feat: add feature",
            "base"
        ]
    );
    // Each commit introduces exactly its file / edit.
    assert_eq!(common::git(dir, &["show", "main~2:feat.txt"]), "feature");
    assert_eq!(common::git(dir, &["show", "main~1:doc.txt"]), "docs");
    assert_eq!(common::git(dir, &["show", "main:base.txt"]), "base\nedit");
    // Everything was carved — tree is clean.
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");

    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn carve_leaves_the_unselected_remainder_uncommitted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "init"), ("b.txt", "b\n", "init2")]);

    let mut repo = Repo::open(dir).expect("open");
    std::fs::write(dir.join("a.txt"), "a\nedit-a\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b\nedit-b\n").unwrap();

    // Carve only a.txt into a commit; b.txt's edit stays uncommitted.
    let a = ["a.txt".to_string()];
    let entries = vec![CarveEntry {
        message: "commit a only",
        identity: None,
        selection: empty_sel(&a),
    }];
    let (outcome, ids) = repo.carve_working_copy(&entries).expect("carve");
    assert!(matches!(outcome, SaveOutcome::Clean));
    assert_eq!(ids.len(), 1);

    assert_eq!(
        common::git_log_subjects(dir),
        vec!["commit a only", "init2", "init"]
    );
    assert_eq!(common::git(dir, &["show", "main:a.txt"]), "a\nedit-a");
    // b.txt's edit is still uncommitted on disk.
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "M b.txt");
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "b\nedit-b\n"
    );
    // a.txt's disk content still matches what we committed (byte-identical tree).
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a\nedit-a\n"
    );
}

#[test]
fn carve_rejects_a_path_claimed_by_two_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(dir, &[("a.txt", "a\n", "init")]);

    let mut repo = Repo::open(dir).expect("open");
    std::fs::write(dir.join("a.txt"), "a\nedit\n").unwrap();

    let a1 = ["a.txt".to_string()];
    let a2 = ["a.txt".to_string()];
    let entries = vec![
        CarveEntry {
            message: "one",
            identity: None,
            selection: empty_sel(&a1),
        },
        CarveEntry {
            message: "two",
            identity: None,
            selection: empty_sel(&a2),
        },
    ];
    let err = repo.carve_working_copy(&entries).unwrap_err();
    assert!(
        err.to_string().contains("more than one carve commit"),
        "unexpected error: {err}"
    );
}
