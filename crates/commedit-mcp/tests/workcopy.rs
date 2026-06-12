//! Working-copy preservation through the MCP surface: uncommitted changes
//! ride through rewrites, fold into commits, and are discarded only with an
//! explicit confirmation.

mod common;

use commedit_mcp::dto::{
    CommitWorkingCopyReq, DiscardWorkingCopyReq, EditMessageReq, HunkSelectionDto,
    IdentityFieldsDto, ListHistoryReq, PatchSelectionDto, SaveResultDto, ShowCommitReq,
    SquashWorkingCopyReq,
};
use common::{expect_err, git, git_log_subjects, init_repo, open_server};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

/// Build a `commit_working_copy` request with the default identity, spelling out
/// only the partial-selection tiers under test.
fn commit_req(
    message: &str,
    paths: Option<Vec<String>>,
    hunks: Option<Vec<HunkSelectionDto>>,
    patches: Option<Vec<PatchSelectionDto>>,
) -> CommitWorkingCopyReq {
    CommitWorkingCopyReq {
        message: message.into(),
        identity: IdentityFieldsDto::default(),
        paths,
        hunks,
        patches,
    }
}

#[tokio::test]
async fn uncommitted_changes_survive_a_rewrite() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")],
    );
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("a.txt"), "1\nlocal edit\n").unwrap();
    assert!(!server.working_copy_status().await.unwrap().0.clean);

    // Rewrite the bottom commit's message — the dirty file must ride along.
    let history = server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: None,
        }))
        .await
        .unwrap()
        .0;
    let result = server
        .edit_message(Parameters(EditMessageReq {
            commit: history.commits[1].sha.clone(),
            message: "first, edited".into(),
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    assert_eq!(git_log_subjects(dir.path()), ["second", "first, edited"]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "1\nlocal edit\n"
    );
    let status = server.working_copy_status().await.unwrap().0;
    assert!(!status.clean);
    assert_eq!(status.entries[0].files, vec!["a.txt".to_string()]);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "M a.txt");
}

#[tokio::test]
async fn squash_working_copy_folds_the_dirt_into_a_commit() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")],
    );
    let server = open_server(dir.path());

    // A clean working copy has nothing to fold.
    let history = server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: None,
        }))
        .await
        .unwrap()
        .0;
    let first = history.commits[1].clone();
    let err = expect_err(
        server
            .squash_working_copy(Parameters(SquashWorkingCopyReq {
                dest: first.sha.clone(),
                message: None,
                paths: None,
                hunks: None,
                patches: None,
            }))
            .await,
    );
    assert!(
        err.message.contains("clean"),
        "unexpected error: {}",
        err.message
    );

    // Fold a dirty a.txt into the bottom commit ("first" introduced a.txt).
    std::fs::write(dir.path().join("a.txt"), "1\nfolded\n").unwrap();
    let result = server
        .squash_working_copy(Parameters(SquashWorkingCopyReq {
            dest: first.sha,
            message: None,
            paths: None,
            hunks: None,
            patches: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    // The message is kept (fixup), the content landed, the tree is clean.
    assert_eq!(git_log_subjects(dir.path()), ["second", "first"]);
    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "1\nfolded");
    assert!(server.working_copy_status().await.unwrap().0.clean);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn discard_working_copy_requires_confirmation() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first")]);
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("a.txt"), "dirty\n").unwrap();

    let err = expect_err(
        server
            .discard_working_copy(Parameters(DiscardWorkingCopyReq { confirm: false }))
            .await,
    );
    assert!(
        err.message.contains("confirm"),
        "unexpected error: {}",
        err.message
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "dirty\n"
    );

    let resp = server
        .discard_working_copy(Parameters(DiscardWorkingCopyReq { confirm: true }))
        .await
        .unwrap()
        .0;
    assert!(resp.ok);

    // The tree is reset to the branch tip.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "1\n"
    );
    assert!(server.working_copy_status().await.unwrap().0.clean);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    // The discard is on the session op-log (undo can bring the changes back).
    let ops = server.list_operations().await.unwrap().0;
    assert_eq!(ops.ops.len(), 1);
    assert!(
        ops.ops[0].label.contains("Drop uncommitted"),
        "label: {}",
        ops.ops[0].label
    );
}

#[tokio::test]
async fn untracked_files_stay_out_of_the_working_copy_and_alive_on_disk() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")],
    );
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("untracked.txt"), "keep me\n").unwrap();
    let status = server.working_copy_status().await.unwrap().0;
    assert!(status.clean, "untracked files are not uncommitted changes");

    // A rewrite leaves the untracked file untouched on disk.
    let history = server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: None,
        }))
        .await
        .unwrap()
        .0;
    server
        .edit_message(Parameters(EditMessageReq {
            commit: history.commits[1].sha.clone(),
            message: "first, edited".into(),
        }))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("untracked.txt")).unwrap(),
        "keep me\n"
    );
}

#[tokio::test]
async fn squash_working_copy_accepts_a_change_id_prefix() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")],
    );
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("a.txt"), "1\nfolded\n").unwrap();
    let history = server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: None,
        }))
        .await
        .unwrap()
        .0;
    let result = server
        .squash_working_copy(Parameters(SquashWorkingCopyReq {
            dest: history.commits[1].change_id[..8].to_string(),
            message: None,
            paths: None,
            hunks: None,
            patches: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "1\nfolded");
    assert!(server.working_copy_status().await.unwrap().0.clean);
}

#[tokio::test]
async fn commit_working_copy_paths_tier_commits_only_listed_files() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("a.txt"), "a\nedit-a\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b\nedit-b\n").unwrap();

    let result = server
        .commit_working_copy(Parameters(commit_req(
            "commit a",
            Some(vec!["a.txt".into()]),
            None,
            None,
        )))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    assert_eq!(
        git_log_subjects(dir.path()),
        ["commit a", "second", "first"]
    );
    assert_eq!(git(dir.path(), &["show", "HEAD:a.txt"]), "a\nedit-a");

    // The remainder is exactly the b.txt edit, still uncommitted.
    let status = server.working_copy_status().await.unwrap().0;
    assert!(!status.clean);
    assert_eq!(status.entries[0].files, vec!["b.txt".to_string()]);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "M b.txt");
}

#[tokio::test]
async fn commit_working_copy_hunks_tier_uses_show_commit_numbering() {
    let dir = TempDir::new().unwrap();
    let base: String = (1..=20).map(|n| format!("l{n}\n")).collect();
    init_repo(dir.path(), &[("f.txt", &base, "first")]);
    let server = open_server(dir.path());

    // Two far-apart edits → two independent, numbered hunks.
    let edited: String = (1..=20)
        .map(|n| match n {
            3 => "L3\n".to_string(),
            17 => "L17\n".to_string(),
            _ => format!("l{n}\n"),
        })
        .collect();
    std::fs::write(dir.path().join("f.txt"), &edited).unwrap();

    // show_commit on the working-copy entry reports the numbering to select from.
    let status = server.working_copy_status().await.unwrap().0;
    let wc_sha = status.entries[0].sha.clone();
    let shown = server
        .show_commit(Parameters(ShowCommitReq {
            commit: wc_sha,
            include_contents: None,
        }))
        .await
        .unwrap()
        .0;
    let hunks = shown
        .files
        .iter()
        .find(|f| f.path == "f.txt")
        .unwrap()
        .hunks
        .as_ref()
        .expect("text file has numbered hunks");
    assert_eq!(hunks.len(), 2);
    assert_eq!(hunks[0].index, 0);
    assert_eq!(hunks[1].index, 1);
    assert!(hunks[0].header.starts_with("@@"));

    // Commit only hunk 0; hunk 1 stays uncommitted.
    let result = server
        .commit_working_copy(Parameters(commit_req(
            "first hunk",
            None,
            Some(vec![HunkSelectionDto {
                path: "f.txt".into(),
                hunks: vec![0],
            }]),
            None,
        )))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    let committed = git(dir.path(), &["show", "HEAD:f.txt"]);
    assert!(
        committed.contains("\nL3\n"),
        "hunk 0 committed: {committed}"
    );
    assert!(
        committed.contains("\nl17\n"),
        "hunk 1 not committed: {committed}"
    );
    let status = server.working_copy_status().await.unwrap().0;
    assert_eq!(status.entries[0].files, vec!["f.txt".to_string()]);
}

#[tokio::test]
async fn commit_working_copy_patches_tier_commits_a_sub_hunk() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("f.txt", "1\n2\n3\n", "first")]);
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("f.txt"), "1\n2\nA\nB\n3\n").unwrap();
    // An edited patch (à la `git add -p` → e) commits only the `+A` line.
    let patch = "@@ -2,2 +2,3 @@\n 2\n+A\n 3\n".to_string();
    let result = server
        .commit_working_copy(Parameters(commit_req(
            "add A",
            None,
            None,
            Some(vec![PatchSelectionDto {
                path: "f.txt".into(),
                patch,
            }]),
        )))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    assert_eq!(git(dir.path(), &["show", "HEAD:f.txt"]), "1\n2\nA\n3");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "1\n2\nA\nB\n3\n"
    );
    let status = server.working_copy_status().await.unwrap().0;
    assert_eq!(status.entries[0].files, vec!["f.txt".to_string()]);
}

#[tokio::test]
async fn partial_commit_validation_rejects_bad_selections() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "a\n", "first")]);
    let server = open_server(dir.path());
    std::fs::write(dir.path().join("a.txt"), "a\nedit\n").unwrap();

    // The same path in two tiers is rejected.
    let err = expect_err(
        server
            .commit_working_copy(Parameters(commit_req(
                "x",
                Some(vec!["a.txt".into()]),
                Some(vec![HunkSelectionDto {
                    path: "a.txt".into(),
                    hunks: vec![0],
                }]),
                None,
            )))
            .await,
    );
    assert!(
        err.message.contains("more than one tier"),
        "got: {}",
        err.message
    );

    // A hunk entry that selects nothing is rejected.
    let err = expect_err(
        server
            .commit_working_copy(Parameters(commit_req(
                "x",
                None,
                Some(vec![HunkSelectionDto {
                    path: "a.txt".into(),
                    hunks: vec![],
                }]),
                None,
            )))
            .await,
    );
    assert!(
        err.message.contains("no hunk indices"),
        "got: {}",
        err.message
    );

    // Neither failed attempt touched history.
    assert_eq!(git_log_subjects(dir.path()), ["first"]);
}

#[tokio::test]
async fn commit_working_copy_composes_all_three_tiers_in_one_call() {
    let dir = TempDir::new().unwrap();
    let base: String = (1..=20).map(|n| format!("l{n}\n")).collect();
    init_repo(
        dir.path(),
        &[
            ("f.txt", &base, "first"),
            ("g.txt", "g\n", "second"),
            ("h.txt", "1\n2\n3\n", "third"),
        ],
    );
    let server = open_server(dir.path());

    // Interleaved edits across three files: f.txt gets two hunks, g.txt a whole
    // edit, h.txt two added lines we'll split.
    let f_edited: String = (1..=20)
        .map(|n| match n {
            3 => "L3\n".to_string(),
            17 => "L17\n".to_string(),
            _ => format!("l{n}\n"),
        })
        .collect();
    std::fs::write(dir.path().join("f.txt"), &f_edited).unwrap();
    std::fs::write(dir.path().join("g.txt"), "g\nedited\n").unwrap();
    std::fs::write(dir.path().join("h.txt"), "1\n2\nA\nB\n3\n").unwrap();

    // One call composes all three tiers: f.txt hunk 0, g.txt whole, h.txt's `+A`.
    let result = server
        .commit_working_copy(Parameters(commit_req(
            "compose",
            Some(vec!["g.txt".into()]),
            Some(vec![HunkSelectionDto {
                path: "f.txt".into(),
                hunks: vec![0],
            }]),
            Some(vec![PatchSelectionDto {
                path: "h.txt".into(),
                patch: "@@ -2,2 +2,3 @@\n 2\n+A\n 3\n".into(),
            }]),
        )))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    // Each tier committed exactly its slice.
    let f = git(dir.path(), &["show", "HEAD:f.txt"]);
    assert!(
        f.contains("\nL3\n") && f.contains("\nl17\n"),
        "f.txt hunk 0 only: {f}"
    );
    assert_eq!(git(dir.path(), &["show", "HEAD:g.txt"]), "g\nedited");
    assert_eq!(git(dir.path(), &["show", "HEAD:h.txt"]), "1\n2\nA\n3");

    // The remainder is f.txt's hunk 1 and h.txt's `+B`; g.txt is fully committed.
    let status = git(dir.path(), &["status", "--porcelain"]);
    assert!(
        status.contains("M f.txt") && status.contains("M h.txt"),
        "remainder: {status}"
    );
    assert!(
        !status.contains("g.txt"),
        "g.txt was fully committed: {status}"
    );
    git(dir.path(), &["fsck", "--no-progress"]);
}

#[tokio::test]
async fn commit_working_copy_with_no_selection_commits_everything() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("a.txt"), "a\nedit-a\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b\nedit-b\n").unwrap();

    // Omitting all three tiers commits the whole working copy (regression for the
    // extended request DTO).
    let result = server
        .commit_working_copy(Parameters(commit_req("all", None, None, None)))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    assert_eq!(git(dir.path(), &["show", "HEAD:a.txt"]), "a\nedit-a");
    assert_eq!(git(dir.path(), &["show", "HEAD:b.txt"]), "b\nedit-b");
    assert!(server.working_copy_status().await.unwrap().0.clean);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn squash_working_copy_can_reword_and_fold_partially() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")],
    );
    let server = open_server(dir.path());

    // Two dirty files; fold only a.txt into "first" and reword it in one call,
    // leaving b.txt's edit uncommitted.
    std::fs::write(dir.path().join("a.txt"), "1\nA\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "2\nB\n").unwrap();
    let history = server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: Some(vec![]),
        }))
        .await
        .unwrap()
        .0;
    let first = history.commits[1].change_id.clone();

    let result = server
        .squash_working_copy(Parameters(SquashWorkingCopyReq {
            dest: first,
            message: Some("first (with a.txt)".into()),
            paths: Some(vec!["a.txt".into()]),
            hunks: None,
            patches: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    // "first" gained a.txt's change and the new message; b.txt stays uncommitted.
    assert_eq!(
        git_log_subjects(dir.path()),
        ["second", "first (with a.txt)"]
    );
    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "1\nA");
    let status = server.working_copy_status().await.unwrap().0;
    assert!(!status.clean);
    assert_eq!(status.entries[0].files, vec!["b.txt".to_string()]);
}
