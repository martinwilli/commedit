//! Response diet: show_commit caps a large file's diff and can restrict to
//! specific paths.

mod common;

use commedit_mcp::dto::ShowCommitReq;
use common::{git, init_repo, open_server, sel};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

#[tokio::test]
async fn show_commit_caps_a_large_diff_and_filters_paths() {
    let dir = TempDir::new().unwrap();
    // A huge added file (well over the per-file cap) plus a small one, in one
    // commit on top of an initial commit.
    let big: String = (0..2000).map(|i| format!("line {i}\n")).collect();
    init_repo(dir.path(), &[("seed.txt", "seed\n", "first")]);
    std::fs::write(dir.path().join("big.txt"), &big).unwrap();
    std::fs::write(dir.path().join("small.txt"), "small\n").unwrap();
    git(dir.path(), &["add", "big.txt", "small.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "add files"]);

    let server = open_server(dir.path());

    // Full show_commit of the tip: the big file's diff is truncated, the small
    // one isn't.
    let head = git(dir.path(), &["rev-parse", "HEAD"]);
    let full = server
        .show_commit(Parameters(ShowCommitReq {
            session: sel("main"),
            commit: head.clone(),
            paths: None,
            include_contents: None,
        }))
        .await
        .unwrap()
        .0;
    let big_file = full.files.iter().find(|f| f.path == "big.txt").unwrap();
    assert!(big_file.truncated, "the huge file's diff is capped");
    assert!(big_file.total_lines > 500, "reports the full line count");
    assert!(
        big_file.diff.as_deref().unwrap().lines().count() <= 500,
        "the emitted diff is capped"
    );
    let small_file = full.files.iter().find(|f| f.path == "small.txt").unwrap();
    assert!(!small_file.truncated);
    assert_eq!(small_file.total_lines, 0);

    // The paths filter returns only the named file.
    let only = server
        .show_commit(Parameters(ShowCommitReq {
            session: sel("main"),
            commit: head,
            paths: Some(vec!["small.txt".into()]),
            include_contents: None,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(only.files.len(), 1);
    assert_eq!(only.files[0].path, "small.txt");
}
