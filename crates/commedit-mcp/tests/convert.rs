//! Pure DTO conversion tests — response shapes, serde tags and mode defaults,
//! without a repository.

use std::collections::BTreeMap;

use commedit_engine::conflict::{ConflictedCommit, ConflictedPath, SaveOutcome};
use commedit_engine::diff::{ChangeKind, FileChange};
use commedit_engine::history::CommitInfo;
use commedit_engine::squash::SquashMode;
use commedit_engine::transparency::{RefDecoration, RefKind};
use commedit_mcp::convert::{
    commit_dto, conflicted_commit_dto, file_change_dto, resolve_squash_mode, save_result_dto,
    CONFLICT_GUIDANCE,
};
use commedit_mcp::dto::SaveResultDto;
use jj_lib::backend::{ChangeId, CommitId};
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo_path::RepoPath;

fn ci(id: u8, parents: &[u8]) -> CommitInfo {
    CommitInfo {
        id: CommitId::new(vec![id]),
        change_id: ChangeId::new(vec![id]),
        subject: format!("subject {id}"),
        description: format!("subject {id}\n\nbody"),
        author_name: "Author".into(),
        author_email: "a@example.com".into(),
        committer_name: "Committer".into(),
        committer_email: "c@example.com".into(),
        author_time: "2026-01-01 10:00:00 +0100".into(),
        committer_time: "2026-01-02 10:00:00 +0100".into(),
        parents: parents.iter().map(|p| CommitId::new(vec![*p])).collect(),
    }
}

#[test]
fn commit_dto_maps_fields_and_filters_the_root_parent() {
    let root_hex = CommitId::new(vec![0]).hex();
    let mut refs = BTreeMap::new();
    refs.insert(
        CommitId::new(vec![3]).hex(),
        vec![RefDecoration { name: "main".into(), kind: RefKind::Branch, current: true }],
    );

    let dto = commit_dto(&ci(3, &[2]), &root_hex, &refs);
    assert_eq!(dto.sha, CommitId::new(vec![3]).hex());
    assert_eq!(dto.change_id, ChangeId::new(vec![3]).hex());
    assert_eq!(dto.subject, "subject 3");
    assert_eq!(dto.parent_shas, vec![CommitId::new(vec![2]).hex()]);
    assert!(!dto.is_merge);
    assert_eq!(dto.refs.len(), 1);
    assert_eq!(dto.refs[0].name, "main");
    assert_eq!(dto.refs[0].kind, "branch");
    assert!(dto.refs[0].current);

    // The oldest commit's parent is the virtual root: not a real commit.
    let oldest = commit_dto(&ci(1, &[0]), &root_hex, &BTreeMap::new());
    assert!(oldest.parent_shas.is_empty());
    assert!(oldest.refs.is_empty());

    let merge = commit_dto(&ci(4, &[3, 2]), &root_hex, &BTreeMap::new());
    assert!(merge.is_merge);
    assert_eq!(merge.parent_shas.len(), 2);
}

#[test]
fn file_change_dto_renders_a_diff_and_gates_contents() {
    let fc = FileChange {
        path: "src/a.txt".into(),
        kind: ChangeKind::Modified,
        old_text: Some("one\ntwo\n".into()),
        new_text: Some("one\nTWO\n".into()),
        is_binary: false,
        conflicted_base: false,
    };
    let without = file_change_dto(&fc, false);
    assert_eq!(without.kind, "modified");
    let diff = without.diff.expect("text file has a diff");
    assert!(diff.contains("-two"), "diff shows the removed line: {diff}");
    assert!(diff.contains("+TWO"), "diff shows the added line: {diff}");
    assert!(without.old_text.is_none() && without.new_text.is_none());

    let with = file_change_dto(&fc, true);
    assert_eq!(with.old_text.as_deref(), Some("one\ntwo\n"));
    assert_eq!(with.new_text.as_deref(), Some("one\nTWO\n"));
}

#[test]
fn binary_files_carry_no_diff_or_contents() {
    let fc = FileChange {
        path: "blob.bin".into(),
        kind: ChangeKind::Added,
        old_text: None,
        new_text: None,
        is_binary: true,
        conflicted_base: false,
    };
    let dto = file_change_dto(&fc, true);
    assert_eq!(dto.kind, "added");
    assert!(dto.is_binary);
    assert!(dto.diff.is_none());
    assert!(dto.old_text.is_none() && dto.new_text.is_none());
}

#[test]
fn save_result_serializes_with_a_status_tag() {
    let clean = save_result_dto(&SaveOutcome::Clean, Some("abc123".into()));
    let json = serde_json::to_value(&clean).unwrap();
    assert_eq!(json["status"], "clean");
    assert_eq!(json["head_sha"], "abc123");

    let conflicted = SaveOutcome::Conflicts {
        commits: vec![ConflictedCommit {
            change_id: ChangeId::new(vec![7]),
            commit_id: CommitId::new(vec![8]),
            subject: "subject 7".into(),
            files: vec![ConflictedPath {
                path: RepoPath::from_internal_string("a.txt").unwrap().to_owned(),
                resolvable: true,
            }],
        }],
    };
    let dto = save_result_dto(&conflicted, None);
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json["status"], "conflicts");
    assert_eq!(json["guidance"], CONFLICT_GUIDANCE);
    assert_eq!(json["commits"][0]["change_id"], ChangeId::new(vec![7]).hex());
    assert_eq!(json["commits"][0]["files"][0]["path"], "a.txt");
    assert_eq!(json["commits"][0]["files"][0]["resolvable"], true);
    match dto {
        SaveResultDto::Conflicts { commits, .. } => assert_eq!(commits.len(), 1),
        SaveResultDto::Clean { .. } => panic!("expected conflicts"),
    }
}

#[test]
fn conflicted_commit_dto_keys_on_the_change_id() {
    let c = ConflictedCommit {
        change_id: ChangeId::new(vec![9]),
        commit_id: CommitId::new(vec![1]),
        subject: "s".into(),
        files: vec![ConflictedPath {
            path: RepoPath::from_internal_string("dir/f").unwrap().to_owned(),
            resolvable: false,
        }],
    };
    let dto = conflicted_commit_dto(&c);
    assert_eq!(dto.change_id, ChangeId::new(vec![9]).hex());
    assert_eq!(dto.sha, CommitId::new(vec![1]).hex());
    assert_eq!(dto.files[0].path, "dir/f");
    assert!(!dto.files[0].resolvable);
}

#[test]
fn squash_mode_resolution_prefers_explicit_then_prefix_then_fixup() {
    assert_eq!(resolve_squash_mode(Some("squash"), "fixup! x"), Ok(SquashMode::Squash));
    assert_eq!(resolve_squash_mode(Some("amend"), "x"), Ok(SquashMode::Amend));
    assert_eq!(resolve_squash_mode(Some("fixup"), "x"), Ok(SquashMode::Fixup));
    assert!(resolve_squash_mode(Some("merge"), "x").is_err());
    assert_eq!(resolve_squash_mode(None, "squash! x"), Ok(SquashMode::Squash));
    assert_eq!(resolve_squash_mode(None, "amend! x"), Ok(SquashMode::Amend));
    assert_eq!(resolve_squash_mode(None, "plain subject"), Ok(SquashMode::Fixup));
}
