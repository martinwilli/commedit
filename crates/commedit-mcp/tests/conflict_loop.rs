//! The conflict-resolution protocol end-to-end: a mutation whose descendant
//! rebase conflicts is held back from git in full, then resolved oldest-first
//! through the conflict tools (or discarded by abort_rewrite).

mod common;

use commedit_mcp::dto::{
    ConflictFileEditDto, ConflictPatchEditDto, DropCommitReq, EditMessageReq, FileContentDto,
    ListHistoryReq, ReadConflictReq, ReplaceFilesReq, ResolveConflictsReq, SaveResultDto,
};
use commedit_mcp::server::CommeditServer;
use common::{expect_err, git, git_log_subjects, init_repo, open_server, sel};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

/// Three commits all editing the same line of `f.txt` — editing the middle
/// commit's content makes the rebase of its descendant conflict.
fn conflicting_repo(dir: &std::path::Path) {
    init_repo(
        dir,
        &[
            ("f.txt", "1\n2\n3\n", "base"),
            ("f.txt", "1\nA\n3\n", "A"),
            ("f.txt", "1\nB\n3\n", "B"),
        ],
    );
}

/// Rewrite commit "A"'s content so the descendant "B" no longer applies.
async fn conflicting_edit(server: &CommeditServer) -> SaveResultDto {
    let history = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    let a = history.commits.iter().find(|c| c.subject == "A").unwrap();
    server
        .replace_files(Parameters(ReplaceFilesReq {
            session: sel("main"),
            commit: a.sha.clone(),
            files: vec![FileContentDto {
                path: "f.txt".into(),
                content: "1\nX\n3\n".into(),
            }],
            delete_paths: None,
        }))
        .await
        .unwrap()
        .0
}

#[tokio::test]
async fn a_conflicting_edit_is_held_back_then_resolved_oldest_first() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let head_before = git(dir.path(), &["rev-parse", "HEAD"]);
    let server = open_server(dir.path());

    let mut result = conflicting_edit(&server).await;
    assert!(
        matches!(result, SaveResultDto::Conflicts { .. }),
        "the non-commuting edit should conflict"
    );

    // Held back in full: git history, HEAD and worktree untouched.
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_log_subjects(dir.path()), ["B", "A", "base"]);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");

    // The pending state is also visible via pending_status.
    let pending = server
        .pending_status(Parameters(sel("main")))
        .await
        .unwrap()
        .0;
    assert!(pending.pending);
    assert!(!pending.conflicts.is_empty());
    assert_ne!(pending.git_head_sha, pending.jj_head_sha);

    // No other mutation may run while pending.
    let stale = git(dir.path(), &["rev-parse", "HEAD"]);
    let err = expect_err(
        server
            .edit_message(Parameters(EditMessageReq {
                session: sel("main"),
                commit: stale,
                message: "x".into(),
            }))
            .await,
    );
    assert!(
        err.message.contains("pending"),
        "unexpected error: {}",
        err.message
    );

    // Resolve oldest-first until the chain is clean.
    let mut steps = 0;
    while let SaveResultDto::Conflicts { commits, guidance } = result {
        assert!(guidance.contains("OLDEST"), "guidance rides along");
        let oldest = &commits[0];
        let path = &oldest.files[0];
        assert!(path.resolvable);

        let resp = server
            .read_conflict(Parameters(ReadConflictReq {
                session: sel("main"),
                commit: oldest.change_id.clone(),
                path: Some(path.path.clone()),
                paths: None,
                context_lines: None,
                full: None,
            }))
            .await
            .unwrap()
            .0;
        let file = &resp.files[0];
        assert!(
            file.text.contains("<<<<<<<"),
            "markers present: {}",
            file.text
        );
        assert!(
            !file.text.contains("|||||||"),
            "2-way markers omit the base"
        );

        result = server
            .resolve_conflicts(Parameters(ResolveConflictsReq {
                session: sel("main"),
                commit: oldest.change_id.clone(),
                files: vec![ConflictFileEditDto {
                    path: path.path.clone(),
                    text: Some("1\nR\n3\n".into()),
                    marker_len: Some(file.marker_len),
                    edits: None,
                    delete: None,
                }],
            }))
            .await
            .unwrap()
            .0;
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }

    // Clean: the rewrite (with resolutions) is exported to plain git.
    assert!(
        !server
            .pending_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .pending
    );
    assert_eq!(git_log_subjects(dir.path()), ["B", "A", "base"]);
    assert_eq!(git(dir.path(), &["show", "HEAD~1:f.txt"]), "1\nX\n3");
    assert_eq!(git(dir.path(), &["show", "HEAD:f.txt"]), "1\nR\n3");
    assert_eq!(
        git(dir.path(), &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    let tree = git(dir.path(), &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree.contains(".jjconflict"), "no conflict residue: {tree}");
    git(dir.path(), &["fsck", "--no-progress"]);
}

#[tokio::test]
async fn read_conflict_validates_change_and_path() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let server = open_server(dir.path());

    // Nothing pending yet.
    let err = expect_err(
        server
            .read_conflict(Parameters(ReadConflictReq {
                session: sel("main"),
                commit: "00".into(),
                path: Some("f.txt".into()),
                paths: None,
                context_lines: None,
                full: None,
            }))
            .await,
    );
    assert!(
        err.message.contains("pending"),
        "unexpected error: {}",
        err.message
    );

    let result = conflicting_edit(&server).await;
    let SaveResultDto::Conflicts { commits, .. } = result else {
        panic!("expected conflicts");
    };

    let err = expect_err(
        server
            .read_conflict(Parameters(ReadConflictReq {
                session: sel("main"),
                commit: commits[0].change_id.clone(),
                path: Some("nope.txt".into()),
                paths: None,
                context_lines: None,
                full: None,
            }))
            .await,
    );
    assert!(
        err.message.contains("f.txt"),
        "names the real conflicted files: {}",
        err.message
    );
}

#[tokio::test]
async fn read_conflict_reads_every_file_in_one_call() {
    let dir = TempDir::new().unwrap();
    let p = dir.path();
    git(p, &["-c", "init.defaultBranch=main", "init", "-q"]);
    // Three commits, each touching the same line of TWO files.
    for (msg, mid) in [("base", "2"), ("A", "A"), ("B", "B")] {
        std::fs::write(p.join("f.txt"), format!("1\n{mid}\n3\n")).unwrap();
        std::fs::write(p.join("g.txt"), format!("x\n{mid}\nz\n")).unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-q", "-m", msg]);
    }
    let server = open_server(p);

    // Rewrite "A"'s content for both files so "B"'s rebase conflicts on both.
    let history = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    let a = history.commits.iter().find(|c| c.subject == "A").unwrap();
    let result = server
        .replace_files(Parameters(ReplaceFilesReq {
            session: sel("main"),
            commit: a.sha.clone(),
            files: vec![
                FileContentDto {
                    path: "f.txt".into(),
                    content: "1\nX\n3\n".into(),
                },
                FileContentDto {
                    path: "g.txt".into(),
                    content: "x\nX\nz\n".into(),
                },
            ],
            delete_paths: None,
        }))
        .await
        .unwrap()
        .0;
    let SaveResultDto::Conflicts { commits, .. } = result else {
        panic!("expected conflicts on both files");
    };
    let oldest = &commits[0];
    assert_eq!(oldest.files.len(), 2, "both files conflict");

    // Omitting both `path` and `paths` reads every resolvable file at once.
    let resp = server
        .read_conflict(Parameters(ReadConflictReq {
            session: sel("main"),
            commit: oldest.change_id.clone(),
            path: None,
            paths: None,
            context_lines: None,
            full: None,
        }))
        .await
        .unwrap()
        .0;
    assert_eq!(resp.files.len(), 2, "one round-trip returns both files");
    let mut paths: Vec<&str> = resp.files.iter().map(|f| f.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(paths, ["f.txt", "g.txt"]);
    for f in &resp.files {
        assert!(
            f.text.contains("<<<<<<<"),
            "markers present in {}: {}",
            f.path,
            f.text
        );
    }
}

#[tokio::test]
async fn abort_rewrite_restores_the_original_history() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let head_before = git(dir.path(), &["rev-parse", "HEAD"]);
    let server = open_server(dir.path());

    // Abort without a pending rewrite is refused.
    let err = expect_err(server.abort_rewrite(Parameters(sel("main"))).await);
    assert!(
        err.message.contains("pending"),
        "unexpected error: {}",
        err.message
    );

    let result = conflicting_edit(&server).await;
    assert!(matches!(result, SaveResultDto::Conflicts { .. }));

    let resp = server
        .abort_rewrite(Parameters(sel("main")))
        .await
        .unwrap()
        .0;
    assert!(resp.ok);
    assert_eq!(resp.head_sha.as_deref(), Some(head_before.as_str()));
    assert!(
        !server
            .pending_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .pending
    );

    // Git never saw any of it; mutations work again.
    assert_eq!(git_log_subjects(dir.path()), ["B", "A", "base"]);
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
    let tip = git(dir.path(), &["rev-parse", "HEAD"]);
    let clean = server
        .edit_message(Parameters(EditMessageReq {
            session: sel("main"),
            commit: tip,
            message: "B, edited".into(),
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(clean, SaveResultDto::Clean { .. }));
    assert_eq!(git_log_subjects(dir.path()), ["B, edited", "A", "base"]);
}

#[tokio::test]
async fn a_conflicted_drop_lands_in_the_trash_only_after_settling_clean() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let server = open_server(dir.path());

    // Dropping "A" leaves "B"'s same-line edit dangling: a true conflict.
    let history = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    let a = history.commits.iter().find(|c| c.subject == "A").unwrap();
    let resp = server
        .drop_commit(Parameters(DropCommitReq {
            session: sel("main"),
            commit: a.sha.clone(),
            keep_changes: false,
        }))
        .await
        .unwrap()
        .0;
    let SaveResultDto::Conflicts { commits, .. } = resp.result else {
        panic!("expected the drop to conflict");
    };
    assert_eq!(resp.dropped.subject, "A");

    // While pending, the trash push is only staged — not visible yet.
    assert!(server
        .list_trash(Parameters(sel("main")))
        .await
        .unwrap()
        .0
        .commits
        .is_empty());

    // Resolving the conflict settles the drop; now the trash has it.
    let oldest = &commits[0];
    let resp = server
        .read_conflict(Parameters(ReadConflictReq {
            session: sel("main"),
            commit: oldest.change_id.clone(),
            path: Some(oldest.files[0].path.clone()),
            paths: None,
            context_lines: None,
            full: None,
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
                text: Some("1\nB\n3\n".into()),
                marker_len: Some(file.marker_len),
                edits: None,
                delete: None,
            }],
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(result, SaveResultDto::Clean { .. }));

    let trash = server.list_trash(Parameters(sel("main"))).await.unwrap().0;
    assert_eq!(trash.commits.len(), 1);
    assert_eq!(trash.commits[0].subject, "A");
    assert_eq!(git_log_subjects(dir.path()), ["B", "base"]);
}

#[tokio::test]
async fn an_aborted_drop_leaves_the_trash_untouched() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let server = open_server(dir.path());

    let history = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    let a = history.commits.iter().find(|c| c.subject == "A").unwrap();
    let resp = server
        .drop_commit(Parameters(DropCommitReq {
            session: sel("main"),
            commit: a.sha.clone(),
            keep_changes: false,
        }))
        .await
        .unwrap()
        .0;
    assert!(matches!(resp.result, SaveResultDto::Conflicts { .. }));

    server.abort_rewrite(Parameters(sel("main"))).await.unwrap();
    assert!(server
        .list_trash(Parameters(sel("main")))
        .await
        .unwrap()
        .0
        .commits
        .is_empty());
    assert_eq!(git_log_subjects(dir.path()), ["B", "A", "base"]);
}

#[tokio::test]
async fn conflicts_resolve_by_sha_or_prefix() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let server = open_server(dir.path());

    let mut result = conflicting_edit(&server).await;
    let mut steps = 0;
    while let SaveResultDto::Conflicts { commits, .. } = result {
        let oldest = &commits[0];

        // Read by the commit's current sha, resolve by a change-id prefix.
        let resp = server
            .read_conflict(Parameters(ReadConflictReq {
                session: sel("main"),
                commit: oldest.sha.clone(),
                path: Some(oldest.files[0].path.clone()),
                paths: None,
                context_lines: None,
                full: None,
            }))
            .await
            .unwrap()
            .0;
        let file = &resp.files[0];
        result = server
            .resolve_conflicts(Parameters(ResolveConflictsReq {
                session: sel("main"),
                commit: oldest.change_id[..8].to_string(),
                files: vec![ConflictFileEditDto {
                    path: oldest.files[0].path.clone(),
                    text: Some("1\nR\n3\n".into()),
                    marker_len: Some(file.marker_len),
                    edits: None,
                    delete: None,
                }],
            }))
            .await
            .unwrap()
            .0;
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }

    assert!(
        !server
            .pending_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .pending
    );
    assert_eq!(git(dir.path(), &["show", "HEAD:f.txt"]), "1\nR\n3");
}

/// Resolve a conflict with a surgical `edits` patch against the marker text —
/// the small old→new alternative to resending the whole resolved file.
#[tokio::test]
async fn a_conflict_resolves_via_edits_patching_the_marker_text() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let server = open_server(dir.path());

    let mut result = conflicting_edit(&server).await;
    let mut steps = 0;
    while let SaveResultDto::Conflicts { commits, .. } = result {
        let oldest = &commits[0];
        let path = oldest.files[0].path.clone();

        let resp = server
            .read_conflict(Parameters(ReadConflictReq {
                session: sel("main"),
                commit: oldest.change_id.clone(),
                path: Some(path.clone()),
                paths: None,
                context_lines: None,
                full: None,
            }))
            .await
            .unwrap()
            .0;
        let text = resp.files[0].text.clone();
        assert!(text.contains("<<<<<<<"), "markers present: {text}");

        // Patch only the `<<<<<<< … >>>>>>>` block — not the whole file — down
        // to the chosen line. The untouched context ("1" / "3") is never resent.
        let start = text.find("<<<<<<<").expect("start marker");
        let gt = text.find(">>>>>>>").expect("end marker");
        let end = text[gt..]
            .find('\n')
            .map(|i| gt + i + 1)
            .expect("newline after end marker");
        let block = text[start..end].to_string();

        result = server
            .resolve_conflicts(Parameters(ResolveConflictsReq {
                session: sel("main"),
                commit: oldest.change_id.clone(),
                files: vec![ConflictFileEditDto {
                    path,
                    text: None,
                    marker_len: None,
                    edits: Some(vec![ConflictPatchEditDto {
                        old: block,
                        new: "R\n".into(),
                        replace_all: None,
                    }]),
                    delete: None,
                }],
            }))
            .await
            .unwrap()
            .0;
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }

    // Clean: the patched resolution reached git, markers gone.
    assert!(
        matches!(result, SaveResultDto::Clean { .. }),
        "the edits resolution should settle clean"
    );
    assert!(
        !server
            .pending_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .pending
    );
    assert_eq!(git(dir.path(), &["show", "HEAD~1:f.txt"]), "1\nX\n3");
    assert_eq!(git(dir.path(), &["show", "HEAD:f.txt"]), "1\nR\n3");
    assert_eq!(git(dir.path(), &["status", "--porcelain"]), "");
}

/// The `edits` path surfaces a bad patch (absent / ambiguous / empty `old`) and
/// a mode conflict as `invalid` caller errors, leaving the rewrite still
/// pending so the caller can just retry with a fixed edit.
#[tokio::test]
async fn resolve_conflicts_rejects_bad_patch_and_mode_conflicts() {
    let dir = TempDir::new().unwrap();
    conflicting_repo(dir.path());
    let server = open_server(dir.path());

    let result = conflicting_edit(&server).await;
    let SaveResultDto::Conflicts { commits, .. } = result else {
        panic!("expected conflicts");
    };
    let oldest = &commits[0];
    let change = oldest.change_id.clone();
    let path = oldest.files[0].path.clone();

    // Build a single-edit `edits` request against the shared pending commit.
    let req = |edits: Vec<ConflictPatchEditDto>| {
        Parameters(ResolveConflictsReq {
            session: sel("main"),
            commit: change.clone(),
            files: vec![ConflictFileEditDto {
                path: path.clone(),
                text: None,
                marker_len: None,
                edits: Some(edits),
                delete: None,
            }],
        })
    };

    // `old` absent → NotFound, surfaced as invalid with a closest-match hint.
    let err = expect_err(
        server
            .resolve_conflicts(req(vec![ConflictPatchEditDto {
                old: "this text is nowhere in the conflict\n".into(),
                new: "x".into(),
                replace_all: None,
            }]))
            .await,
    );
    assert!(
        err.message.contains("not found") && err.message.contains("closest"),
        "not-found error with hint: {}",
        err.message
    );

    // `old` matching many times (a bare newline) → Ambiguous.
    let err = expect_err(
        server
            .resolve_conflicts(req(vec![ConflictPatchEditDto {
                old: "\n".into(),
                new: "x".into(),
                replace_all: None,
            }]))
            .await,
    );
    assert!(
        err.message.contains("matched") && err.message.contains("times"),
        "ambiguous error names the count: {}",
        err.message
    );

    // An empty `old` is rejected at the boundary (mirrors replace_in_file).
    let err = expect_err(
        server
            .resolve_conflicts(req(vec![ConflictPatchEditDto {
                old: String::new(),
                new: "x".into(),
                replace_all: None,
            }]))
            .await,
    );
    assert!(err.message.contains("empty"), "empty old: {}", err.message);

    // An empty `edits` vec is a no-op mode — rejected.
    let err = expect_err(server.resolve_conflicts(req(Vec::new())).await);
    assert!(
        err.message.contains("empty"),
        "empty edits: {}",
        err.message
    );

    // Two modes at once (full text and edits) is a caller mistake.
    let err = expect_err(
        server
            .resolve_conflicts(Parameters(ResolveConflictsReq {
                session: sel("main"),
                commit: change.clone(),
                files: vec![ConflictFileEditDto {
                    path: path.clone(),
                    text: Some("1\nR\n3\n".into()),
                    marker_len: Some(2),
                    edits: Some(vec![ConflictPatchEditDto {
                        old: "1".into(),
                        new: "2".into(),
                        replace_all: None,
                    }]),
                    delete: None,
                }],
            }))
            .await,
    );
    assert!(
        err.message.contains("exactly one"),
        "mode conflict: {}",
        err.message
    );

    // None of the rejects touched git or cleared the pending rewrite.
    assert!(
        server
            .pending_status(Parameters(sel("main")))
            .await
            .unwrap()
            .0
            .pending
    );
}

/// A large file whose only contested line is `mid`, sandwiched between 20
/// padding lines on each side.
fn big_file(mid: &str) -> String {
    let mut s = String::new();
    for i in 1..=20 {
        s.push_str(&format!("pad {i:02}\n"));
    }
    s.push_str(mid);
    s.push('\n');
    for i in 21..=40 {
        s.push_str(&format!("pad {i:02}\n"));
    }
    s
}

/// Slice out the `<<<<<<< … >>>>>>>` block (verbatim, through the end of the
/// close-marker line) so it can serve as a patch `old`.
fn conflict_block(text: &str) -> String {
    let start = text.find("<<<<<<<").expect("open marker");
    let close = text[start..].find(">>>>>>>").expect("close marker") + start;
    let end = text[close..]
        .find('\n')
        .map(|n| close + n + 1)
        .unwrap_or(text.len());
    text[start..end].to_string()
}

#[tokio::test]
async fn read_conflict_windows_to_the_hunk_and_patches_from_the_window() {
    let dir = TempDir::new().unwrap();
    // base <- A <- B, all editing the one contested line of a 41-line file.
    init_repo(
        dir.path(),
        &[
            ("big.txt", big_file("2").as_str(), "base"),
            ("big.txt", big_file("A").as_str(), "A"),
            ("big.txt", big_file("B").as_str(), "B"),
        ],
    );
    let server = open_server(dir.path());

    // Rewrite A's content so B's rebase conflicts on the contested line.
    let history = server
        .list_history(Parameters(ListHistoryReq {
            session: sel("main"),
            limit: None,
            offset: None,
            fields: None,
            working_copy: None,
        }))
        .await
        .unwrap()
        .0;
    let a = history.commits.iter().find(|c| c.subject == "A").unwrap();
    let mut result = server
        .replace_files(Parameters(ReplaceFilesReq {
            session: sel("main"),
            commit: a.sha.clone(),
            files: vec![FileContentDto {
                path: "big.txt".into(),
                content: big_file("X"),
            }],
            delete_paths: None,
        }))
        .await
        .unwrap()
        .0;
    // Resolve oldest-first (B's rebase and the working copy that rides it both
    // conflict). On the first round, prove the read is windowed and that a patch
    // lifted from that window still resolves against the untrimmed content.
    let mut steps = 0;
    let mut checked_window = false;
    while let SaveResultDto::Conflicts { commits, .. } = result {
        let change = commits[0].change_id.clone();

        // Default read is windowed: the hunk with a little context, far padding
        // collapsed into sentinels — not the whole 41-line file.
        let windowed = server
            .read_conflict(Parameters(ReadConflictReq {
                session: sel("main"),
                commit: change.clone(),
                path: Some("big.txt".into()),
                paths: None,
                context_lines: None,
                full: None,
            }))
            .await
            .unwrap()
            .0
            .files
            .remove(0);
        assert!(windowed.text.contains("<<<<<<<") && windowed.text.contains(">>>>>>>"));

        if !checked_window {
            assert!(
                windowed.text.contains("omitted"),
                "elision sentinel present: {}",
                windowed.text
            );
            assert!(
                !windowed.text.contains("pad 01"),
                "far padding is trimmed: {}",
                windowed.text
            );
            assert!(
                windowed.text.lines().count() < 15,
                "windowed to the hunk, got {} lines",
                windowed.text.lines().count()
            );
            // full: true returns the entire file, no sentinels.
            let full = server
                .read_conflict(Parameters(ReadConflictReq {
                    session: sel("main"),
                    commit: change.clone(),
                    path: Some("big.txt".into()),
                    paths: None,
                    context_lines: None,
                    full: Some(true),
                }))
                .await
                .unwrap()
                .0
                .files
                .remove(0);
            assert!(!full.text.contains("omitted"), "full view has no sentinel");
            assert!(full.text.contains("pad 01") && full.text.contains("pad 40"));
            checked_window = true;
        }

        // Resolve with a patch whose `old` is lifted straight from the WINDOWED
        // view — it still matches uniquely against the untrimmed content.
        result = server
            .resolve_conflicts(Parameters(ResolveConflictsReq {
                session: sel("main"),
                commit: change,
                files: vec![ConflictFileEditDto {
                    path: "big.txt".into(),
                    text: None,
                    marker_len: None,
                    edits: Some(vec![ConflictPatchEditDto {
                        old: conflict_block(&windowed.text),
                        new: "MID\n".into(),
                        replace_all: None,
                    }]),
                    delete: None,
                }],
            }))
            .await
            .unwrap()
            .0;
        steps += 1;
        assert!(steps < 10, "resolution should converge");
    }

    let shown = git(dir.path(), &["show", "HEAD:big.txt"]);
    assert!(
        shown.contains("\nMID\n"),
        "contested line resolved: {shown}"
    );
    assert!(shown.starts_with("pad 01\n") && shown.trim_end().ends_with("pad 40"));
    assert!(!shown.contains("<<<<<<<"), "no markers left behind");
    assert_eq!(git_log_subjects(dir.path()), ["B", "A", "base"]);
}
