//! The `absorb_working_copy` tool: preview a routing plan, then fold each
//! uncommitted hunk into its originating commit in one call.

mod common;

use commedit_mcp::dto::{AbsorbWorkingCopyReq, SaveResultDto};
use common::{expect_err, git, git_log_subjects, init_repo, open_server, sel};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

fn absorb_req(paths: Option<Vec<String>>, dry_run: bool) -> AbsorbWorkingCopyReq {
    AbsorbWorkingCopyReq {
        session: sel("main"),
        paths,
        dry_run,
    }
}

#[tokio::test]
async fn dry_run_then_apply_folds_each_hunk_home() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[
            ("f.txt", "a1\nAAA\na2\n", "A"),
            ("f.txt", "a1\nAAA\na2\nb1\nBBB\nb2\n", "B"),
            ("f.txt", "a1\nAAA\na2\nb1\nBBB\nb2\nc1\nCCC\nc2\n", "C"),
        ],
    );
    let server = open_server(dir.path());

    // Edit A's and C's blocks.
    std::fs::write(
        dir.path().join("f.txt"),
        "a1\nXA\na2\nb1\nBBB\nb2\nc1\nXC\nc2\n",
    )
    .unwrap();

    // Preview: a two-entry plan (A then C), nothing applied, tree still dirty.
    let preview = server
        .absorb_working_copy(Parameters(absorb_req(None, true)))
        .await
        .unwrap()
        .0;
    assert!(preview.dry_run);
    assert!(preview.applied.is_none());
    assert_eq!(
        preview
            .plan
            .iter()
            .map(|e| e.subject.as_str())
            .collect::<Vec<_>>(),
        vec!["A", "C"]
    );
    assert!(!preview.remaining);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "M f.txt");

    // Apply: folds clean, tree clean, each commit owns its edit.
    let applied = server
        .absorb_working_copy(Parameters(absorb_req(None, false)))
        .await
        .unwrap()
        .0;
    assert!(!applied.dry_run);
    assert!(matches!(applied.applied, Some(SaveResultDto::Clean { .. })));
    assert!(applied.working_copy.as_ref().is_some_and(|wc| wc.clean));
    assert_eq!(git_log_subjects(dir.path()), vec!["C", "B", "A"]);
    assert_eq!(git(dir.path(), &["show", "main~2:f.txt"]), "a1\nXA\na2");
    assert_eq!(
        git(dir.path(), &["show", "main:f.txt"]),
        "a1\nXA\na2\nb1\nBBB\nb2\nc1\nXC\nc2"
    );
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn clean_tree_is_rejected() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("f.txt", "a1\nAAA\na2\n", "A")]);
    let server = open_server(dir.path());

    let err = expect_err(
        server
            .absorb_working_copy(Parameters(absorb_req(None, false)))
            .await,
    );
    assert!(
        err.message.contains("no uncommitted changes"),
        "unexpected error: {}",
        err.message
    );
}
