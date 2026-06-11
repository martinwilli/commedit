//! The conflict-resolution protocol end-to-end: a mutation whose descendant
//! rebase conflicts is held back from git in full, then resolved oldest-first
//! through the conflict tools (or discarded by abort_rewrite).

mod common;

use common::{expect_err, git, git_log_subjects, init_repo, open_server};
use commedit_mcp::dto::{
    ConflictFileEditDto, DropCommitReq, EditMessageReq, FileContentDto, ListHistoryReq,
    ReadConflictReq, ReplaceFilesReq, ResolveConflictsReq, SaveResultDto,
};
use commedit_mcp::server::CommeditServer;
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

/// Three commits all editing the same line of `f.txt` — editing the middle
/// commit's content makes the rebase of its descendant conflict.
fn conflicting_repo(dir: &std::path::Path) {
    init_repo(
        dir,
        &[
            ("f.txt", "1\n2\n3\n", "base"),
            ("f.txt", "1\nA\n3\n", "A"),
            ("f.txt", "1\nB\n3\n", "B"),
        ],
    );
}

/// Rewrite commit "A"'s content so the descendant "B" no longer applies.
async fn conflicting_edit(server: &CommeditServer) -> SaveResultDto {
    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None }))
        .await
        .unwrap()
        .0;
    let a = history.commits.iter().find(|c| c.subject == "A").unwrap();
    server
        .replace_files(Parameters(ReplaceFilesReq {
            sha: a.sha.clone(),
            files: vec![FileContentDto { path: "f.txt".into(), content: "1\nX\n3\n".into() }],
        }))
        .await
        .unwrap()
        .0
}

#[tokio::test]
async fn a_conflicting_edit_is_held_back_then_resolved_oldest_first() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let head_before = git(dir.path(), &["rev-parse", "HEAD"]);
    let server = open_server(dir.path());

    let mut result = conflicting_edit(&server).await;
    assert!(
        matches!(result, SaveResultDto::Conflicts { .. }),
        "the non-commuting edit should conflict"
    );

    // Held back in full: git history, HEAD and worktree untouched.
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_log_subjects(dir.path()), ["B", "A", "base"]);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");

    // The pending state is also visible via pending_status.
    let pending = server.pending_status().await.unwrap().0;
    assert!(pending.pending);
    assert!(!pending.conflicts.is_empty());
    assert_ne!(pending.git_head_sha, pending.jj_head_sha);

    // No other mutation may run while pending.
    let stale = git(dir.path(), &["rev-parse", "HEAD"]);
    let err = expect_err(
        server
            .edit_message(Parameters(EditMessageReq { sha: stale, message: "x".into() }))
            .await,
    );
    assert!(err.message.contains("pending"), "unexpected error: {}", err.message);

    // Resolve oldest-first until the chain is clean.
    let mut steps = 0;
    while let SaveResultDto::Conflicts { commits, guidance } = result {
        assert!(guidance.contains("OLDEST"), "guidance rides along");
        let oldest = &commits[0];
        let path = &oldest.files[0];
        assert!(path.resolvable);

        let file = server
            .read_conflict(Parameters(ReadConflictReq {
                change_id: oldest.change_id.clone(),
                path: path.path.clone(),
            }))
            .await
            .unwrap()
            .0;
        assert!(file.text.contains("<<<<<<<"), "markers present: {}", file.text);
        assert!(!file.text.contains("|||||||"), "2-way markers omit the base");

        result = server
            .resolve_conflicts(Parameters(ResolveConflictsReq {
                change_id: oldest.change_id.clone(),
                files: vec![ConflictFileEditDto {
                    path: path.path.clone(),
                    text: "1\nR\n3\n".into(),
                    marker_len: file.marker_len,
                }],
            }))
            .await
            .unwrap()
            .0;
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }

    // Clean: the rewrite (with resolutions) is exported to plain git.
    assert!(!server.pending_status().await.unwrap().0.pending);
    assert_eq!(git_log_subjects(dir.path()), ["B", "A", "base"]);
    assert_eq!(git(dir.path(), &["show", "HEAD~1:f.txt"]), "1\nX\n3");
    assert_eq!(git(dir.path(), &["show", "HEAD:f.txt"]), "1\nR\n3");
    assert_eq!(git(dir.path(), &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    let tree = git(dir.path(), &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree.contains(".jjconflict"), "no conflict residue: {tree}");
    git(dir.path(), &["fsck", "--no-progress"]);
}

#[tokio::test]
async fn read_conflict_validates_change_and_path() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let server = open_server(dir.path());

    // Nothing pending yet.
    let err = expect_err(
        server
            .read_conflict(Parameters(ReadConflictReq {
                change_id: "00".into(),
                path: "f.txt".into(),
            }))
            .await,
    );
    assert!(err.message.contains("pending"), "unexpected error: {}", err.message);

    let result = conflicting_edit(&server).await;
    let SaveResultDto::Conflicts { commits, .. } = result else {
        panic!("expected conflicts");
    };

    let err = expect_err(
        server
            .read_conflict(Parameters(ReadConflictReq {
                change_id: commits[0].change_id.clone(),
                path: "nope.txt".into(),
            }))
            .await,
    );
    assert!(err.message.contains("f.txt"), "names the real conflicted files: {}", err.message);
}

#[tokio::test]
async fn abort_rewrite_restores_the_original_history() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let head_before = git(dir.path(), &["rev-parse", "HEAD"]);
    let server = open_server(dir.path());

    // Abort without a pending rewrite is refused.
    let err = expect_err(server.abort_rewrite().await);
    assert!(err.message.contains("pending"), "unexpected error: {}", err.message);

    let result = conflicting_edit(&server).await;
    assert!(matches!(result, SaveResultDto::Conflicts { .. }));

    let resp = server.abort_rewrite().await.unwrap().0;
    assert!(resp.ok);
    assert_eq!(resp.head_sha.as_deref(), Some(head_before.as_str()));
    assert!(!server.pending_status().await.unwrap().0.pending);

    // Git never saw any of it; mutations work again.
    assert_eq!(git_log_subjects(dir.path()), ["B", "A", "base"]);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    let tip = git(dir.path(), &["rev-parse", "HEAD"]);
    let clean = server
        .edit_message(Parameters(EditMessageReq { sha: tip, message: "B, edited".into() }))
        .await
        .unwrap()
        .0;
    assert!(matches!(clean, SaveResultDto::Clean { .. }));
    assert_eq!(git_log_subjects(dir.path()), ["B, edited", "A", "base"]);
}

#[tokio::test]
async fn a_conflicted_drop_lands_in_the_trash_only_after_settling_clean() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let server = open_server(dir.path());

    // Dropping "A" leaves "B"'s same-line edit dangling: a true conflict.
    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None }))
        .await
        .unwrap()
        .0;
    let a = history.commits.iter().find(|c| c.subject == "A").unwrap();
    let resp = server
        .drop_commit(Parameters(DropCommitReq { sha: a.sha.clone() }))
        .await
        .unwrap()
        .0;
    let SaveResultDto::Conflicts { commits, .. } = resp.result else {
        panic!("expected the drop to conflict");
    };
    assert_eq!(resp.dropped.subject, "A");

    // While pending, the trash push is only staged — not visible yet.
    assert!(server.list_trash().await.unwrap().0.commits.is_empty());

    // Resolving the conflict settles the drop; now the trash has it.
    let oldest = &commits[0];
    let file = server
        .read_conflict(Parameters(ReadConflictReq {
            change_id: oldest.change_id.clone(),
            path: oldest.files[0].path.clone(),
        }))
        .await
        .unwrap()
        .0;
    let result = server
        .resolve_conflicts(Parameters(ResolveConflictsReq {
            change_id: oldest.change_id.clone(),
            files: vec![ConflictFileEditDto {
                path: oldest.files[0].path.clone(),
                text: "1\nB\n3\n".into(),
                marker_len: file.marker_len,
            }],
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    let trash = server.list_trash().await.unwrap().0;
    assert_eq!(trash.commits.len(), 1);
    assert_eq!(trash.commits[0].subject, "A");
    assert_eq!(git_log_subjects(dir.path()), ["B", "base"]);
}

#[tokio::test]
async fn an_aborted_drop_leaves_the_trash_untouched() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let server = open_server(dir.path());

    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None }))
        .await
        .unwrap()
        .0;
    let a = history.commits.iter().find(|c| c.subject == "A").unwrap();
    let resp = server
        .drop_commit(Parameters(DropCommitReq { sha: a.sha.clone() }))
        .await
        .unwrap()
        .0;
    assert!(matches!(resp.result, SaveResultDto::Conflicts { .. }));

    server.abort_rewrite().await.unwrap();
    assert!(server.list_trash().await.unwrap().0.commits.is_empty());
    assert_eq!(git_log_subjects(dir.path()), ["B", "A", "base"]);
}

#[tokio::test]
async fn finalize_is_a_clean_noop_without_a_pending_rewrite() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let server = open_server(dir.path());

    let result = server.finalize().await.unwrap().0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));
}
