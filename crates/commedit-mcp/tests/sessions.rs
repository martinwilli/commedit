//! The multi-tenant session registry: several independent editing sessions over
//! one repository, addressed per tool call by id (the branch short-name). Two
//! sessions edit disjoint branches in parallel; a branch can be opened only once;
//! every session-operating tool rejects an unknown id; the last session can't be
//! closed; and reloading one session leaves the others untouched.

mod common;

use commedit_mcp::dto::{
    DropCommitReq, EditMessageReq, ListHistoryReq, OpenSessionReq, ReloadRepoReq,
};
use commedit_mcp::server::CommeditServer;
use common::{expect_err, git, init_repo, open_server, sel};
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

/// A main checkout plus a linked worktree on its own branch `feat`, both under one
/// container dir (the worktree a sibling of the repo, not nested in it).
fn repo_with_worktree() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let main = tmp.path().join("main");
    std::fs::create_dir(&main).unwrap();
    init_repo(&main, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    let wt = tmp.path().join("wt"); // git creates it
    git(
        &main,
        &[
            "worktree",
            "add",
            "-b",
            "feat",
            wt.to_str().unwrap(),
            "main",
        ],
    );
    (tmp, main, wt)
}

/// The change_id of the commit with subject `subject` in `session`'s history.
async fn change_id_of(server: &CommeditServer, session: &str, subject: &str) -> String {
    server
        .list_history(Parameters(ListHistoryReq {
            session: sel(session),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .expect("list_history")
        .0
        .commits
        .into_iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("no commit {subject:?} in session {session}"))
        .change_id
}

async fn open_feature(server: &CommeditServer) -> commedit_mcp::dto::OpenSessionResp {
    server
        .open_session(Parameters(OpenSessionReq {
            branch: "feature".into(),
        }))
        .await
        .expect("open feature session")
        .0
}

#[tokio::test]
async fn two_sessions_edit_disjoint_branches_in_parallel() {
    let tmp = two_branch_repo();
    let dir = tmp.path();
    let server = open_server(dir);

    // The launch session is `main`; open a second over the off-worktree `feature`.
    let opened = open_feature(&server).await;
    assert_eq!(opened.session, "feature");
    assert!(!opened.worktree_bound, "feature is not checked out");

    // list_sessions sees both, id-sorted, with no per-session selector needed.
    let ids: Vec<String> = server
        .list_sessions()
        .await
        .unwrap()
        .0
        .sessions
        .into_iter()
        .map(|s| s.session)
        .collect();
    assert_eq!(ids, vec!["feature".to_string(), "main".to_string()]);

    // Each session edits a commit unique to its branch, issued concurrently.
    let main_c = change_id_of(&server, "main", "C").await;
    let feat_d = change_id_of(&server, "feature", "D").await;
    let main_before = git(dir, &["rev-parse", "main"]);
    let feat_before = git(dir, &["rev-parse", "feature"]);

    let (r_main, r_feat) = tokio::join!(
        server.edit_message(Parameters(EditMessageReq {
            session: sel("main"),
            commit: main_c,
            message: "C (on main)".into(),
        })),
        server.edit_message(Parameters(EditMessageReq {
            session: sel("feature"),
            commit: feat_d,
            message: "D (on feature)".into(),
        })),
    );
    r_main.expect("edit on main");
    r_feat.expect("edit on feature");

    // Both branches moved, to distinct new tips, each carrying only its own edit.
    assert_eq!(subjects(dir, "main"), vec!["C (on main)", "B", "A"]);
    assert_eq!(
        subjects(dir, "feature"),
        vec!["D (on feature)", "C", "B", "A"]
    );
    assert_ne!(git(dir, &["rev-parse", "main"]), main_before);
    assert_ne!(git(dir, &["rev-parse", "feature"]), feat_before);
    assert_ne!(
        git(dir, &["rev-parse", "main"]),
        git(dir, &["rev-parse", "feature"]),
        "the two sessions landed on disjoint refs"
    );
}

#[tokio::test]
async fn open_session_refuses_a_branch_already_open() {
    let tmp = two_branch_repo();
    let server = open_server(tmp.path());

    // The launch session already holds `main`.
    let err = expect_err(
        server
            .open_session(Parameters(OpenSessionReq {
                branch: "main".into(),
            }))
            .await,
    );
    assert!(err.message.contains("already open"), "{}", err.message);

    // Opening `feature` once works; a second time is refused.
    open_feature(&server).await;
    let err = expect_err(
        server
            .open_session(Parameters(OpenSessionReq {
                branch: "feature".into(),
            }))
            .await,
    );
    assert!(err.message.contains("already open"), "{}", err.message);

    // A branch that does not exist is refused too.
    let err = expect_err(
        server
            .open_session(Parameters(OpenSessionReq {
                branch: "ghost".into(),
            }))
            .await,
    );
    assert!(err.message.contains("no local branch"), "{}", err.message);
}

#[tokio::test]
async fn session_operating_tools_reject_an_unknown_session() {
    let tmp = two_branch_repo();
    let server = open_server(tmp.path());

    // A tool with arguments...
    let err = expect_err(
        server
            .list_history(Parameters(ListHistoryReq {
                session: sel("nope"),
                limit: None,
                offset: None,
                fields: None,
                working_copy: None,
            }))
            .await,
    );
    assert!(err.message.contains("no open session"), "{}", err.message);
    assert!(
        err.message.contains("main"),
        "lists the open ones: {}",
        err.message
    );

    // ...and an otherwise argument-less one.
    let err = expect_err(server.working_copy_status(Parameters(sel("nope"))).await);
    assert!(err.message.contains("no open session"), "{}", err.message);

    // list_sessions needs no selector and still works.
    assert_eq!(server.list_sessions().await.unwrap().0.sessions.len(), 1);
}

#[tokio::test]
async fn close_session_refuses_the_last_remaining_session() {
    let tmp = two_branch_repo();
    let server = open_server(tmp.path());

    // Only `main` is open: it can't be closed.
    let err = expect_err(server.close_session(Parameters(sel("main"))).await);
    assert!(err.message.contains("last open session"), "{}", err.message);

    // Open `feature`; now `main` may close, leaving just `feature`.
    open_feature(&server).await;
    let resp = server
        .close_session(Parameters(sel("main")))
        .await
        .expect("close main")
        .0;
    assert_eq!(resp.closed, "main");
    let remaining: Vec<String> = resp.sessions.into_iter().map(|s| s.session).collect();
    assert_eq!(remaining, vec!["feature".to_string()]);

    // `feature` is now the last one — refused.
    let err = expect_err(server.close_session(Parameters(sel("feature"))).await);
    assert!(err.message.contains("last open session"), "{}", err.message);
}

#[tokio::test]
async fn open_session_binds_to_a_branchs_own_worktree() {
    // `feat` is checked out in a linked worktree — the case plain `open_branch`
    // refuses. open_session lets git's branch→worktree mapping pick the anchor, so
    // it opens worktree-bound at that worktree (a live working copy), not off-worktree.
    let (_tmp, main, wt) = repo_with_worktree();
    let server = open_server(&main);

    let opened = server
        .open_session(Parameters(OpenSessionReq {
            branch: "feat".into(),
        }))
        .await
        .expect("open feat session")
        .0;
    assert_eq!(opened.session, "feat");
    assert!(
        opened.worktree_bound,
        "feat is checked out in a worktree, so the session is worktree-bound"
    );

    // The session is anchored at the worktree, and its working-copy tools are
    // available (no off-worktree refusal) — here just a clean status.
    let info = server
        .list_sessions()
        .await
        .unwrap()
        .0
        .sessions
        .into_iter()
        .find(|s| s.session == "feat")
        .expect("feat is listed");
    assert_eq!(
        info.root,
        std::fs::canonicalize(&wt).unwrap().to_str().unwrap(),
        "the session is anchored at feat's worktree"
    );
    let status = server
        .working_copy_status(Parameters(sel("feat")))
        .await
        .expect("working-copy tools work on a worktree-bound session")
        .0;
    assert!(status.clean, "the fresh worktree is clean");
}

#[tokio::test]
async fn reloading_one_session_leaves_the_others_untouched() {
    let tmp = two_branch_repo();
    let dir = tmp.path();
    let server = open_server(dir);
    open_feature(&server).await;

    // Drop a commit in each session, populating each one's trash independently.
    let main_b = change_id_of(&server, "main", "B").await;
    server
        .drop_commit(Parameters(DropCommitReq {
            session: sel("main"),
            commit: main_b,
            keep_changes: false,
        }))
        .await
        .expect("drop B on main");
    let feat_c = change_id_of(&server, "feature", "C").await;
    server
        .drop_commit(Parameters(DropCommitReq {
            session: sel("feature"),
            commit: feat_c,
            keep_changes: false,
        }))
        .await
        .expect("drop C on feature");

    // Reload only `main` — its trash and op-log reset...
    server
        .reload_repo(Parameters(ReloadRepoReq {
            session: sel("main"),
            path: None,
            branch: None,
        }))
        .await
        .expect("reload main");
    assert!(
        server
            .list_trash(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .commits
            .is_empty(),
        "main's trash was reset by its own reload"
    );

    // ...while `feature`'s session is wholly unaffected: its trash still holds C.
    let feat_trash: Vec<String> = server
        .list_trash(Parameters(sel("feature")))
        .await
        .unwrap()
        .0
        .commits
        .into_iter()
        .map(|c| c.subject)
        .collect();
    assert_eq!(
        feat_trash,
        vec!["C".to_string()],
        "feature's trash survived a reload of a different session"
    );
}
