//! End-to-end tests for the `merge_out_commit` tool: introduce an artificial
//! merge above a commit and assert plain git sees the degenerate `merge(P, C)`
//! with `C` as the side branch, content unchanged.

mod common;

use commedit_mcp::dto::{MergeOutReq, SaveResultDto};
use commedit_mcp::server::CommeditServer;
use common::{expect_err, git, init_merge_repo, init_repo, is_merge, open_server};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

/// Unwrap a clean save, returning the new head sha. The `..` absorbs whatever
/// extra fields the clean arm carries.
fn clean_head(result: &SaveResultDto) -> String {
    match result {
        SaveResultDto::Clean { head_sha, .. } => head_sha.clone().expect("clean save has a head"),
        SaveResultDto::Conflicts { commits, .. } => {
            panic!("expected a clean save, got conflicts in {commits:?}")
        }
    }
}

async fn merge_out(server: &CommeditServer, commit: &str) -> SaveResultDto {
    server
        .merge_out_commit(Parameters(MergeOutReq {
            commit: commit.into(),
            child: None,
        }))
        .await
        .expect("merge_out_commit call")
        .0
}

/// Subject of `rev` per plain git.
fn subject(dir: &std::path::Path, rev: &str) -> String {
    git(dir, &["log", "-1", "--format=%s", rev])
}

#[tokio::test]
async fn merge_out_mid_history() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(
        dir,
        &[
            ("a.txt", "one\n", "first"),
            ("b.txt", "two\n", "second"),
            ("c.txt", "three\n", "third"),
        ],
    );
    let server = open_server(dir);

    // Merge out the middle commit "second" (parent "first").
    let result = merge_out(&server, &git(dir, &["rev-parse", "HEAD~1"])).await;
    let head = clean_head(&result);

    // The tip is still "third", now sitting on the new merge.
    assert_eq!(subject(dir, &head), "third");
    assert!(is_merge(dir, "HEAD~1"), "the introduced commit is a merge");
    assert_eq!(subject(dir, "HEAD~1"), "Merge \"second\"");
    // Parent order [P, C]: first parent "first" (mainline), second "second" (side).
    assert_eq!(subject(dir, "HEAD~1^1"), "first");
    assert_eq!(subject(dir, "HEAD~1^2"), "second");
    // The merge carries C's tree (no change of its own), "third" intact.
    assert_eq!(git(dir, &["show", "HEAD~1:b.txt"]), "two");
    assert_eq!(git(dir, &["show", "HEAD:c.txt"]), "three");
    assert_eq!(git(dir, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn merge_out_the_tip_becomes_the_new_head() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(
        dir,
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let server = open_server(dir);

    let result = merge_out(&server, &git(dir, &["rev-parse", "HEAD"])).await;
    let head = clean_head(&result);

    assert!(is_merge(dir, &head), "the new tip is a merge");
    assert_eq!(subject(dir, &head), "Merge \"second\"");
    assert_eq!(subject(dir, "HEAD^1"), "first");
    assert_eq!(subject(dir, "HEAD^2"), "second");
    assert_eq!(git(dir, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn merge_out_refuses_a_merge_and_the_root() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_merge_repo(dir);
    let server = open_server(dir);

    // The branch tip is a merge — no single parent to fold out.
    let err = expect_err(
        server
            .merge_out_commit(Parameters(MergeOutReq {
                commit: git(dir, &["rev-parse", "HEAD"]),
                child: None,
            }))
            .await,
    );
    assert!(err.message.contains("single-parent"), "{}", err.message);

    // The root commit "base" has no real parent either.
    let root = git(dir, &["rev-list", "--max-parents=0", "HEAD"]);
    let err = expect_err(
        server
            .merge_out_commit(Parameters(MergeOutReq {
                commit: root,
                child: None,
            }))
            .await,
    );
    assert!(err.message.contains("single-parent"), "{}", err.message);
}
