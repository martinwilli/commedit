//! Pure engine → DTO conversions. Keeping them free of locking and I/O makes
//! the response shapes unit-testable without a repository.

use std::collections::BTreeMap;

use commedit_engine::conflict::{ConflictedCommit, OpEntry, SaveOutcome};
use commedit_engine::diff::{unified_diff, ChangeKind, FileChange};
use commedit_engine::history::CommitInfo;
use commedit_engine::squash::{parse_squash_mode, SquashMode};
use commedit_engine::transparency::{RefDecoration, RefKind};
use commedit_engine::workcopy::WorkingCopyEntry;
use jj_lib::object_id::ObjectId as _;

use crate::dto::{
    CommitDto, ConflictedCommitDto, ConflictedPathDto, FileChangeDto, OpEntryDto, RefDto,
    SaveResultDto, WorkingCopyEntryDto,
};

/// The fixed protocol reminder attached to every `Conflicts` result.
pub const CONFLICT_GUIDANCE: &str = "History is untouched in git until this resolves. \
Resolve the OLDEST commit first (fixing it often auto-clears descendants): read_conflict \
each resolvable file, remove ALL conflict markers, then resolve_conflicts echoing each \
file's marker_len. Files with resolvable=false are structural; abort_rewrite is the only \
way out. No other mutation is allowed until status is clean.";

/// A commit row plus its ref decorations as one response object. `root` is the
/// virtual root commit's id — a parent pointing at it is omitted, so the
/// repository's first commit reports no parents.
pub fn commit_dto(
    info: &CommitInfo,
    root_hex: &str,
    refs: &BTreeMap<String, Vec<RefDecoration>>,
) -> CommitDto {
    let sha = info.id_hex();
    CommitDto {
        change_id: info.change_id_hex(),
        subject: info.subject.clone(),
        description: info.description.clone(),
        author_name: info.author_name.clone(),
        author_email: info.author_email.clone(),
        author_time: info.author_time.clone(),
        committer_name: info.committer_name.clone(),
        committer_email: info.committer_email.clone(),
        committer_time: info.committer_time.clone(),
        parent_shas: info
            .parents
            .iter()
            .map(|p| p.hex())
            .filter(|p| p != root_hex)
            .collect(),
        is_merge: info.parents.len() >= 2,
        refs: refs.get(&sha).map(|v| v.iter().map(ref_dto).collect()).unwrap_or_default(),
        sha,
    }
}

fn ref_dto(d: &RefDecoration) -> RefDto {
    RefDto {
        name: d.name.clone(),
        kind: match d.kind {
            RefKind::Branch => "branch".to_string(),
            RefKind::Tag => "tag".to_string(),
        },
        current: d.current,
    }
}

/// Render one engine [`FileChange`] for a response: a unified diff for text
/// files, plus the full contents when `include_contents` asks for them.
pub fn file_change_dto(fc: &FileChange, include_contents: bool) -> FileChangeDto {
    let diff = (!fc.is_binary).then(|| {
        unified_diff(
            fc.old_text.as_deref().unwrap_or(""),
            fc.new_text.as_deref().unwrap_or(""),
            &fc.path,
        )
    });
    FileChangeDto {
        path: fc.path.clone(),
        kind: match fc.kind {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Removed => "removed",
        }
        .to_string(),
        is_binary: fc.is_binary,
        conflicted_base: fc.conflicted_base,
        diff,
        old_text: include_contents.then(|| fc.old_text.clone()).flatten(),
        new_text: include_contents.then(|| fc.new_text.clone()).flatten(),
    }
}

pub fn wc_entry_dto(e: &WorkingCopyEntry) -> WorkingCopyEntryDto {
    WorkingCopyEntryDto {
        sha: e.info.id_hex(),
        change_id: e.info.change_id_hex(),
        changed_files: e.changed_files,
        files: e.file_names.clone(),
        has_conflict: e.has_conflict,
    }
}

pub fn conflicted_commit_dto(c: &ConflictedCommit) -> ConflictedCommitDto {
    ConflictedCommitDto {
        change_id: c.change_id_hex(),
        sha: c.commit_id.hex(),
        subject: c.subject.clone(),
        files: c
            .files
            .iter()
            .map(|f| ConflictedPathDto { path: f.path_str(), resolvable: f.resolvable })
            .collect(),
    }
}

/// `index` is the entry's position in [`commedit_engine::repo::Repo`]'s
/// session op-log, 1-based — the value `jump_to_operation` takes (0 being the
/// session-start floor below the first entry).
pub fn op_entry_dto(index: usize, e: &OpEntry) -> OpEntryDto {
    OpEntryDto {
        index,
        label: e.label().to_string(),
        affected_change_ids: e.affected().to_vec(),
    }
}

/// Fold a mutation outcome into the tagged response: `head_sha` is the branch
/// tip after a clean save (read it after the outcome, the save moves it).
pub fn save_result_dto(outcome: &SaveOutcome, head_sha: Option<String>) -> SaveResultDto {
    match outcome {
        SaveOutcome::Clean => SaveResultDto::Clean { head_sha },
        SaveOutcome::Conflicts { commits } => SaveResultDto::Conflicts {
            commits: commits.iter().map(conflicted_commit_dto).collect(),
            guidance: CONFLICT_GUIDANCE.to_string(),
        },
    }
}

/// The squash mode a request selects: the explicit `mode` string if given,
/// else what the source's autosquash subject prefix requests, else Fixup.
/// `Err` is a human-readable message for an unknown mode string.
pub fn resolve_squash_mode(
    mode: Option<&str>,
    source_subject: &str,
) -> Result<SquashMode, String> {
    match mode {
        Some("fixup") => Ok(SquashMode::Fixup),
        Some("squash") => Ok(SquashMode::Squash),
        Some("amend") => Ok(SquashMode::Amend),
        Some(other) => Err(format!(
            "unknown squash mode {other:?}: use \"fixup\", \"squash\" or \"amend\""
        )),
        None => Ok(parse_squash_mode(source_subject).unwrap_or(SquashMode::Fixup)),
    }
}
