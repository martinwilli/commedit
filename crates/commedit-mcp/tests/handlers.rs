//! End-to-end tool handler tests against scratch git repos, asserting both the
//! responses and (for mutations) the resulting plain-git state.

mod common;

use common::{expect_err, git, init_merge_repo, init_repo, open_server};
use commedit_mcp::dto::{ListHistoryReq, ShowCommitReq};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

#[tokio::test]
async fn list_history_returns_the_branch_commits_with_refs() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[
            ("a.txt", "one\n", "first"),
            ("b.txt", "two\n", "second"),
            ("c.txt", "three\n", "third"),
        ],
    );
    let server = open_server(dir.path());

    let resp = server
        .list_history(Parameters(ListHistoryReq { limit: None }))
        .await
        .unwrap()
        .0;
    let subjects: Vec<&str> = resp.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["third", "second", "first"]);
    assert_eq!(resp.head_sha.as_deref(), Some(resp.commits[0].sha.as_str()));
    assert!(!resp.has_more);
    assert_eq!(resp.trash_count, 0);

    // The tip carries the checked-out branch decoration.
    let tip_refs = &resp.commits[0].refs;
    assert!(tip_refs.iter().any(|r| r.name == "main" && r.kind == "branch" && r.current));
    // The oldest commit has no parents (the virtual root is filtered).
    assert!(resp.commits[2].parent_shas.is_empty());
    assert_eq!(resp.commits[0].parent_shas, vec![resp.commits[1].sha.clone()]);
}

#[tokio::test]
async fn list_history_honours_the_limit() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("a.txt", "2\n", "second"), ("a.txt", "3\n", "third")],
    );
    let server = open_server(dir.path());

    let resp = server
        .list_history(Parameters(ListHistoryReq { limit: Some(2) }))
        .await
        .unwrap()
        .0;
    assert_eq!(resp.commits.len(), 2);
    assert!(resp.has_more);
    assert_eq!(resp.commits[0].subject, "third");
}

#[tokio::test]
async fn list_history_marks_merges() {
    let dir = TempDir::new().unwrap();
    init_merge_repo(dir.path());
    let server = open_server(dir.path());

    let resp = server
        .list_history(Parameters(ListHistoryReq { limit: None }))
        .await
        .unwrap()
        .0;
    let merge = &resp.commits[0];
    assert_eq!(merge.subject, "merge");
    assert!(merge.is_merge);
    assert_eq!(merge.parent_shas.len(), 2);
    assert!(resp.commits[1..].iter().all(|c| !c.is_merge));
}

#[tokio::test]
async fn show_commit_renders_diffs_and_optionally_contents() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "one\n", "first"), ("a.txt", "one\ntwo\n", "second")],
    );
    let server = open_server(dir.path());

    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None }))
        .await
        .unwrap()
        .0;
    let sha = history.commits[0].sha.clone();

    let resp = server
        .show_commit(Parameters(ShowCommitReq { sha: sha.clone(), include_contents: None }))
        .await
        .unwrap()
        .0;
    assert_eq!(resp.commit.subject, "second");
    assert_eq!(resp.files.len(), 1);
    let file = &resp.files[0];
    assert_eq!(file.path, "a.txt");
    assert_eq!(file.kind, "modified");
    assert!(file.diff.as_deref().unwrap().contains("+two"));
    assert!(file.old_text.is_none() && file.new_text.is_none());

    let with = server
        .show_commit(Parameters(ShowCommitReq { sha, include_contents: Some(true) }))
        .await
        .unwrap()
        .0;
    assert_eq!(with.files[0].old_text.as_deref(), Some("one\n"));
    assert_eq!(with.files[0].new_text.as_deref(), Some("one\ntwo\n"));
}

#[tokio::test]
async fn show_commit_rejects_an_unknown_sha() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "one\n", "first")]);
    let server = open_server(dir.path());

    let err = expect_err(
        server
            .show_commit(Parameters(ShowCommitReq {
                sha: "0123456789abcdef0123456789abcdef01234567".into(),
                include_contents: None,
            }))
            .await,
    );
    assert!(err.message.contains("not found"), "unexpected error: {}", err.message);
}

#[tokio::test]
async fn list_trash_starts_empty() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "one\n", "first")]);
    let server = open_server(dir.path());

    let resp = server.list_trash().await.unwrap().0;
    assert!(resp.commits.is_empty());
}

#[tokio::test]
async fn working_copy_status_reflects_dirty_tracked_files() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "one\n", "first")]);
    let server = open_server(dir.path());

    let clean = server.working_copy_status().await.unwrap().0;
    assert!(clean.clean);
    assert!(clean.entries.is_empty());
    assert!(clean.session_start_head_sha.is_some());

    std::fs::write(dir.path().join("a.txt"), "edited\n").unwrap();
    let dirty = server.working_copy_status().await.unwrap().0;
    assert!(!dirty.clean);
    assert_eq!(dirty.entries.len(), 1);
    assert_eq!(dirty.entries[0].files, vec!["a.txt".to_string()]);
    assert!(!dirty.entries[0].has_conflict);

    // The entry's sha reads as a commit: its diff is the uncommitted change.
    let shown = server
        .show_commit(Parameters(ShowCommitReq {
            sha: dirty.entries[0].sha.clone(),
            include_contents: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(shown.files[0].diff.as_deref().unwrap().contains("+edited"));
}

#[tokio::test]
async fn session_diff_and_operations_start_empty() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "one\n", "first")]);
    let server = open_server(dir.path());

    let diff = server.session_diff().await.unwrap().0;
    assert!(diff.files.is_empty());

    let ops = server.list_operations().await.unwrap().0;
    assert!(ops.ops.is_empty());
    assert_eq!(ops.cursor, 0);
    assert!(!ops.can_undo && !ops.can_redo && !ops.pending);

    let pending = server.pending_status().await.unwrap().0;
    assert!(!pending.pending);
    assert!(pending.conflicts.is_empty());
    assert_eq!(pending.git_head_sha, pending.jj_head_sha);

    // An untouched session shows a clean git status.
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}
