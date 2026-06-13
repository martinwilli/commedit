//! End-to-end tests for the read-only `show_graph` query: the branch's commit
//! graph by change_id, including a merge's two parents and a fork's children.

mod common;

use commedit_mcp::dto::ShowGraphResp;
use commedit_mcp::server::CommeditServer;
use common::{init_merge_repo, init_repo, open_server};
use tempfile::TempDir;

async fn graph(server: &CommeditServer) -> ShowGraphResp {
    server.show_graph().await.expect("show_graph call").0
}

#[tokio::test]
async fn show_graph_links_a_linear_branch_by_change_id() {
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

    let g = graph(&server).await;
    assert!(g.head_sha.is_some());
    assert_eq!(g.commits.len(), 3, "every ancestor of HEAD appears");

    // Newest first: third -> second -> first.
    let (third, second, first) = (&g.commits[0], &g.commits[1], &g.commits[2]);
    assert_eq!(
        (
            third.subject.as_str(),
            second.subject.as_str(),
            first.subject.as_str()
        ),
        ("third", "second", "first")
    );

    // The tip has no children; the root no parents.
    assert!(third.children.is_empty(), "the tip has no children");
    assert!(first.parents.is_empty(), "the root has no parents");

    // The chain links by change_id, in both directions.
    assert_eq!(third.parents, vec![second.change_id.clone()]);
    assert_eq!(second.parents, vec![first.change_id.clone()]);
    assert_eq!(second.children, vec![third.change_id.clone()]);
    assert_eq!(first.children, vec![second.change_id.clone()]);
}

#[tokio::test]
async fn show_graph_exposes_a_merge_and_its_fork() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    init_merge_repo(dir);
    let server = open_server(dir);

    let g = graph(&server).await;
    let by = |s: &str| {
        g.commits
            .iter()
            .find(|c| c.subject == s)
            .unwrap_or_else(|| panic!("commit {s:?} not found"))
    };

    // The merge tip shows both parents (the shape a linear list can't convey).
    let merge = by("merge");
    assert_eq!(merge.parents.len(), 2, "the merge exposes both parents");
    assert!(merge.children.is_empty());

    // "base" is the fork point: the root with both lanes as children.
    let base = by("base");
    assert!(base.parents.is_empty(), "base is the root");
    assert_eq!(base.children.len(), 2, "the fork base has two children");

    // Both lanes converge back on the merge.
    assert_eq!(by("main-1").children, vec![merge.change_id.clone()]);
    assert_eq!(by("side-1").children, vec![merge.change_id.clone()]);
}
