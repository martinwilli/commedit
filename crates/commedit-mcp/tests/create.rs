//! End-to-end tests for the commit-construction tools (create_commit,
//! revert_commit, commit_working_copy) and file deletion, asserting the
//! resulting plain-git state.

mod common;

use commedit_mcp::dto::{
    CherryPickCommitReq, CommitWorkingCopyReq, ConflictFileEditDto, CreateCommitReq,
    FileContentDto, IdentityFieldsDto, ListHistoryReq, ReadConflictReq, ReplaceFilesReq,
    ResolveConflictsReq, RevertCommitReq, SaveResultDto,
};
use commedit_mcp::server::CommeditServer;
use common::{expect_err, git, git_log_subjects, init_repo, open_server, sel};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

/// Unwrap a clean save, returning the new head sha.
fn clean_head(result: &SaveResultDto) -> String {
    match result {
        SaveResultDto::Clean { head_sha, .. } => head_sha.clone().expect("clean save has a head"),
        SaveResultDto::Conflicts { commits, .. } => {
            panic!("expected a clean save, got conflicts in {commits:?}")
        }
    }
}

/// A `CreateCommitReq` with everything but the named fields defaulted.
fn create(message: &str, files: &[(&str, &str)], new_parent: Option<&str>) -> CreateCommitReq {
    CreateCommitReq {
        session: sel("main"),
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

/// A `CherryPickCommitReq` with everything but the named fields defaulted.
fn pick(commit: &str, new_parent: Option<&str>) -> CherryPickCommitReq {
    CherryPickCommitReq {
        session: sel("main"),
        commit: commit.into(),
        new_parent: new_parent.map(str::to_string),
        child: None,
        identity: IdentityFieldsDto::default(),
    }
}

async fn history(server: &CommeditServer) -> commedit_mcp::dto::ListHistoryResp {
    server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
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
    assert!(
        server
            .working_copy_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .clean
    );
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
    assert!(root.detail.parent_shas.as_ref().unwrap().is_empty());
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
    let status = server
        .working_copy_status(Parameters(sel("main")))
        .await
        .unwrap()
        .0;
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
        session: sel("main"),
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
        session: sel("main"),
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
            session: sel("main"),
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
        &[
            ("x.txt", "foo\n", "add x"),
            ("x.txt", "foo\nbar\n", "modify x"),
        ],
    );
    let server = open_server(dir.path());

    let add_x = history(&server).await.commits[1].change_id.clone();
    let result = server
        .revert_commit(Parameters(RevertCommitReq {
            session: sel("main"),
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
            session: sel("main"),
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
    assert!(
        matches!(result, SaveResultDto::Clean { .. }),
        "delete settles the conflict"
    );

    // The file is gone — not present at HEAD and not left as a 0-byte file.
    let tree = git(dir.path(), &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        !tree.contains("x.txt"),
        "x.txt is removed from the tree: {tree}"
    );
    assert!(
        !dir.path().join("x.txt").exists(),
        "x.txt is gone from disk"
    );
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
                session: sel("main"),
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
                session: sel("main"),
                message: "nope".into(),
                identity: IdentityFieldsDto::default(),
                paths: None,
                hunks: None,
                patches: None,
                add_paths: None,
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
            session: sel("main"),
            message: "local work".into(),
            identity: IdentityFieldsDto::default(),
            paths: None,
            hunks: None,
            patches: None,
            add_paths: None,
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result.result);

    assert_eq!(
        git_log_subjects(dir.path()),
        ["local work", "second", "first"]
    );
    assert_eq!(git(dir.path(), &["show", "HEAD:a.txt"]), "1\nlocal");
    // The working tree ends up clean.
    assert!(
        server
            .working_copy_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .clean
    );
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn cherry_pick_copies_a_commit_from_another_branch() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "one\n", "first")]);
    // A sibling branch carries a commit main never saw.
    git(dir.path(), &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.path().join("f.txt"), "feat\n").unwrap();
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "feature work"]);
    let picked = git(dir.path(), &["rev-parse", "HEAD"]);
    git(dir.path(), &["checkout", "-q", "main"]);

    let server = open_server(dir.path());
    // The session imports only main: the feature commit is off-history.
    assert_eq!(history(&server).await.commits.len(), 1);

    let result = server
        .cherry_pick_commit(Parameters(pick(&picked, None)))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    // The change is replayed onto main, keeping the source subject and author.
    assert_eq!(git_log_subjects(dir.path()), ["feature work", "first"]);
    assert_eq!(git(dir.path(), &["show", "HEAD:f.txt"]), "feat");
    assert_eq!(
        git(dir.path(), &["show", "-s", "--format=%an", "HEAD"]),
        "Tester"
    );
    // The provenance trailer records the source (git `cherry-pick -x` style).
    let body = git(dir.path(), &["show", "-s", "--format=%b", "HEAD"]);
    assert!(
        body.contains(&format!("cherry picked from commit {picked}")),
        "missing provenance trailer: {body}"
    );
    // The source branch is left exactly where it was — only main moved.
    assert_eq!(git(dir.path(), &["rev-parse", "feature"]), picked);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    git(dir.path(), &["fsck", "--no-progress"]);
}

#[tokio::test]
async fn cherry_pick_resolves_an_in_history_change_id_and_places_it() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "one\n", "add a"), ("b.txt", "two\n", "add b")],
    );
    let server = open_server(dir.path());

    // Pick "add b" (which adds b.txt) by change id and drop a copy at the root.
    let add_b = history(&server).await.commits[0].change_id.clone();
    let result = server
        .cherry_pick_commit(Parameters(pick(&add_b, Some("root"))))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    // The copy becomes the repository's first commit, with b.txt in its tree.
    let hist = history(&server).await;
    let root = hist.commits.last().unwrap();
    assert_eq!(root.subject, "add b");
    assert!(root.detail.parent_shas.as_ref().unwrap().is_empty());
    assert_eq!(
        git(dir.path(), &["show", &format!("{}:b.txt", root.sha)]),
        "two"
    );
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn cherry_pick_refuses_a_merge() {
    let dir = TempDir::new().unwrap();
    common::init_merge_repo(dir.path());
    let server = open_server(dir.path());

    let merge = history(&server).await.commits[0].change_id.clone();
    let err = expect_err(
        server
            .cherry_pick_commit(Parameters(pick(&merge, None)))
            .await,
    );
    assert!(
        err.message.contains("merge"),
        "unexpected error: {}",
        err.message
    );
}

#[tokio::test]
async fn a_cherry_pick_that_overlaps_conflicts_and_resolves() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("x.txt", "base\n", "first")]);
    // feature edits x.txt one way…
    git(dir.path(), &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.path().join("x.txt"), "feature\n").unwrap();
    git(dir.path(), &["add", "x.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "feature edit"]);
    let picked = git(dir.path(), &["rev-parse", "HEAD"]);
    // …main edits the same line another way, so the pick can't apply cleanly.
    git(dir.path(), &["checkout", "-q", "main"]);
    std::fs::write(dir.path().join("x.txt"), "mainline\n").unwrap();
    git(dir.path(), &["add", "x.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "main edit"]);
    let head_before = git(dir.path(), &["rev-parse", "HEAD"]);

    let server = open_server(dir.path());
    let result = server
        .cherry_pick_commit(Parameters(pick(&picked, None)))
        .await
        .unwrap()
        .0;
    let SaveResultDto::Conflicts { commits, .. } = result else {
        panic!("the overlapping pick should conflict");
    };

    // Held back in full — git history and the tree are untouched.
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_log_subjects(dir.path()), ["main edit", "first"]);

    // Resolving to the picked content settles it and exports.
    let oldest = &commits[0];
    let resp = server
        .read_conflict(Parameters(ReadConflictReq {
            session: sel("main"),
            commit: oldest.change_id.clone(),
            path: Some(oldest.files[0].path.clone()),
            paths: None,
        }))
        .await
        .unwrap()
        .0;
    let file = &resp.files[0];
    let result = server
        .resolve_conflicts(Parameters(ResolveConflictsReq {
            session: sel("main"),
            commit: oldest.change_id.clone(),
            files: vec![ConflictFileEditDto {
                path: oldest.files[0].path.clone(),
                text: Some("feature\n".into()),
                marker_len: Some(file.marker_len),
                delete: None,
            }],
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));
    assert_eq!(git(dir.path(), &["show", "HEAD:x.txt"]), "feature");
    assert_eq!(
        git_log_subjects(dir.path()),
        ["feature edit", "main edit", "first"]
    );
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    git(dir.path(), &["fsck", "--no-progress"]);
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
            session: sel("main"),
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
