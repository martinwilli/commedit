//! End-to-end tests for the commit-construction tools (create_commit,
//! revert_commit, commit_working_copy) and file deletion, asserting the
//! resulting plain-git state.

mod common;

use commedit_mcp::dto::{
    CommitWorkingCopyReq, ConflictFileEditDto, CreateCommitReq, FileContentDto, IdentityFieldsDto,
    ListHistoryReq, ReplaceFilesReq, ResolveConflictsReq, RevertCommitReq, SaveResultDto,
};
use commedit_mcp::server::CommeditServer;
use common::{expect_err, git, git_log_subjects, init_repo, open_server};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

/// Unwrap a clean save, returning the new head sha.
fn clean_head(result: &SaveResultDto) -> String {
    match result {
        SaveResultDto::Clean { head_sha } => head_sha.clone().expect("clean save has a head"),
        SaveResultDto::Conflicts { commits, .. } => {
            panic!("expected a clean save, got conflicts in {commits:?}")
        }
    }
}

/// A `CreateCommitReq` with everything but the named fields defaulted.
fn create(message: &str, files: &[(&str, &str)], new_parent: Option<&str>) -> CreateCommitReq {
    CreateCommitReq {
        message: message.into(),
        files: files
            .iter()
            .map(|(p, c)| FileContentDto {
                path: (*p).into(),
                content: (*c).into(),
            })
            .collect(),
        delete_paths: None,
        new_parent: new_parent.map(str::to_string),
        child: None,
        identity: IdentityFieldsDto::default(),
    }
}

async fn history(server: &CommeditServer) -> commedit_mcp::dto::ListHistoryResp {
    server
        .list_history(Parameters(ListHistoryReq { limit: None, brief: None }))
        .await
        .unwrap()
        .0
}

#[tokio::test]
async fn create_commit_on_top_of_head_adds_a_new_tip() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let server = open_server(dir.path());

    let result = server
        .create_commit(Parameters(create("third", &[("c.txt", "three\n")], None)))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["third", "second", "first"]);
    assert_eq!(git(dir.path(), &["show", "HEAD:c.txt"]), "three");
    // The tree is clean — create_commit synthesizes content, it doesn't touch disk.
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    assert!(server.working_copy_status().await.unwrap().0.clean);
}

#[tokio::test]
async fn create_commit_inserts_under_a_commit_and_rebases_descendants() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let server = open_server(dir.path());

    // new_parent = "first" splices the new commit between first and second.
    let first = history(&server).await.commits[1].change_id.clone();
    let result = server
        .create_commit(Parameters(create(
            "inserted",
            &[("c.txt", "ins\n")],
            Some(&first),
        )))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(
        git_log_subjects(dir.path()),
        ["second", "inserted", "first"]
    );
    // The inserted commit adds c.txt; second rebased on top still adds b.txt, so
    // both files are present at the tip.
    assert_eq!(git(dir.path(), &["show", "HEAD:c.txt"]), "ins");
    assert_eq!(git(dir.path(), &["show", "HEAD:b.txt"]), "two");
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn create_commit_with_no_files_makes_an_empty_commit() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "one\n", "first")]);
    let server = open_server(dir.path());

    let result = server
        .create_commit(Parameters(create("checkpoint", &[], None)))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["checkpoint", "first"]);
    // No tree change between the new tip and its parent.
    assert_eq!(
        git(dir.path(), &["diff", "--name-only", "HEAD~1", "HEAD"]),
        ""
    );
}

#[tokio::test]
async fn create_commit_at_root_becomes_the_first_commit() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let server = open_server(dir.path());

    let result = server
        .create_commit(Parameters(create(
            "root-commit",
            &[("r.txt", "r\n")],
            Some("root"),
        )))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(
        git_log_subjects(dir.path()),
        ["second", "first", "root-commit"]
    );
    // The new commit is the repository's first commit (no parents) and its file
    // is carried all the way up to the tip.
    let hist = history(&server).await;
    let root = hist.commits.last().unwrap();
    assert_eq!(root.subject, "root-commit");
    assert!(root.detail.as_ref().unwrap().parent_shas.is_empty());
    assert_eq!(git(dir.path(), &["show", "HEAD:r.txt"]), "r");
}

#[tokio::test]
async fn create_commit_preserves_uncommitted_changes() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let server = open_server(dir.path());

    // Dirty a tracked file, then create a commit on top of HEAD.
    std::fs::write(dir.path().join("a.txt"), "one\nlocal\n").unwrap();
    let result = server
        .create_commit(Parameters(create("third", &[("c.txt", "three\n")], None)))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["third", "second", "first"]);
    // The new commit sits *beneath* the uncommitted change: it doesn't capture
    // the dirty a.txt (HEAD still has the committed content)…
    assert_eq!(git(dir.path(), &["show", "HEAD:a.txt"]), "one");
    // …and the uncommitted edit is still there, on disk and in the working copy.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "one\nlocal\n"
    );
    let status = server.working_copy_status().await.unwrap().0;
    assert!(!status.clean);
    assert_eq!(status.entries[0].files, vec!["a.txt".to_string()]);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "M a.txt");
}

#[tokio::test]
async fn create_commit_with_an_explicit_author() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "one\n", "first")]);
    let server = open_server(dir.path());

    let req = CreateCommitReq {
        message: "authored".into(),
        files: vec![FileContentDto {
            path: "c.txt".into(),
            content: "c\n".into(),
        }],
        delete_paths: None,
        new_parent: None,
        child: None,
        identity: IdentityFieldsDto {
            author_name: Some("Alice".into()),
            author_email: Some("alice@example.com".into()),
            ..Default::default()
        },
    };
    let result = server.create_commit(Parameters(req)).await.unwrap().0;
    clean_head(&result);

    assert_eq!(
        git(dir.path(), &["show", "-s", "--format=%an", "HEAD"]),
        "Alice"
    );
    assert_eq!(
        git(dir.path(), &["show", "-s", "--format=%ae", "HEAD"]),
        "alice@example.com"
    );
}

#[tokio::test]
async fn create_commit_can_delete_relative_to_its_parent() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let server = open_server(dir.path());

    let req = CreateCommitReq {
        message: "drop a".into(),
        files: vec![],
        delete_paths: Some(vec!["a.txt".into()]),
        new_parent: None,
        child: None,
        identity: IdentityFieldsDto::default(),
    };
    let result = server.create_commit(Parameters(req)).await.unwrap().0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["drop a", "second", "first"]);
    // a.txt is removed at the tip, both in the tree and on disk.
    assert_eq!(
        git(dir.path(), &["ls-tree", "-r", "--name-only", "HEAD"]),
        "b.txt"
    );
    assert!(!dir.path().join("a.txt").exists());
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn revert_commit_inverts_a_commits_change() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[
            ("a.txt", "one\n", "first"),
            ("a.txt", "one\ntwo\n", "second"),
        ],
    );
    let server = open_server(dir.path());

    let second = history(&server).await.commits[0].change_id.clone();
    let result = server
        .revert_commit(Parameters(RevertCommitReq {
            commit: second,
            new_parent: None,
            child: None,
            identity: IdentityFieldsDto::default(),
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    let subjects = git_log_subjects(dir.path());
    assert_eq!(subjects[0], "Revert \"second\"");
    assert_eq!(&subjects[1..], ["second", "first"]);
    // The revert undoes second's change: a.txt is back to first's content.
    assert_eq!(git(dir.path(), &["show", "HEAD:a.txt"]), "one");
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn a_modify_delete_conflict_resolves_by_deleting_the_file() {
    let dir = TempDir::new().unwrap();
    // x.txt is added, then modified — so reverting its addition wants to delete a
    // file whose content has since diverged: a modify/delete conflict.
    init_repo(
        dir.path(),
        &[("x.txt", "foo\n", "add x"), ("x.txt", "foo\nbar\n", "modify x")],
    );
    let server = open_server(dir.path());

    let add_x = history(&server).await.commits[1].change_id.clone();
    let result = server
        .revert_commit(Parameters(RevertCommitReq {
            commit: add_x,
            new_parent: None,
            child: None,
            identity: IdentityFieldsDto::default(),
        }))
        .await
        .unwrap()
        .0;
    let SaveResultDto::Conflicts { commits, .. } = result else {
        panic!("the revert should conflict (modify vs delete)");
    };

    // Resolve by deleting the path — no read_conflict / marker_len needed, which
    // content resolution could not express (it would leave an empty file).
    let oldest = &commits[0];
    let result = server
        .resolve_conflicts(Parameters(ResolveConflictsReq {
            commit: oldest.change_id.clone(),
            files: vec![ConflictFileEditDto {
                path: oldest.files[0].path.clone(),
                text: None,
                marker_len: None,
                delete: Some(true),
            }],
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }), "delete settles the conflict");

    // The file is gone — not present at HEAD and not left as a 0-byte file.
    let tree = git(dir.path(), &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree.contains("x.txt"), "x.txt is removed from the tree: {tree}");
    assert!(!dir.path().join("x.txt").exists(), "x.txt is gone from disk");
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    git(dir.path(), &["fsck", "--no-progress"]);
}

#[tokio::test]
async fn revert_commit_refuses_a_merge() {
    let dir = TempDir::new().unwrap();
    common::init_merge_repo(dir.path());
    let server = open_server(dir.path());

    let merge = history(&server).await.commits[0].change_id.clone();
    let err = expect_err(
        server
            .revert_commit(Parameters(RevertCommitReq {
                commit: merge,
                new_parent: None,
                child: None,
                identity: IdentityFieldsDto::default(),
            }))
            .await,
    );
    assert!(
        err.message.contains("merge"),
        "unexpected error: {}",
        err.message
    );
    // A refused input is reported as invalid params, not an internal error.
    assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn commit_working_copy_commits_the_dirt() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")],
    );
    let server = open_server(dir.path());

    // A clean tree has nothing to commit.
    let err = expect_err(
        server
            .commit_working_copy(Parameters(CommitWorkingCopyReq {
                message: "nope".into(),
                identity: IdentityFieldsDto::default(),
            }))
            .await,
    );
    assert!(
        err.message.contains("clean"),
        "unexpected error: {}",
        err.message
    );

    // Dirty a tracked file, then crystallize it into a commit.
    std::fs::write(dir.path().join("a.txt"), "1\nlocal\n").unwrap();
    let result = server
        .commit_working_copy(Parameters(CommitWorkingCopyReq {
            message: "local work".into(),
            identity: IdentityFieldsDto::default(),
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(
        git_log_subjects(dir.path()),
        ["local work", "second", "first"]
    );
    assert_eq!(git(dir.path(), &["show", "HEAD:a.txt"]), "1\nlocal");
    // The working tree ends up clean.
    assert!(server.working_copy_status().await.unwrap().0.clean);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn replace_files_can_delete_a_file() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let server = open_server(dir.path());

    // Delete a.txt from the tip commit (it was added by "first", carried into
    // "second"), keeping the message and the rest of the tree.
    let second = history(&server).await.commits[0].change_id.clone();
    let result = server
        .replace_files(Parameters(ReplaceFilesReq {
            commit: second,
            files: vec![],
            delete_paths: Some(vec!["a.txt".into()]),
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["second", "first"]);
    assert_eq!(
        git(dir.path(), &["ls-tree", "-r", "--name-only", "HEAD"]),
        "b.txt"
    );
    assert!(!dir.path().join("a.txt").exists());
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}
