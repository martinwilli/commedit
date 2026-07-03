//! Working-copy preservation through the MCP surface: uncommitted changes
//! ride through rewrites, fold into commits, and are discarded only with an
//! explicit confirmation.

mod common;

use commedit_mcp::dto::{
    CommitWorkingCopyReq, DiscardWorkingCopyReq, EditMessageReq, HunkSelectionDto,
    IdentityFieldsDto, ListHistoryReq, PatchSelectionDto, SaveResultDto, ShowCommitReq,
    SquashWorkingCopyReq,
};
use common::{expect_err, git, git_log_subjects, init_repo, open_server, sel};
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
        session: sel("main"),
        message: message.into(),
        identity: IdentityFieldsDto::default(),
        paths,
        hunks,
        patches,
        add_paths: None,
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
    assert!(
        !server
            .working_copy_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .clean
    );

    // Rewrite the bottom commit's message — the dirty file must ride along.
    let history = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    let result = server
        .edit_message(Parameters(EditMessageReq {
            session: sel("main"),
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
    let status = server
        .working_copy_status(Parameters(sel("main")))
        .await
        .unwrap()
        .0;
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
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    let first = history.commits[1].clone();
    let err = expect_err(
        server
            .squash_working_copy(Parameters(SquashWorkingCopyReq {
                session: sel("main"),
                dest: first.sha.clone(),
                message: None,
                paths: None,
                hunks: None,
                patches: None,
                add_paths: None,
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
            session: sel("main"),
            dest: first.sha,
            message: None,
            paths: None,
            hunks: None,
            patches: None,
            add_paths: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result.result, SaveResultDto::Clean { .. }));

    // The message is kept (fixup), the content landed, the tree is clean.
    assert_eq!(git_log_subjects(dir.path()), ["second", "first"]);
    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "1\nfolded");
    assert!(
        server
            .working_copy_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .clean
    );
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
            .discard_working_copy(Parameters(DiscardWorkingCopyReq {
                session: sel("main"),
                confirm: false,
            }))
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
        .discard_working_copy(Parameters(DiscardWorkingCopyReq {
            session: sel("main"),
            confirm: true,
        }))
        .await
        .unwrap()
        .0;
    assert!(resp.ok);

    // The tree is reset to the branch tip.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "1\n"
    );
    assert!(
        server
            .working_copy_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .clean
    );
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    // The discard is on the session op-log (undo can bring the changes back).
    let ops = server
        .list_operations(Parameters(sel("main")))
        .await
        .unwrap()
        .0;
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
    let status = server
        .working_copy_status(Parameters(sel("main")))
        .await
        .unwrap()
        .0;
    assert!(status.clean, "untracked files are not uncommitted changes");

    // A rewrite leaves the untracked file untouched on disk.
    let history = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    server
        .edit_message(Parameters(EditMessageReq {
            session: sel("main"),
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
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    let result = server
        .squash_working_copy(Parameters(SquashWorkingCopyReq {
            session: sel("main"),
            dest: history.commits[1].change_id[..8].to_string(),
            message: None,
            paths: None,
            hunks: None,
            patches: None,
            add_paths: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result.result, SaveResultDto::Clean { .. }));

    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "1\nfolded");
    assert!(
        server
            .working_copy_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .clean
    );
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
    assert!(matches!(result.result, SaveResultDto::Clean { .. }));

    assert_eq!(
        git_log_subjects(dir.path()),
        ["commit a", "second", "first"]
    );
    assert_eq!(git(dir.path(), &["show", "HEAD:a.txt"]), "a\nedit-a");

    // The remainder is exactly the b.txt edit, still uncommitted.
    let status = server
        .working_copy_status(Parameters(sel("main")))
        .await
        .unwrap()
        .0;
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
    let status = server
        .working_copy_status(Parameters(sel("main")))
        .await
        .unwrap()
        .0;
    let wc_sha = status.entries[0].sha.clone();
    let shown = server
        .show_commit(Parameters(ShowCommitReq {
            session: sel("main"),
            commit: wc_sha,
            paths: None,
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
    assert!(matches!(result.result, SaveResultDto::Clean { .. }));

    let committed = git(dir.path(), &["show", "HEAD:f.txt"]);
    assert!(
        committed.contains("\nL3\n"),
        "hunk 0 committed: {committed}"
    );
    assert!(
        committed.contains("\nl17\n"),
        "hunk 1 not committed: {committed}"
    );
    let status = server
        .working_copy_status(Parameters(sel("main")))
        .await
        .unwrap()
        .0;
    assert_eq!(status.entries[0].files, vec!["f.txt".to_string()]);
}

#[tokio::test]
async fn commit_working_copy_reports_the_new_commit_and_remainder() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );
    let server = open_server(dir.path());

    // Two dirty files; commit only a.txt, leaving b.txt uncommitted.
    std::fs::write(dir.path().join("a.txt"), "a\nedit-a\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b\nedit-b\n").unwrap();

    let resp = server
        .commit_working_copy(Parameters(commit_req(
            "commit a",
            Some(vec!["a.txt".into()]),
            None,
            None,
        )))
        .await
        .unwrap()
        .0;
    assert!(matches!(resp.result, SaveResultDto::Clean { .. }));

    // The new commit is reported inline — its sha and stable change_id, ready to
    // chain a follow-up edit without a list_history.
    let committed = resp.committed.expect("the new commit is returned");
    assert_eq!(committed.subject, "commit a");
    assert!(!committed.change_id.is_empty() && !committed.sha.is_empty());

    // The remainder (b.txt) is reported inline too, so a partial commit is
    // verifiable without a follow-up working_copy_status.
    let wc = resp.working_copy.expect("working copy reported");
    assert!(!wc.clean, "b.txt is still uncommitted");
    assert_eq!(wc.entries[0].files, vec!["b.txt".to_string()]);
}

#[tokio::test]
async fn squash_working_copy_partial_reports_the_remainder() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("a.txt"), "a\nedit-a\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b\nedit-b\n").unwrap();

    // "first" introduced a.txt — fold only a.txt into it; b.txt stays uncommitted.
    let first = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0
        .commits[1]
        .change_id
        .clone();
    let resp = server
        .squash_working_copy(Parameters(SquashWorkingCopyReq {
            session: sel("main"),
            dest: first,
            message: None,
            paths: Some(vec!["a.txt".into()]),
            hunks: None,
            patches: None,
            add_paths: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(resp.result, SaveResultDto::Clean { .. }));

    let wc = resp.working_copy.expect("remainder reported");
    assert!(
        !wc.clean,
        "b.txt remains uncommitted after the partial fold"
    );
    assert_eq!(wc.entries[0].files, vec!["b.txt".to_string()]);
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
    assert!(matches!(result.result, SaveResultDto::Clean { .. }));

    assert_eq!(git(dir.path(), &["show", "HEAD:f.txt"]), "1\n2\nA\n3");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "1\n2\nA\nB\n3\n"
    );
    let status = server
        .working_copy_status(Parameters(sel("main")))
        .await
        .unwrap()
        .0;
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
    assert!(matches!(result.result, SaveResultDto::Clean { .. }));

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
    assert!(matches!(result.result, SaveResultDto::Clean { .. }));

    assert_eq!(git(dir.path(), &["show", "HEAD:a.txt"]), "a\nedit-a");
    assert_eq!(git(dir.path(), &["show", "HEAD:b.txt"]), "b\nedit-b");
    assert!(
        server
            .working_copy_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .clean
    );
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
            session: sel("main"),
            limit: None,
            offset: None,
            fields: Some(vec![]),
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    let first = history.commits[1].change_id.clone();

    let result = server
        .squash_working_copy(Parameters(SquashWorkingCopyReq {
            session: sel("main"),
            dest: first,
            message: Some("first (with a.txt)".into()),
            paths: Some(vec!["a.txt".into()]),
            hunks: None,
            patches: None,
            add_paths: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result.result, SaveResultDto::Clean { .. }));

    // "first" gained a.txt's change and the new message; b.txt stays uncommitted.
    assert_eq!(
        git_log_subjects(dir.path()),
        ["second", "first (with a.txt)"]
    );
    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "1\nA");
    let status = server
        .working_copy_status(Parameters(sel("main")))
        .await
        .unwrap()
        .0;
    assert!(!status.clean);
    assert_eq!(status.entries[0].files, vec!["b.txt".to_string()]);
}

#[tokio::test]
async fn list_history_can_include_working_copy_status() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first")]);
    let server = open_server(dir.path());
    std::fs::write(dir.path().join("a.txt"), "1\ndirty\n").unwrap();

    // Without the flag, no working-copy block is attached.
    let plain = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: Some(vec![]),
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(plain.working_copy.is_none());

    // With it, the uncommitted change rides along in one call.
    let with_wc = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: Some(vec![]),
            working_copy: Some(true),
        }))
        .await
        .unwrap()
        .0;
    let wc = with_wc.working_copy.expect("working-copy block present");
    assert!(!wc.clean);
    assert_eq!(wc.entries[0].files, vec!["a.txt".to_string()]);
}

#[tokio::test]
async fn commit_working_copy_add_paths_includes_a_brand_new_file() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first")]);
    let server = open_server(dir.path());

    // A brand-new file is untracked, so without add_paths the tree looks clean
    // and there is nothing to commit.
    std::fs::write(dir.path().join("new.txt"), "hello\n").unwrap();
    let err = expect_err(
        server
            .commit_working_copy(Parameters(commit_req("add new", None, None, None)))
            .await,
    );
    assert!(
        err.message.contains("clean"),
        "unexpected error: {}",
        err.message
    );

    // Naming it in add_paths pulls it into the commit.
    let mut req = commit_req("add new", None, None, None);
    req.add_paths = Some(vec!["new.txt".into()]);
    let result = server.commit_working_copy(Parameters(req)).await.unwrap().0;
    assert!(matches!(result.result, SaveResultDto::Clean { .. }));

    assert_eq!(git_log_subjects(dir.path()), ["add new", "first"]);
    assert_eq!(git(dir.path(), &["show", "HEAD:new.txt"]), "hello");
    assert!(
        server
            .working_copy_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .clean
    );
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn commit_working_copy_add_paths_combines_a_new_file_and_a_tracked_edit() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first")]);
    let server = open_server(dir.path());

    // A logical unit that both edits a tracked file and introduces a new one
    // lands as a single commit.
    std::fs::write(dir.path().join("a.txt"), "1\nedited\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "new\n").unwrap();

    let mut req = commit_req("feature", None, None, None);
    req.add_paths = Some(vec!["b.txt".into()]);
    let result = server.commit_working_copy(Parameters(req)).await.unwrap().0;
    assert!(matches!(result.result, SaveResultDto::Clean { .. }));

    assert_eq!(git(dir.path(), &["show", "HEAD:a.txt"]), "1\nedited");
    assert_eq!(git(dir.path(), &["show", "HEAD:b.txt"]), "new");
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn squash_working_copy_add_paths_folds_a_new_file_into_a_commit() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")],
    );
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("c.txt"), "new\n").unwrap();
    let history = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    let result = server
        .squash_working_copy(Parameters(SquashWorkingCopyReq {
            session: sel("main"),
            dest: history.commits[1].change_id[..8].to_string(),
            message: None,
            paths: None,
            hunks: None,
            patches: None,
            add_paths: Some(vec!["c.txt".into()]),
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result.result, SaveResultDto::Clean { .. }));

    // The new file folded into "first" (HEAD~1) and the tree is clean again.
    assert_eq!(git(dir.path(), &["show", "HEAD~1:c.txt"]), "new");
    assert_eq!(git_log_subjects(dir.path()), ["second", "first"]);
    assert!(
        server
            .working_copy_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .clean
    );
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}
