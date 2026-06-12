//! The session op-log surface: undo/redo/jump time-travel, trash staleness
//! after an undone drop, and reload_repo picking up external git changes.

mod common;

use commedit_mcp::dto::{
    DiscardWorkingCopyReq, DropCommitReq, EditMessageReq, FileContentDto, JumpToOperationReq,
    ListHistoryReq, ReplaceFilesReq, RestoreCommitReq, SaveResultDto,
};
use common::{expect_err, git, git_log_subjects, init_repo, open_server};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

async fn edit_tip_message(server: &commedit_mcp::server::CommeditServer, message: &str) {
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
            commit: history.commits[0].sha.clone(),
            message: message.into(),
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));
}

#[tokio::test]
async fn undo_redo_and_jump_step_the_recorded_states() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")],
    );
    let server = open_server(dir.path());

    edit_tip_message(&server, "second v2").await;
    edit_tip_message(&server, "second v3").await;

    let ops = server.list_operations().await.unwrap().0;
    assert_eq!(ops.ops.len(), 2);
    assert_eq!(ops.cursor, 2);
    assert!(ops.can_undo && !ops.can_redo);
    assert!(
        ops.ops[0].label.contains("Edit message"),
        "label: {}",
        ops.ops[0].label
    );

    // Undo: back to v2; git follows.
    let resp = server.undo().await.unwrap().0;
    assert_eq!(resp.cursor, 1);
    assert_eq!(git_log_subjects(dir.path()), ["second v2", "first"]);
    assert_eq!(
        resp.head_sha.unwrap(),
        git(dir.path(), &["rev-parse", "HEAD"])
    );

    // Redo: forward to v3 again.
    let resp = server.redo().await.unwrap().0;
    assert_eq!(resp.cursor, 2);
    assert_eq!(git_log_subjects(dir.path()), ["second v3", "first"]);

    // Jump to the session-start floor: everything undone.
    let resp = server
        .jump_to_operation(Parameters(JumpToOperationReq { index: 0 }))
        .await
        .unwrap()
        .0;
    assert_eq!(resp.cursor, 0);
    assert_eq!(git_log_subjects(dir.path()), ["second", "first"]);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");

    // Bounds are explicit errors.
    let err = expect_err(server.undo().await);
    assert!(
        err.message.contains("session start"),
        "unexpected error: {}",
        err.message
    );
    let err = expect_err(
        server
            .jump_to_operation(Parameters(JumpToOperationReq { index: 5 }))
            .await,
    );
    assert!(
        err.message.contains("out of range"),
        "unexpected error: {}",
        err.message
    );

    // A fresh edit truncates the redo tail.
    edit_tip_message(&server, "second v4").await;
    let err = expect_err(server.redo().await);
    assert!(
        err.message.contains("redo"),
        "unexpected error: {}",
        err.message
    );
    assert_eq!(git_log_subjects(dir.path()), ["second v4", "first"]);
}

#[tokio::test]
async fn a_discard_is_recorded_but_its_content_is_gone() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first")]);
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("a.txt"), "dirty\n").unwrap();
    server
        .discard_working_copy(Parameters(DiscardWorkingCopyReq { confirm: true }))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "1\n"
    );

    // The discard shows up as a recorded op, but undoing past it restores the
    // session-start state — which never contained the discarded edit. This is
    // the one unrecoverable action, as the tool description warns.
    server.undo().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "1\n"
    );
}

#[tokio::test]
async fn restoring_a_trash_entry_stale_after_undo_fails_cleanly() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[
            ("a.txt", "1\n", "first"),
            ("b.txt", "2\n", "second"),
            ("c.txt", "3\n", "third"),
        ],
    );
    let server = open_server(dir.path());

    let history = server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: None,
        }))
        .await
        .unwrap()
        .0;
    let dropped = server
        .drop_commit(Parameters(DropCommitReq {
            commit: history.commits[1].sha.clone(),
        }))
        .await
        .unwrap()
        .0;

    // Undo the drop: the commit is back in history, but the trash entry
    // lingers (session trash is MCP state, not part of the jj op log).
    server.undo().await.unwrap();
    assert_eq!(git_log_subjects(dir.path()), ["third", "second", "first"]);
    assert_eq!(server.list_trash().await.unwrap().0.commits.len(), 1);

    // Restoring the stale entry fails with a clean error; git is untouched.
    let bottom = server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: None,
        }))
        .await
        .unwrap()
        .0
        .commits[2]
        .sha
        .clone();
    let err = expect_err(
        server
            .restore_commit(Parameters(RestoreCommitReq {
                commit: dropped.dropped.sha.clone(),
                new_parent: bottom,
                child: None,
            }))
            .await,
    );
    assert!(
        err.message.contains("no way to splice"),
        "unexpected error: {}",
        err.message
    );
    assert_eq!(git_log_subjects(dir.path()), ["third", "second", "first"]);
}

#[tokio::test]
async fn reload_repo_picks_up_external_commits_and_resets_the_session() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")],
    );
    let server = open_server(dir.path());

    // Session state to be discarded: an op and a trash entry.
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
        .drop_commit(Parameters(DropCommitReq {
            commit: history.commits[1].sha.clone(),
        }))
        .await
        .unwrap();
    assert_eq!(server.list_trash().await.unwrap().0.commits.len(), 1);

    // An out-of-band commit the running session can't see.
    std::fs::write(dir.path().join("x.txt"), "external\n").unwrap();
    git(dir.path(), &["add", "x.txt"]);
    git(dir.path(), &["commit", "-qm", "external"]);

    let resp = server.reload_repo().await.unwrap().0;
    assert_eq!(
        resp.head_sha.unwrap(),
        git(dir.path(), &["rev-parse", "HEAD"])
    );

    // The fresh import sees the external commit; trash and ops are reset.
    let history = server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: None,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(history.commits[0].subject, "external");
    assert_eq!(history.trash_count, 0);
    let ops = server.list_operations().await.unwrap().0;
    assert!(ops.ops.is_empty());
    assert_eq!(ops.cursor, 0);
}

#[tokio::test]
async fn reload_repo_drops_a_pending_rewrite_without_touching_git() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[
            ("f.txt", "1\n2\n3\n", "base"),
            ("f.txt", "1\nA\n3\n", "A"),
            ("f.txt", "1\nB\n3\n", "B"),
        ],
    );
    let head_before = git(dir.path(), &["rev-parse", "HEAD"]);
    let server = open_server(dir.path());

    // A conflicting edit leaves a pending rewrite.
    let history = server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: None,
        }))
        .await
        .unwrap()
        .0;
    let a = history.commits.iter().find(|c| c.subject == "A").unwrap();
    let result = server
        .replace_files(Parameters(ReplaceFilesReq {
            commit: a.sha.clone(),
            files: vec![FileContentDto {
                path: "f.txt".into(),
                content: "1\nX\n3\n".into(),
            }],
            delete_paths: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Conflicts { .. }));
    assert!(server.pending_status().await.unwrap().0.pending);

    // Reload while pending: allowed, the held rewrite is simply dropped.
    server.reload_repo().await.unwrap();
    assert!(!server.pending_status().await.unwrap().0.pending);
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_log_subjects(dir.path()), ["B", "A", "base"]);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}
