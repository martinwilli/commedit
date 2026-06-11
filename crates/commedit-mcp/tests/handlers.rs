//! End-to-end tool handler tests against scratch git repos, asserting both the
//! responses and (for mutations) the resulting plain-git state.

mod common;

use common::{expect_err, git, git_log_subjects, init_merge_repo, init_repo, open_server};
use commedit_mcp::dto::{
    CommitEditDto, CommitField, DropCommitReq, EditCommitsReq, EditIdentityReq, EditMessageReq,
    FileContentDto, IdentityFieldsDto, ListHistoryReq, ReorderCommitReq, ReplaceFilesReq,
    ReplaceInFileReq, ReplaceInMessageReq, RestoreCommitReq, SaveResultDto, ShowCommitReq,
    SplitCommitReq, SquashCommitReq, StrReplaceDto,
};
use commedit_mcp::server::CommeditServer;
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

/// The current history's shas, newest first.
async fn shas(server: &CommeditServer) -> Vec<String> {
    server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0
        .commits
        .iter()
        .map(|c| c.sha.clone())
        .collect()
}

/// Unwrap a clean save, returning the new head sha.
fn clean_head(result: &SaveResultDto) -> String {
    match result {
        SaveResultDto::Clean { head_sha } => head_sha.clone().expect("clean save has a head"),
        SaveResultDto::Conflicts { commits, .. } => {
            panic!("expected a clean save, got conflicts in {commits:?}")
        }
    }
}

#[tokio::test]
async fn list_history_returns_the_branch_commits_with_refs() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[
            ("a.txt", "one\n", "first"),
            ("b.txt", "two\n", "second"),
            ("c.txt", "three\n", "third"),
        ],
    );
    let server = open_server(dir.path());

    let resp = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let subjects: Vec<&str> = resp.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["third", "second", "first"]);
    // Emitted shas are abbreviated (>= 8 chars); head_sha stays full, so the
    // tip's abbreviated sha is a prefix of it.
    let head = resp.head_sha.as_deref().unwrap();
    let tip = resp.commits[0].sha.as_str();
    assert!(tip.len() >= 8 && head.starts_with(tip), "tip {tip} prefixes head {head}");
    assert!(!resp.has_more);
    assert_eq!(resp.next_offset, None);
    assert_eq!(resp.offset, 0);
    assert_eq!(resp.trash_count, 0);

    // The tip carries the checked-out branch decoration.
    let tip_refs = &resp.commits[0].refs;
    assert!(tip_refs.iter().any(|r| r.name == "main" && r.kind == "branch" && r.current));
    // The oldest commit has no parents (the virtual root is filtered).
    assert!(resp.commits[2].detail.parent_shas.as_ref().unwrap().is_empty());
    assert_eq!(
        resp.commits[0].detail.parent_shas.clone().unwrap(),
        vec![resp.commits[1].sha.clone()]
    );
}

#[tokio::test]
async fn list_history_honours_the_limit() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("a.txt", "2\n", "second"), ("a.txt", "3\n", "third")],
    );
    let server = open_server(dir.path());

    let resp = server
        .list_history(Parameters(ListHistoryReq { limit: Some(2), offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    assert_eq!(resp.commits.len(), 2);
    assert!(resp.has_more);
    assert_eq!(resp.commits[0].subject, "third");
}

#[tokio::test]
async fn list_history_fields_selects_the_verbose_detail() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first\n\nwith a long body line"), ("b.txt", "2\n", "second")],
    );
    let server = open_server(dir.path());

    // `fields: []` keeps only the header — every verbose field is omitted.
    let header = server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: Some(vec![]),
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(header.commits[0].subject, "second");
    assert!(!header.commits[0].sha.is_empty());
    assert!(!header.commits[0].change_id.is_empty());
    assert!(header.commits.iter().all(|c| {
        let d = &c.detail;
        d.description.is_none()
            && d.author_time.is_none()
            && d.committer_time.is_none()
            && d.parent_shas.is_none()
    }));

    // An explicit subset includes exactly those fields and nothing else.
    let subset = server
        .list_history(Parameters(ListHistoryReq {
            limit: None,
            offset: None,
            fields: Some(vec![CommitField::AuthorTime, CommitField::CommitterTime]),
        }))
        .await
        .unwrap()
        .0;
    let d = &subset.commits[1].detail;
    assert!(d.author_time.is_some() && d.committer_time.is_some());
    assert!(d.description.is_none() && d.author_name.is_none() && d.parent_shas.is_none());

    // Omitting `fields` carries the full detail, including the message body.
    let full = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let description = full.commits[1].detail.description.as_ref().expect("full listing has detail");
    assert!(
        description.contains("with a long body line"),
        "full detail carries the message body: {description}"
    );
}

#[tokio::test]
async fn list_history_marks_merges() {
    let dir = TempDir::new().unwrap();
    init_merge_repo(dir.path());
    let server = open_server(dir.path());

    let resp = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let merge = &resp.commits[0];
    assert_eq!(merge.subject, "merge");
    assert!(merge.is_merge);
    assert_eq!(merge.detail.parent_shas.as_ref().unwrap().len(), 2);
    assert!(resp.commits[1..].iter().all(|c| !c.is_merge));
}

#[tokio::test]
async fn show_commit_renders_diffs_and_optionally_contents() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "one\n", "first"), ("a.txt", "one\ntwo\n", "second")],
    );
    let server = open_server(dir.path());

    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let sha = history.commits[0].sha.clone();

    let resp = server
        .show_commit(Parameters(ShowCommitReq { commit: sha.clone(), include_contents: None }))
        .await
        .unwrap()
        .0;
    assert_eq!(resp.commit.subject, "second");
    assert_eq!(resp.files.len(), 1);
    let file = &resp.files[0];
    assert_eq!(file.path, "a.txt");
    assert_eq!(file.kind, "modified");
    assert!(file.diff.as_deref().unwrap().contains("+two"));
    assert!(file.old_text.is_none() && file.new_text.is_none());

    let with = server
        .show_commit(Parameters(ShowCommitReq { commit: sha, include_contents: Some(true) }))
        .await
        .unwrap()
        .0;
    assert_eq!(with.files[0].old_text.as_deref(), Some("one\n"));
    assert_eq!(with.files[0].new_text.as_deref(), Some("one\ntwo\n"));
}

#[tokio::test]
async fn show_commit_rejects_an_unknown_ref() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "one\n", "first")]);
    let server = open_server(dir.path());

    let err = expect_err(
        server
            .show_commit(Parameters(ShowCommitReq {
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                include_contents: None,
            }))
            .await,
    );
    assert!(err.message.contains("not found"), "unexpected error: {}", err.message);
}

#[tokio::test]
async fn list_trash_starts_empty() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "one\n", "first")]);
    let server = open_server(dir.path());

    let resp = server.list_trash().await.unwrap().0;
    assert!(resp.commits.is_empty());
}

#[tokio::test]
async fn working_copy_status_reflects_dirty_tracked_files() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "one\n", "first")]);
    let server = open_server(dir.path());

    let clean = server.working_copy_status().await.unwrap().0;
    assert!(clean.clean);
    assert!(clean.entries.is_empty());
    assert!(clean.session_start_head_sha.is_some());

    std::fs::write(dir.path().join("a.txt"), "edited\n").unwrap();
    let dirty = server.working_copy_status().await.unwrap().0;
    assert!(!dirty.clean);
    assert_eq!(dirty.entries.len(), 1);
    assert_eq!(dirty.entries[0].files, vec!["a.txt".to_string()]);
    assert!(!dirty.entries[0].has_conflict);

    // The entry's sha reads as a commit: its diff is the uncommitted change.
    let shown = server
        .show_commit(Parameters(ShowCommitReq {
            commit: dirty.entries[0].sha.clone(),
            include_contents: None,
        }))
        .await
        .unwrap()
        .0;
    assert!(shown.files[0].diff.as_deref().unwrap().contains("+edited"));
}

#[tokio::test]
async fn session_diff_and_operations_start_empty() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "one\n", "first")]);
    let server = open_server(dir.path());

    let diff = server.session_diff().await.unwrap().0;
    assert!(diff.files.is_empty());

    let ops = server.list_operations().await.unwrap().0;
    assert!(ops.ops.is_empty());
    assert_eq!(ops.cursor, 0);
    assert!(!ops.can_undo && !ops.can_redo && !ops.pending);

    let pending = server.pending_status().await.unwrap().0;
    assert!(!pending.pending);
    assert!(pending.conflicts.is_empty());
    assert_eq!(pending.git_head_sha, pending.jj_head_sha);

    // An untouched session shows a clean git status.
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn edit_message_rewrites_any_commit_and_exports_to_git() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second"), ("c.txt", "3\n", "third")],
    );
    let server = open_server(dir.path());

    // Edit the middle commit, not just the tip.
    let target = shas(&server).await[1].clone();
    let result = server
        .edit_message(Parameters(EditMessageReq {
            commit: target,
            message: "second, edited\n\nwith a body".into(),
        }))
        .await
        .unwrap()
        .0;
    let head = clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["third", "second, edited", "first"]);
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    git(dir.path(), &["fsck", "--strict"]);
}

#[tokio::test]
async fn edit_identity_prefills_omitted_fields() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")]);
    let server = open_server(dir.path());

    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let target = &history.commits[0];
    let committer_time = target.detail.committer_time.clone().unwrap();

    let result = server
        .edit_identity(Parameters(EditIdentityReq {
            commit: target.sha.clone(),
            identity: IdentityFieldsDto {
                author_name: Some("New Author".into()),
                ..Default::default()
            },
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    // The author name changed; everything else was prefilled from the commit,
    // including the committer timestamp (not re-stamped to "now").
    let show = git(dir.path(), &["log", "-1", "--format=%an|%ae|%cn|%ce", "HEAD"]);
    assert_eq!(show, "New Author|tester@example.com|Tester|tester@example.com");
    let listed = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    assert_eq!(listed.commits[0].detail.committer_time.clone().unwrap(), committer_time);
}

#[tokio::test]
async fn edit_commits_batches_message_and_identity_in_one_pass() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second"), ("c.txt", "3\n", "third")],
    );
    let server = open_server(dir.path());

    // Address by the (abbreviated) change_ids the listing returns — proving they
    // round-trip back as refs.
    let hist = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let id = |i: usize| hist.commits[i].change_id.clone(); // [third, second, first]

    let dated = |commit: String, t: &str| CommitEditDto {
        commit,
        message: None,
        identity: IdentityFieldsDto {
            author_time: Some(t.into()),
            committer_time: Some(t.into()),
            ..Default::default()
        },
    };

    // One batch: re-date a parent ("first") and its child ("second"), and reword
    // the tip ("third") — all in a single transaction / rebase.
    let result = server
        .edit_commits(Parameters(EditCommitsReq {
            edits: vec![
                dated(id(2), "2026-06-11 18:00:00 +0200"),
                dated(id(1), "2026-06-11 18:30:00 +0200"),
                CommitEditDto {
                    commit: id(0),
                    message: Some("third (edited)".into()),
                    identity: IdentityFieldsDto::default(),
                },
            ],
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["third (edited)", "second", "first"]);
    let listed = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let detail = |i: usize| listed.commits[i].detail.clone();
    // The child's committer is the pinned value, not re-stamped to "now".
    assert_eq!(detail(1).author_time.unwrap(), "2026-06-11 18:30:00 +0200");
    assert_eq!(detail(1).committer_time.unwrap(), "2026-06-11 18:30:00 +0200");
    assert_eq!(detail(2).author_time.unwrap(), "2026-06-11 18:00:00 +0200");
    assert_eq!(detail(2).committer_time.unwrap(), "2026-06-11 18:00:00 +0200");
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn edit_commits_rejects_empty_and_noop_batches() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")]);
    let server = open_server(dir.path());

    let empty = expect_err(server.edit_commits(Parameters(EditCommitsReq { edits: vec![] })).await);
    assert!(empty.message.contains("must not be empty"), "{}", empty.message);

    let target = shas(&server).await[0].clone();
    let noop = expect_err(
        server
            .edit_commits(Parameters(EditCommitsReq {
                edits: vec![CommitEditDto {
                    commit: target,
                    message: None,
                    identity: IdentityFieldsDto::default(),
                }],
            }))
            .await,
    );
    assert!(noop.message.contains("changes nothing"), "{}", noop.message);
}

#[tokio::test]
async fn replace_files_rewrites_contents_across_descendants() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "one\n", "first"), ("b.txt", "two\n", "second")],
    );
    let server = open_server(dir.path());

    let target = shas(&server).await[1].clone();
    let result = server
        .replace_files(Parameters(ReplaceFilesReq {
            commit: target,
            files: vec![
                FileContentDto { path: "a.txt".into(), content: "ONE\n".into() },
                FileContentDto { path: "new.txt".into(), content: "added\n".into() },
            ],
            delete_paths: None,
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["second", "first"]);
    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "ONE");
    assert_eq!(git(dir.path(), &["show", "HEAD~1:new.txt"]), "added");
    // The descendant rebased onto the edited tree; the worktree follows.
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "ONE\n");
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn replace_files_requires_files() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first")]);
    let server = open_server(dir.path());

    let sha = shas(&server).await[0].clone();
    let err = expect_err(
        server
            .replace_files(Parameters(ReplaceFilesReq { commit: sha, files: vec![], delete_paths: None }))
            .await,
    );
    assert!(err.message.contains("files"), "unexpected error: {}", err.message);
}

#[tokio::test]
async fn replace_in_file_rewrites_a_unique_match_across_descendants() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "the bulck form\n", "first"), ("b.txt", "two\n", "second")],
    );
    let server = open_server(dir.path());

    let target = shas(&server).await[1].clone();
    let result = server
        .replace_in_file(Parameters(ReplaceInFileReq {
            commit: target,
            edits: vec![StrReplaceDto {
                path: "a.txt".into(),
                old: "bulck".into(),
                new: "bulk".into(),
                replace_all: None,
            }],
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["second", "first"]);
    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "the bulk form");
    // The descendant rebased onto the edited tree; the worktree follows.
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "the bulk form\n");
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn replace_in_file_rejects_an_ambiguous_match() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "a\na\n", "first")]);
    let server = open_server(dir.path());

    let target = shas(&server).await[0].clone();
    let err = expect_err(
        server
            .replace_in_file(Parameters(ReplaceInFileReq {
                commit: target,
                edits: vec![StrReplaceDto {
                    path: "a.txt".into(),
                    old: "a".into(),
                    new: "b".into(),
                    replace_all: None,
                }],
            }))
            .await,
    );
    // The ambiguity message comes only from the ReplaceError→invalid path.
    assert!(err.message.contains("matched 2 times"), "unexpected error: {}", err.message);
}

#[tokio::test]
async fn replace_in_message_fixes_a_typo() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "the bulck form"), ("b.txt", "2\n", "second")],
    );
    let server = open_server(dir.path());

    let target = shas(&server).await[1].clone();
    let result = server
        .replace_in_message(Parameters(ReplaceInMessageReq {
            commit: target,
            old: "bulck".into(),
            new: "bulk".into(),
            replace_all: None,
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["second", "the bulk form"]);
}

#[tokio::test]
async fn replace_in_message_rejects_a_missing_match() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first")]);
    let server = open_server(dir.path());

    let target = shas(&server).await[0].clone();
    let err = expect_err(
        server
            .replace_in_message(Parameters(ReplaceInMessageReq {
                commit: target,
                old: "nope".into(),
                new: "x".into(),
                replace_all: None,
            }))
            .await,
    );
    assert!(err.message.contains("not found"), "unexpected error: {}", err.message);
}

#[tokio::test]
async fn split_commit_peels_a_fixup_child_off_the_edited_commit() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[
            ("a.txt", "one\n", "first"),
            ("a.txt", "one\ntwo\nthree\n", "second"),
            ("b.txt", "x\n", "third"),
        ],
    );
    let server = open_server(dir.path());

    // Keep only part of "second"'s change; the rest moves to a fixup child.
    let target = shas(&server).await[1].clone();
    let result = server
        .split_commit(Parameters(SplitCommitReq {
            commit: target,
            files: vec![FileContentDto { path: "a.txt".into(), content: "one\ntwo\n".into() }],
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(
        git_log_subjects(dir.path()),
        ["third", "fixup! second", "second", "first"]
    );
    // The split halves combined reproduce the original content.
    assert_eq!(git(dir.path(), &["show", "HEAD~2:a.txt"]), "one\ntwo");
    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "one\ntwo\nthree");
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn reorder_moves_a_commit_under_a_new_parent() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second"), ("c.txt", "3\n", "third")],
    );
    let server = open_server(dir.path());

    // Move "third" below "second": its parent becomes "first".
    let shas = shas(&server).await;
    let result = server
        .reorder_commit(Parameters(ReorderCommitReq {
            commit: shas[0].clone(),
            new_parent: shas[2].clone(),
            child: None,
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["second", "third", "first"]);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn reorder_to_root_makes_a_commit_the_first() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second"), ("c.txt", "3\n", "third")],
    );
    let server = open_server(dir.path());

    let top = shas(&server).await[0].clone();
    let result = server
        .reorder_commit(Parameters(ReorderCommitReq {
            commit: top,
            new_parent: "root".into(),
            child: None,
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["second", "first", "third"]);
    // "third" really is the new root commit.
    let bottom = git(dir.path(), &["rev-list", "--max-parents=0", "HEAD"]);
    assert_eq!(git(dir.path(), &["log", "-1", "--format=%s", &bottom]), "third");
}

#[tokio::test]
async fn reorder_rejects_noop_self_and_merge_moves() {
    let dir = TempDir::new().unwrap();
    init_merge_repo(dir.path());
    let server = open_server(dir.path());

    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let merge = history.commits.iter().find(|c| c.is_merge).unwrap();
    let base = history.commits.iter().find(|c| c.subject == "base").unwrap();
    let main1 = history.commits.iter().find(|c| c.subject == "main-1").unwrap();

    let err = expect_err(
        server
            .reorder_commit(Parameters(ReorderCommitReq {
                commit: merge.sha.clone(),
                new_parent: base.sha.clone(),
                child: None,
            }))
            .await,
    );
    assert!(err.message.contains("merge"), "unexpected error: {}", err.message);

    let err = expect_err(
        server
            .reorder_commit(Parameters(ReorderCommitReq {
                commit: main1.sha.clone(),
                new_parent: main1.sha.clone(),
                child: None,
            }))
            .await,
    );
    assert!(err.message.contains("own parent"), "unexpected error: {}", err.message);

    let err = expect_err(
        server
            .reorder_commit(Parameters(ReorderCommitReq {
                commit: main1.sha.clone(),
                new_parent: base.sha.clone(),
                child: None,
            }))
            .await,
    );
    assert!(err.message.contains("already a child"), "unexpected error: {}", err.message);
}

#[tokio::test]
async fn an_ambiguous_fork_reorder_needs_child_sha() {
    let dir = TempDir::new().unwrap();
    init_merge_repo(dir.path());
    // A commit on top of the merge, to be moved down under "base".
    std::fs::write(dir.path().join("d.txt"), "top\n").unwrap();
    git(dir.path(), &["add", "d.txt"]);
    git(dir.path(), &["commit", "-qm", "top"]);
    let server = open_server(dir.path());

    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let top = history.commits.iter().find(|c| c.subject == "top").unwrap();
    let base = history.commits.iter().find(|c| c.subject == "base").unwrap();
    let main1 = history.commits.iter().find(|c| c.subject == "main-1").unwrap();

    // Two lines (main-1's and side-1's) converge on "base": ambiguous.
    let err = expect_err(
        server
            .reorder_commit(Parameters(ReorderCommitReq {
                commit: top.sha.clone(),
                new_parent: base.sha.clone(),
                child: None,
            }))
            .await,
    );
    assert!(err.message.contains("child to pick"), "unexpected error: {}", err.message);
    assert!(err.message.contains("main-1") && err.message.contains("side-1"));

    // Disambiguated: splice between base and main-1.
    let result = server
        .reorder_commit(Parameters(ReorderCommitReq {
            commit: top.sha.clone(),
            new_parent: base.sha.clone(),
            child: Some(main1.sha.clone()),
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    // "top" now sits between base and main-1 on the first-parent line.
    assert_eq!(
        git(dir.path(), &["log", "--first-parent", "--format=%s", "HEAD"]),
        "merge\nmain-1\ntop\nbase"
    );
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn drop_then_restore_round_trips_through_the_trash() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second"), ("c.txt", "3\n", "third")],
    );
    let server = open_server(dir.path());

    let target = shas(&server).await[1].clone();
    let resp = server
        .drop_commit(Parameters(DropCommitReq { commit: target.clone() }))
        .await
        .unwrap()
        .0;
    clean_head(&resp.result);
    assert_eq!(resp.dropped.subject, "second");
    assert_eq!(git_log_subjects(dir.path()), ["third", "first"]);

    // It sits in the trash, counted by list_history.
    let trash = server.list_trash().await.unwrap().0;
    assert_eq!(trash.commits.len(), 1);
    assert_eq!(trash.commits[0].sha, target);
    let listing = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    assert_eq!(listing.trash_count, 1);

    // Restore it where it came from: on top of "first".
    let first = shas(&server).await[1].clone();
    let result = server
        .restore_commit(Parameters(RestoreCommitReq {
            commit: target,
            new_parent: first,
            child: None,
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["third", "second", "first"]);
    assert!(server.list_trash().await.unwrap().0.commits.is_empty());
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    git(dir.path(), &["fsck", "--no-progress"]);
}

#[tokio::test]
async fn drop_refuses_merges_and_unknown_restores() {
    let dir = TempDir::new().unwrap();
    init_merge_repo(dir.path());
    let server = open_server(dir.path());

    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let merge = history.commits.iter().find(|c| c.is_merge).unwrap();
    let err = expect_err(
        server.drop_commit(Parameters(DropCommitReq { commit: merge.sha.clone() })).await,
    );
    assert!(err.message.contains("merge"), "unexpected error: {}", err.message);

    let err = expect_err(
        server
            .restore_commit(Parameters(RestoreCommitReq {
                commit: merge.sha.clone(),
                new_parent: "root".into(),
                child: None,
            }))
            .await,
    );
    assert!(err.message.contains("trash"), "unexpected error: {}", err.message);
}

/// A repo for squash tests: "target" introduces a.txt, "follow-up" edits it
/// (with a body in its message), "third" adds an unrelated file.
fn squash_repo(dir: &std::path::Path) {
    init_repo(
        dir,
        &[
            ("a.txt", "one\n", "target"),
            ("a.txt", "one\ntwo\n", "follow-up\n\nthe follow-up body"),
            ("c.txt", "3\n", "third"),
        ],
    );
}

async fn squash(
    server: &CommeditServer,
    source: &str,
    dest: &str,
    mode: Option<&str>,
) -> SaveResultDto {
    server
        .squash_commit(Parameters(SquashCommitReq {
            source: source.into(),
            dest: dest.into(),
            mode: mode.map(str::to_string),
        }))
        .await
        .unwrap()
        .0
}

#[tokio::test]
async fn squash_fixup_keeps_the_destinations_message() {
    let dir = TempDir::new().unwrap();
    squash_repo(dir.path());
    let server = open_server(dir.path());

    let shas = shas(&server).await;
    let result = squash(&server, &shas[1], &shas[2], None).await;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["third", "target"]);
    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "one\ntwo");
    assert_eq!(git(dir.path(), &["log", "-1", "--format=%B", "HEAD~1"]).trim(), "target");
}

#[tokio::test]
async fn squash_mode_squash_appends_the_sources_body() {
    let dir = TempDir::new().unwrap();
    squash_repo(dir.path());
    let server = open_server(dir.path());

    let shas = shas(&server).await;
    clean_head(&squash(&server, &shas[1], &shas[2], Some("squash")).await);

    let message = git(dir.path(), &["log", "-1", "--format=%B", "HEAD~1"]);
    assert_eq!(message.trim(), "target\n\nfollow-up\n\nthe follow-up body");
}

#[tokio::test]
async fn squash_mode_amend_replaces_the_destinations_message() {
    let dir = TempDir::new().unwrap();
    squash_repo(dir.path());
    let server = open_server(dir.path());

    let shas = shas(&server).await;
    clean_head(&squash(&server, &shas[1], &shas[2], Some("amend")).await);

    let message = git(dir.path(), &["log", "-1", "--format=%B", "HEAD~1"]);
    assert_eq!(message.trim(), "follow-up\n\nthe follow-up body");
}

#[tokio::test]
async fn squash_defaults_to_the_sources_subject_prefix() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[
            ("a.txt", "one\n", "target"),
            ("a.txt", "one\ntwo\n", "squash! target\n\nprefixed body"),
        ],
    );
    let server = open_server(dir.path());

    let shas = shas(&server).await;
    clean_head(&squash(&server, &shas[0], &shas[1], None).await);

    // The squash! prefix selected Squash mode; the prefix line is stripped.
    let message = git(dir.path(), &["log", "-1", "--format=%B", "HEAD"]);
    assert_eq!(message.trim(), "target\n\nprefixed body");
}

#[tokio::test]
async fn squash_from_the_trash_restores_and_folds() {
    let dir = TempDir::new().unwrap();
    squash_repo(dir.path());
    let server = open_server(dir.path());

    let listed = shas(&server).await;
    let dropped = server
        .drop_commit(Parameters(DropCommitReq { commit: listed[1].clone() }))
        .await
        .unwrap()
        .0;
    clean_head(&dropped.result);
    assert_eq!(git_log_subjects(dir.path()), ["third", "target"]);

    // Fold the trashed "follow-up" into "target".
    let target = shas(&server).await[1].clone();
    let result = squash(&server, &dropped.dropped.sha, &target, None).await;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["third", "target"]);
    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "one\ntwo");
    assert!(server.list_trash().await.unwrap().0.commits.is_empty());
}

#[tokio::test]
async fn squash_rejects_a_merge_source_and_bad_modes() {
    let dir = TempDir::new().unwrap();
    init_merge_repo(dir.path());
    let server = open_server(dir.path());

    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let merge = history.commits.iter().find(|c| c.is_merge).unwrap();
    let base = history.commits.iter().find(|c| c.subject == "base").unwrap();

    let err = expect_err(
        server
            .squash_commit(Parameters(SquashCommitReq {
                source: merge.sha.clone(),
                dest: base.sha.clone(),
                mode: None,
            }))
            .await,
    );
    assert!(err.message.contains("cannot squash"), "unexpected error: {}", err.message);

    let main1 = history.commits.iter().find(|c| c.subject == "main-1").unwrap();
    let err = expect_err(
        server
            .squash_commit(Parameters(SquashCommitReq {
                source: main1.sha.clone(),
                dest: base.sha.clone(),
                mode: Some("merge".into()),
            }))
            .await,
    );
    assert!(err.message.contains("unknown squash mode"), "unexpected error: {}", err.message);
}

#[tokio::test]
async fn mutations_reject_a_stale_ref() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first"), ("a.txt", "2\n", "second")]);
    let server = open_server(dir.path());

    let stale = shas(&server).await[0].clone();
    server
        .edit_message(Parameters(EditMessageReq { commit: stale.clone(), message: "new".into() }))
        .await
        .unwrap();

    // The pre-rewrite sha is gone from the branch now.
    let err = expect_err(
        server
            .edit_message(Parameters(EditMessageReq { commit: stale, message: "again".into() }))
            .await,
    );
    assert!(
        err.message.contains("list_history"),
        "the error should point at re-listing: {}",
        err.message
    );
}

#[tokio::test]
async fn a_sha_prefix_addresses_a_commit() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")]);
    let server = open_server(dir.path());

    let target = shas(&server).await[1].clone();
    let result = server
        .edit_message(Parameters(EditMessageReq {
            commit: target[..8].to_string(),
            message: "first, by prefix".into(),
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["second", "first, by prefix"]);
}

#[tokio::test]
async fn a_change_id_chains_mutations_without_relisting() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second")]);
    let server = open_server(dir.path());

    let history = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    let change_id = history.commits[1].change_id.clone();

    // Two mutations by the same change id, no list_history in between: the
    // first rewrite churns the sha, the change id still addresses the commit.
    let result = server
        .edit_message(Parameters(EditMessageReq {
            commit: change_id.clone(),
            message: "first, chained".into(),
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);
    let result = server
        .edit_identity(Parameters(EditIdentityReq {
            commit: change_id.clone(),
            identity: IdentityFieldsDto {
                author_name: Some("Chained Author".into()),
                ..Default::default()
            },
        }))
        .await
        .unwrap()
        .0;
    clean_head(&result);

    let listed = server
        .list_history(Parameters(ListHistoryReq { limit: None, offset: None, fields: None }))
        .await
        .unwrap()
        .0;
    assert_eq!(listed.commits[1].subject, "first, chained");
    assert_eq!(listed.commits[1].detail.author_name.as_deref().unwrap(), "Chained Author");
    assert_eq!(listed.commits[1].change_id, change_id);
}

#[tokio::test]
async fn a_too_short_ref_is_rejected() {
    let dir = TempDir::new().unwrap();
    init_repo(dir.path(), &[("a.txt", "1\n", "first")]);
    let server = open_server(dir.path());

    let err = expect_err(
        server
            .edit_message(Parameters(EditMessageReq { commit: "abc".into(), message: "x".into() }))
            .await,
    );
    assert!(err.message.contains("too short"), "unexpected error: {}", err.message);
}

#[tokio::test]
async fn squash_prefers_a_history_match_over_the_trash() {
    let dir = TempDir::new().unwrap();
    squash_repo(dir.path());
    let server = open_server(dir.path());

    // Drop "follow-up", then undo: the identical commit now sits in the
    // history *and* (stale) in the trash under the same ids.
    let listed = shas(&server).await;
    let dropped = server
        .drop_commit(Parameters(DropCommitReq { commit: listed[1].clone() }))
        .await
        .unwrap()
        .0;
    clean_head(&dropped.result);
    server.undo().await.unwrap();
    assert_eq!(git_log_subjects(dir.path()), ["third", "follow-up", "target"]);
    assert_eq!(server.list_trash().await.unwrap().0.commits.len(), 1);

    // The duplicated ref resolves to the history commit: a plain in-history
    // squash that leaves the stale trash entry alone.
    let target = shas(&server).await[2].clone();
    let result = squash(&server, &dropped.dropped.sha, &target, None).await;
    clean_head(&result);

    assert_eq!(git_log_subjects(dir.path()), ["third", "target"]);
    assert_eq!(git(dir.path(), &["show", "HEAD~1:a.txt"]), "one\ntwo");
    assert_eq!(server.list_trash().await.unwrap().0.commits.len(), 1);
}

#[tokio::test]
async fn show_commit_finds_a_trashed_commit_by_change_id_prefix() {
    let dir = TempDir::new().unwrap();
    init_repo(
        dir.path(),
        &[("a.txt", "1\n", "first"), ("b.txt", "2\n", "second"), ("c.txt", "3\n", "third")],
    );
    let server = open_server(dir.path());

    let target = shas(&server).await[1].clone();
    let dropped = server
        .drop_commit(Parameters(DropCommitReq { commit: target }))
        .await
        .unwrap()
        .0;
    clean_head(&dropped.result);

    let shown = server
        .show_commit(Parameters(ShowCommitReq {
            commit: dropped.dropped.change_id[..8].to_string(),
            include_contents: None,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(shown.commit.subject, "second");
    assert_eq!(shown.files[0].path, "b.txt");
}
