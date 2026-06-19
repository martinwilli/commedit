//! `reload_repo` with a `path` re-homes the session to a *sibling worktree* of
//! the same repository — so one session can edit history isolated in a
//! `git worktree` and then re-home — while refusing any path outside the repo's
//! worktrees. No-arg reload keeps reloading the current repo in place.

mod common;

use std::path::Path;

use commedit_mcp::dto::{EditMessageReq, ListHistoryReq, ReloadRepoReq};
use commedit_mcp::server::CommeditServer;
use common::{expect_err, git, init_repo, open_server};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

async fn reload(server: &CommeditServer, path: Option<String>) -> commedit_mcp::dto::ReloadResp {
    server
        .reload_repo(Parameters(ReloadRepoReq { path, branch: None }))
        .await
        .unwrap()
        .0
}

async fn tip_subject(server: &CommeditServer) -> String {
    server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0
        .commits[0]
        .subject
        .clone()
}

/// The change_id of the session's current tip — a stable ref to edit.
async fn tip_ref(server: &CommeditServer) -> String {
    server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0
        .commits[0]
        .change_id
        .clone()
}

/// A main checkout plus a linked worktree on its own branch, both under one
/// container dir so the worktree is a sibling of the repo (not nested in it).
fn repo_with_worktree() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let main = tmp.path().join("main");
    std::fs::create_dir(&main).unwrap();
    init_repo(&main, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    let wt = tmp.path().join("wt"); // git creates it
    git(&main, &["worktree", "add", "-b", "feat", &s(&wt), "main"]);
    (tmp, main, wt)
}

#[tokio::test]
async fn reload_with_a_path_re_homes_the_session_to_a_sibling_worktree() {
    let (_tmp, main, wt) = repo_with_worktree();
    let server = open_server(&main);

    // Re-home onto the worktree: the response reports the worktree as the root
    // and its branch tip (initially main's tip, since `feat` branched off main).
    let resp = reload(&server, Some(s(&wt))).await;
    assert_eq!(
        std::fs::canonicalize(&wt).unwrap().to_str().unwrap(),
        resp.root
    );
    assert_eq!(
        resp.head_sha.unwrap(),
        git(&wt, &["rev-parse", "HEAD"]),
        "the session now tracks the worktree's branch"
    );

    // A mutation now lands on the worktree's branch, leaving `main` untouched.
    let main_before = git(&main, &["rev-parse", "main"]);
    let tip = tip_ref(&server).await;
    server
        .edit_message(Parameters(EditMessageReq {
            commit: tip,
            message: "B reworded".into(),
        }))
        .await
        .unwrap();
    assert_eq!(
        git(&wt, &["log", "-1", "--format=%s", "feat"]),
        "B reworded"
    );
    assert_eq!(
        git(&main, &["rev-parse", "main"]),
        main_before,
        "the main checkout's branch is untouched by edits on the worktree"
    );
}

#[tokio::test]
async fn the_session_can_re_home_back_to_the_main_checkout() {
    let (_tmp, main, wt) = repo_with_worktree();
    let server = open_server(&main);

    reload(&server, Some(s(&wt))).await;
    let back = reload(&server, Some(s(&main))).await;
    assert_eq!(
        std::fs::canonicalize(&main).unwrap().to_str().unwrap(),
        back.root,
        "a worktree-list member (the main checkout) is a valid re-home target"
    );
    assert_eq!(tip_subject(&server).await, "B");
}

#[tokio::test]
async fn a_path_outside_the_repository_is_refused_and_the_session_is_unaffected() {
    let (_tmp, main, _wt) = repo_with_worktree();
    let server = open_server(&main);

    // A wholly separate repository, not a worktree of this one.
    let other = TempDir::new().unwrap();
    init_repo(other.path(), &[("z.txt", "z\n", "Z")]);

    let err = expect_err(
        server
            .reload_repo(Parameters(ReloadRepoReq {
                path: Some(s(other.path())),
                branch: None,
            }))
            .await,
    );
    assert!(
        err.message.contains("not a worktree of this repository"),
        "message: {}",
        err.message
    );
    // The session still serves the original repo.
    assert_eq!(tip_subject(&server).await, "B");

    // A path that is not a git repository at all is refused too.
    let plain = TempDir::new().unwrap();
    let err = expect_err(
        server
            .reload_repo(Parameters(ReloadRepoReq {
                path: Some(s(plain.path())),
                branch: None,
            }))
            .await,
    );
    assert!(
        err.message.contains("not inside a git repository"),
        "message: {}",
        err.message
    );
}

#[tokio::test]
async fn a_no_arg_reload_still_reloads_the_current_repo_in_place() {
    let tmp = TempDir::new().unwrap();
    init_repo(tmp.path(), &[("a.txt", "a\n", "A")]);
    let server = open_server(tmp.path());

    let resp = reload(&server, None).await;
    assert_eq!(
        std::fs::canonicalize(tmp.path()).unwrap().to_str().unwrap(),
        resp.root
    );
    assert_eq!(
        resp.head_sha.unwrap(),
        git(tmp.path(), &["rev-parse", "HEAD"])
    );
}
