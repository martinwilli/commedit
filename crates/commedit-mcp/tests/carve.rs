//! The `carve_working_copy` tool: split a dirty tree into several commits in one
//! call, leaving the unselected remainder uncommitted.

mod common;

use commedit_mcp::dto::{CarveCommitDto, CarveWorkingCopyReq, IdentityFieldsDto, SaveResultDto};
use common::{git, git_log_subjects, init_repo, open_server, sel};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

fn commit(message: &str, paths: &[&str]) -> CarveCommitDto {
    CarveCommitDto {
        message: message.into(),
        identity: IdentityFieldsDto::default(),
        paths: Some(paths.iter().map(|p| p.to_string()).collect()),
        hunks: None,
        patches: None,
    }
}

#[tokio::test]
async fn carve_splits_a_dirty_tree_in_one_call() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("base.txt", "base\n", "base")]);
    let server = open_server(dir.path());

    std::fs::write(dir.path().join("base.txt"), "base\nedit\n").unwrap();
    std::fs::write(dir.path().join("feat.txt"), "feature\n").unwrap();
    std::fs::write(dir.path().join("doc.txt"), "docs\n").unwrap();

    let resp = server
        .carve_working_copy(Parameters(CarveWorkingCopyReq {
            session: sel("main"),
            commits: vec![
                commit("feat: add feature", &["feat.txt"]),
                commit("base: tweak base", &["base.txt"]),
            ],
            add_paths: Some(vec!["feat.txt".into(), "doc.txt".into()]),
        }))
        .await
        .unwrap()
        .0;

    assert!(matches!(resp.result, SaveResultDto::Clean { .. }));
    // Two commits created, oldest-first.
    assert_eq!(
        resp.committed
            .iter()
            .map(|c| c.subject.as_str())
            .collect::<Vec<_>>(),
        vec!["feat: add feature", "base: tweak base"]
    );
    assert_eq!(
        git_log_subjects(dir.path()),
        vec!["base: tweak base", "feat: add feature", "base"]
    );
    assert_eq!(git(dir.path(), &["show", "main~1:feat.txt"]), "feature");
    assert_eq!(git(dir.path(), &["show", "main:base.txt"]), "base\nedit");
    // doc.txt was never selected → still an untracked remainder.
    assert!(resp.working_copy.is_some_and(|wc| !wc.clean));
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "?? doc.txt");
}
