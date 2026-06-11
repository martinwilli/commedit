//! Working-copy preservation through the MCP surface: uncommitted changes
//! ride through rewrites, fold into commits, and are discarded only with an
//! explicit confirmation.

mod common;

use common::{expect_err, git, git_log_subjects, init_repo, open_server};
use commedit_mcp::dto::{
    DiscardWorkingCopyReq, EditMessageReq, ListHistoryReq, SaveResultDto, SquashWorkingCopyReq,
};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

#[tokio::test]
async fn uncommitted_changes_survive_a_rewrite() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")]);
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("a.txt"), "1\nlocal edit\n").unwrap();
    assert!(!server.working_copy_status().await.unwrap().0.clean);

    // Rewrite the bottom commit's message — the dirty file must ride along.
    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, brief: None }))
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
    init_repo(dir.path(), &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")]);
    let server = open_server(dir.path());

    // A clean working copy has nothing to fold.
    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, brief: None }))
        .await
        .unwrap()
        .0;
    let first = history.commits[1].clone();
    let err = expect_err(
        server
            .squash_working_copy(Parameters(SquashWorkingCopyReq {
                dest: first.sha.clone(),
            }))
            .await,
    );
    assert!(err.message.contains("clean"), "unexpected error: {}", err.message);

    // Fold a dirty a.txt into the bottom commit ("first" introduced a.txt).
    std::fs::write(dir.path().join("a.txt"), "1\nfolded\n").unwrap();
    let result = server
        .squash_working_copy(Parameters(SquashWorkingCopyReq { dest: first.sha }))
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
    assert!(err.message.contains("confirm"), "unexpected error: {}", err.message);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "dirty\n");

    let resp = server
        .discard_working_copy(Parameters(DiscardWorkingCopyReq { confirm: true }))
        .await
        .unwrap()
        .0;
    assert!(resp.ok);

    // The tree is reset to the branch tip.
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "1\n");
    assert!(server.working_copy_status().await.unwrap().0.clean);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    // The discard is on the session op-log (undo can bring the changes back).
    let ops = server.list_operations().await.unwrap().0;
    assert_eq!(ops.ops.len(), 1);
    assert!(ops.ops[0].label.contains("Drop uncommitted"), "label: {}", ops.ops[0].label);
}

#[tokio::test]
async fn untracked_files_stay_out_of_the_working_copy_and_alive_on_disk() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")]);
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("untracked.txt"), "keep me\n").unwrap();
    let status = server.working_copy_status().await.unwrap().0;
    assert!(status.clean, "untracked files are not uncommitted changes");

    // A rewrite leaves the untracked file untouched on disk.
    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, brief: None }))
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
    init_repo(dir.path(), &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")]);
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("a.txt"), "1\nfolded\n").unwrap();
    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, brief: None }))
        .await
        .unwrap()
        .0;
    let result = server
        .squash_working_copy(Parameters(SquashWorkingCopyReq {
            dest: history.commits[1].change_id[..8].to_string(),
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "1\nfolded");
    assert!(server.working_copy_status().await.unwrap().0.clean);
}
