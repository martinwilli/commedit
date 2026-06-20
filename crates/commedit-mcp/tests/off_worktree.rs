//! The MCP surface for editing a branch that is not checked out: a history edit
//! moves only that branch's ref, working-copy tools are refused, and reload_repo
//! can switch which branch the session edits.

mod common;

use commedit_mcp::dto::{CommitWorkingCopyReq, EditMessageReq, IdentityFieldsDto, ReloadRepoReq};
use common::{expect_err, git, init_repo, open_server, open_server_branch, sel};
use rmcp::handler::server::wrapper::Parameters;
use std::path::Path;
use tempfile::TempDir;

/// `main` (A, B, C) checked out + a `feature` branch (A, B, C, D) not checked out.
fn two_branch_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(
        dir,
        &[
            ("a.txt", "a\n", "A"),
            ("b.txt", "b\n", "B"),
            ("c.txt", "c\n", "C"),
        ],
    );
    git(dir, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.join("d.txt"), "d\n").unwrap();
    git(dir, &["add", "d.txt"]);
    git(dir, &["commit", "-q", "-m", "D"]);
    git(dir, &["checkout", "-q", "main"]);
    tmp
}

fn subjects(dir: &Path, r: &str) -> Vec<String> {
    git(dir, &["log", "--format=%s", r])
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn editing_an_off_worktree_branch_moves_only_its_ref() {
    let tmp = two_branch_repo();
    let dir = tmp.path();
    let main_before = git(dir, &["rev-parse", "main"]);
    let head_before = git(dir, &["rev-parse", "HEAD"]);

    let server = open_server_branch(dir, "feature");
    // B is feature~2 (feature is A, B, C, D).
    let b = git(dir, &["rev-parse", "feature~2"]);
    server
        .edit_message(Parameters(EditMessageReq {
            session: sel("feature"),
            commit: b,
            message: "B (edited)".to_string(),
        }))
        .await
        .expect("edit_message on the off-worktree branch");

    assert_eq!(subjects(dir, "feature"), vec!["D", "C", "B (edited)", "A"]);
    assert_eq!(git(dir, &["rev-parse", "main"]), main_before, "main frozen");
    assert_eq!(git(dir, &["rev-parse", "HEAD"]), head_before, "HEAD frozen");
    assert_eq!(git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(git(dir, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn working_copy_tools_are_refused_off_worktree() {
    let tmp = two_branch_repo();
    let server = open_server_branch(tmp.path(), "feature");

    let err = expect_err(
        server
            .commit_working_copy(Parameters(CommitWorkingCopyReq {
                session: sel("feature"),
                message: "x".to_string(),
                identity: IdentityFieldsDto::default(),
                paths: None,
                hunks: None,
                patches: None,
                add_paths: None,
            }))
            .await,
    );
    assert!(
        err.message.contains("not checked out"),
        "clear refusal: {}",
        err.message
    );
}

#[tokio::test]
async fn reload_repo_can_switch_to_an_off_worktree_branch() {
    let tmp = two_branch_repo();
    // Open worktree-bound on main, then switch the session to edit feature.
    let server = open_server(tmp.path());

    let resp = server
        .reload_repo(Parameters(ReloadRepoReq {
            session: sel("main"),
            path: None,
            branch: Some("feature".to_string()),
        }))
        .await
        .expect("reload onto feature")
        .0;
    assert_eq!(resp.session, "feature", "the session re-keyed to feature");
    assert_eq!(resp.branch.as_deref(), Some("feature"));
    assert!(!resp.worktree_bound, "feature is not checked out");
}
