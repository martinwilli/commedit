//! The MCP session catches up to a git HEAD the caller moved out of band (a
//! plain `git commit` on top of HEAD) on the next tool call, so reads and
//! mutations keep working WITHOUT `reload_repo` — and, unlike reload, the catch-up
//! preserves the session trash and op-log.

mod common;

use commedit_mcp::dto::{DropCommitReq, ListHistoryReq};
use commedit_mcp::server::CommeditServer;
use common::{git, init_repo, open_server, sel};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

async fn subjects(server: &CommeditServer) -> Vec<String> {
    server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .expect("list_history should catch up to the out-of-band commit, not error")
        .0
        .commits
        .iter()
        .map(|c| c.subject.clone())
        .collect()
}

#[tokio::test]
async fn list_history_catches_up_to_an_out_of_band_commit() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_repo(dir, &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B")]);
    let server = open_server(dir);

    // The caller crystallizes a unit with plain git, on top of HEAD — the server
    // imported git state only at open, so it has not seen this commit.
    std::fs::write(dir.join("c.txt"), "c\n").unwrap();
    git(dir, &["add", "c.txt"]);
    git(dir, &["commit", "-q", "-m", "C"]);

    assert_eq!(
        subjects(&server).await,
        vec!["C", "B", "A"],
        "the out-of-band commit is visible without a reload"
    );
}

#[tokio::test]
async fn the_catch_up_preserves_the_session_trash() {
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
    let server = open_server(dir);

    // Drop B into the session trash, then commit out of band on top of the new tip.
    let b = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
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
        .find(|c| c.subject == "B")
        .expect("B present")
        .change_id;
    server
        .drop_commit(Parameters(DropCommitReq {
            session: sel("main"),
            commit: b,
            keep_changes: false,
        }))
        .await
        .expect("drop B");

    std::fs::write(dir.join("d.txt"), "d\n").unwrap();
    git(dir, &["add", "d.txt"]);
    git(dir, &["commit", "-q", "-m", "D"]);

    // The next read catches up to D...
    assert_eq!(
        subjects(&server).await,
        vec!["D", "C", "A"],
        "history reflects the dropped B and the out-of-band D"
    );
    // ...and the trash still holds B — a full reload would have cleared it.
    let trash = server
        .list_trash(Parameters(sel("main")))
        .await
        .expect("list_trash")
        .0;
    let trashed: Vec<String> = trash.commits.iter().map(|c| c.subject.clone()).collect();
    assert_eq!(trashed, vec!["B"], "the catch-up preserved the trash");
}
