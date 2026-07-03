//! Pure engine → DTO conversions. Keeping them free of locking and I/O makes
//! the response shapes unit-testable without a repository.

use std::collections::{BTreeMap, HashMap, HashSet};

use commedit_engine::conflict::{ConflictedCommit, OpEntry, SaveOutcome};
use commedit_engine::diff::{render_diff, unified_diff, ChangeKind, ContextExpansion, FileChange};
use commedit_engine::history::{CommitInfo, IdAbbrev};
use commedit_engine::squash::{parse_squash_mode, SquashMode};
use commedit_engine::transparency::{RefDecoration, RefKind};
use commedit_engine::workcopy::WorkingCopyEntry;
use jj_lib::object_id::ObjectId as _;

use crate::dto::{
    AdjacencyDto, CommitDetailDto, CommitDto, CommitField, ConflictedCommitDto, ConflictedPathDto,
    FileChangeDto, HunkDto, OpEntryDto, RefDto, SaveResultDto, TopologyDto, WorkingCopyEntryDto,
};

/// Which verbose [`CommitDetailDto`] fields a `commit_dto` should populate.
/// `ALL` is the full listing `show_commit` / `list_trash` always emit;
/// `list_history` derives one from the request's `fields`.
#[derive(Clone, Copy)]
pub struct DetailFields {
    pub description: bool,
    pub author_name: bool,
    pub author_email: bool,
    pub author_time: bool,
    pub committer_name: bool,
    pub committer_email: bool,
    pub committer_time: bool,
    pub parents: bool,
}

impl DetailFields {
    pub const ALL: Self = Self {
        description: true,
        author_name: true,
        author_email: true,
        author_time: true,
        committer_name: true,
        committer_email: true,
        committer_time: true,
        parents: true,
    };
    pub const NONE: Self = Self {
        description: false,
        author_name: false,
        author_email: false,
        author_time: false,
        committer_name: false,
        committer_email: false,
        committer_time: false,
        parents: false,
    };

    /// Build a selection from a `list_history` request's `fields`: an absent
    /// list is a header-only overview (no verbose fields — verbose detail is
    /// opt-in), and an explicit list selects exactly the named fields.
    pub fn from_request(fields: Option<&[CommitField]>) -> Self {
        let Some(fields) = fields else {
            return Self::NONE;
        };
        let mut sel = Self::NONE;
        for f in fields {
            match f {
                CommitField::Description => sel.description = true,
                CommitField::AuthorName => sel.author_name = true,
                CommitField::AuthorEmail => sel.author_email = true,
                CommitField::AuthorTime => sel.author_time = true,
                CommitField::CommitterName => sel.committer_name = true,
                CommitField::CommitterEmail => sel.committer_email = true,
                CommitField::CommitterTime => sel.committer_time = true,
                CommitField::Parents => sel.parents = true,
            }
        }
        sel
    }
}

/// The short protocol reminder attached to every `Conflicts` result. The full
/// resolution protocol lives in the server instructions (and the resolve-conflicts
/// skill); this is the actionable gist.
pub const CONFLICT_GUIDANCE: &str = "Held — git untouched until the whole chain is clean. \
abort_rewrite (free) if this mutation was the mistake; else resolve the OLDEST conflict first \
(read_conflict → remove all markers → resolve_conflicts, echoing marker_len). resolvable=false \
is structural: abort only. No other mutation until clean.";

/// A commit row plus its ref decorations as one response object. `root` is the
/// virtual root commit's id — a parent pointing at it is omitted, so the
/// repository's first commit reports no parents. Emitted shas and change_ids are
/// abbreviated via `abbrev` (shortest repo-unique prefix); the ref-decoration
/// lookup and root filter stay keyed on the *full* sha.
pub fn commit_dto(
    info: &CommitInfo,
    root_hex: &str,
    refs: &BTreeMap<String, Vec<RefDecoration>>,
    abbrev: &IdAbbrev,
    fields: DetailFields,
) -> CommitDto {
    let full_sha = info.id_hex();
    CommitDto {
        change_id: abbrev.change(&info.change_id),
        subject: info.subject.clone(),
        is_merge: info.parents.len() >= 2,
        refs: refs
            .get(&full_sha)
            .map(|v| v.iter().map(ref_dto).collect())
            .unwrap_or_default(),
        detail: CommitDetailDto {
            description: fields.description.then(|| info.description.clone()),
            author_name: fields.author_name.then(|| info.author_name.clone()),
            author_email: fields.author_email.then(|| info.author_email.clone()),
            author_time: fields.author_time.then(|| info.author_time.clone()),
            committer_name: fields.committer_name.then(|| info.committer_name.clone()),
            committer_email: fields.committer_email.then(|| info.committer_email.clone()),
            committer_time: fields.committer_time.then(|| info.committer_time.clone()),
            parent_shas: fields.parents.then(|| {
                info.parents
                    .iter()
                    .filter(|p| p.hex() != root_hex)
                    .map(|p| abbrev.commit(p))
                    .collect()
            }),
        },
        sha: abbrev.commit(&info.id),
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

/// Trim trailing whitespace from a diff's context and header lines so the
/// result renders as a clean YAML block scalar rather than an escaped one-line
/// string (a lone-space blank context line collapses to an empty line). `+`/`-`
/// lines are left byte-for-byte intact, so a whitespace-only edit — which lives
/// on those lines — is never hidden; such a diff (and any with tabs) keeps the
/// quoted form, which is the only lossless YAML rendering for it. Diffs are
/// display-only here — no tool consumes one as a patch — so this never affects a
/// rewrite, only how the change reads in a transcript.
fn tidy_diff_for_display(diff: String) -> String {
    let mut out = String::with_capacity(diff.len());
    for (i, line) in diff.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.starts_with('+') || line.starts_with('-') {
            out.push_str(line);
        } else {
            out.push_str(line.trim_end());
        }
    }
    out
}

/// Per-file cap on the unified-diff lines a response carries. A diff longer than
/// this is truncated (with `truncated`/`total_lines` set), so one huge or
/// generated file can't dump tens of thousands of tokens; the hunk headers stay,
/// and a caller can re-read a file whole via show_commit's `paths` /
/// `include_contents`.
const MAX_DIFF_LINES: usize = 500;

/// Render one engine [`FileChange`] for a response: a unified diff for text
/// files (capped at [`MAX_DIFF_LINES`]), plus the full contents when
/// `include_contents` asks for them.
pub fn file_change_dto(fc: &FileChange, include_contents: bool) -> FileChangeDto {
    let (old, new) = (
        fc.old_text.as_deref().unwrap_or(""),
        fc.new_text.as_deref().unwrap_or(""),
    );
    let (diff, truncated, total_lines) = if fc.is_binary {
        (None, false, 0)
    } else {
        let full = tidy_diff_for_display(unified_diff(old, new, &fc.path));
        let total = full.lines().count();
        if total > MAX_DIFF_LINES {
            let capped = full
                .lines()
                .take(MAX_DIFF_LINES)
                .collect::<Vec<_>>()
                .join("\n");
            (Some(capped), true, total)
        } else {
            (Some(full), false, 0)
        }
    };
    // Number the diff's hunks so an agent can select them for a partial
    // commit_working_copy. render_diff with the default expansion produces the
    // same hunks the `diff` field shows; we keep only their headers + index.
    let hunks = (!fc.is_binary)
        .then(|| {
            let rendered = render_diff(old, new, &fc.path, &ContextExpansion::default());
            let lines: Vec<&str> = rendered.text.lines().collect();
            rendered
                .hunks
                .iter()
                .enumerate()
                .map(|(index, h)| HunkDto {
                    index,
                    header: lines.get(h.header_line).copied().unwrap_or("").to_string(),
                })
                .collect::<Vec<_>>()
        })
        .filter(|hs| !hs.is_empty());
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
        truncated,
        total_lines,
        hunks,
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
            .map(|f| ConflictedPathDto {
                path: f.path_str(),
                resolvable: f.resolvable,
            })
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
/// tip after a clean save (read it after the outcome, the save moves it), and
/// `topology` the optional graph slice for a topology-changing op.
pub fn save_result_dto(
    outcome: &SaveOutcome,
    head_sha: Option<String>,
    topology: Option<TopologyDto>,
) -> SaveResultDto {
    match outcome {
        SaveOutcome::Clean => SaveResultDto::Clean { head_sha, topology },
        SaveOutcome::Conflicts { commits } => SaveResultDto::Conflicts {
            commits: commits.iter().map(conflicted_commit_dto).collect(),
            guidance: CONFLICT_GUIDANCE.to_string(),
        },
    }
}

/// Build the [`TopologyDto`] for a topology-changing mutation: locate the
/// commit(s) it reshaped and emit their new neighbourhood (parents + children),
/// plus a merge tip when the new branch head is a merge.
///
/// `commits` is the post-mutation history (newest first; the virtual root is
/// already excluded by [`commedit_engine::history::history`]). Affected commits
/// are those whose stable full-hex change_id is one of `anchors` (the tool knew
/// it touched them) OR is absent from `pre_change_ids` (freshly minted, found as
/// `post − pre`). Children are derived by inverting each commit's parents — no
/// lane geometry needed. Returns `None` when nothing affected and the tip is not
/// a merge.
pub fn topology_slice(
    commits: &[CommitInfo],
    anchors: &[String],
    pre_change_ids: &HashSet<String>,
    abbrev: &IdAbbrev,
) -> Option<TopologyDto> {
    let (change_of, children) = adjacency_tables(commits, abbrev);
    let render = |c: &CommitInfo| render_adjacency(c, &change_of, &children, abbrev);

    let anchor_set: HashSet<&str> = anchors.iter().map(String::as_str).collect();
    let mut affected = Vec::new();
    let mut affected_change_hex: HashSet<String> = HashSet::new();
    for c in commits {
        let ch = c.change_id_hex();
        if anchor_set.contains(ch.as_str()) || !pre_change_ids.contains(&ch) {
            affected.push(render(c));
            affected_change_hex.insert(ch);
        }
    }

    // The tip is a merge whose shape a linear history can't show; skip it when
    // it's already among the affected commits.
    let merge_tip = commits.first().filter(|tip| {
        tip.parents.len() >= 2 && !affected_change_hex.contains(&tip.change_id_hex())
    });
    let merge_tip = merge_tip.map(render);

    if affected.is_empty() && merge_tip.is_none() {
        return None;
    }
    Some(TopologyDto {
        affected,
        merge_tip,
    })
}

/// The whole branch as a graph: every commit (newest first) with its parents and
/// children by change_id — the full view the `topology` slice gives in miniature.
/// Backs the read-only `show_graph` query.
pub fn graph_adjacency(commits: &[CommitInfo], abbrev: &IdAbbrev) -> Vec<AdjacencyDto> {
    let (change_of, children) = adjacency_tables(commits, abbrev);
    commits
        .iter()
        .map(|c| render_adjacency(c, &change_of, &children, abbrev))
        .collect()
}

/// The two tables [`render_adjacency`] reads to emit a commit's neighbourhood:
/// `change_of` maps a commit-id hex to that commit's (abbreviated) change_id, so a
/// parent edge resolves to a change_id; `children` inverts parents into each
/// commit-id hex's children's change_ids, in history order. The virtual root
/// never appears in `commits`, so it gets no `change_of` entry (parent edges to it
/// are filtered out) and its `children` bucket is never read.
fn adjacency_tables(
    commits: &[CommitInfo],
    abbrev: &IdAbbrev,
) -> (HashMap<String, String>, HashMap<String, Vec<String>>) {
    let change_of: HashMap<String, String> = commits
        .iter()
        .map(|c| (c.id_hex(), abbrev.change(&c.change_id)))
        .collect();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for c in commits {
        for p in &c.parents {
            children
                .entry(p.hex())
                .or_default()
                .push(abbrev.change(&c.change_id));
        }
    }
    (change_of, children)
}

/// One commit's [`AdjacencyDto`] from the [`adjacency_tables`]: its own change_id,
/// its parents' change_ids (the virtual root filtered out), and the change_ids of
/// the commits rebased directly on top of it.
fn render_adjacency(
    c: &CommitInfo,
    change_of: &HashMap<String, String>,
    children: &HashMap<String, Vec<String>>,
    abbrev: &IdAbbrev,
) -> AdjacencyDto {
    AdjacencyDto {
        change_id: abbrev.change(&c.change_id),
        subject: c.subject.clone(),
        parents: c
            .parents
            .iter()
            .filter_map(|p| change_of.get(&p.hex()).cloned())
            .collect(),
        children: children.get(&c.id_hex()).cloned().unwrap_or_default(),
    }
}

/// The squash mode a request selects: the explicit `mode` string if given,
/// else what the source's autosquash subject prefix requests, else Fixup.
/// `Err` is a human-readable message for an unknown mode string.
pub fn resolve_squash_mode(mode: Option<&str>, source_subject: &str) -> Result<SquashMode, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidy_diff_collapses_blanks_but_preserves_change_lines() {
        // A lone-space blank context line and a context line's trailing space go
        // away; the `+`/`-` lines (where a whitespace edit would show) are kept.
        let diff = " ctx  \n \n+added  \n-removed \n".to_string();
        assert_eq!(tidy_diff_for_display(diff), " ctx\n\n+added  \n-removed \n");
    }

    #[test]
    fn tidy_diff_renders_as_a_yaml_block_scalar() {
        #[derive(serde::Serialize)]
        struct W {
            diff: String,
        }
        // Before tidying the lone-space blank context line forces serde_yaml to
        // emit an escaped one-line string; after, it is a readable block scalar.
        let diff = " fn main() {\n \n     ok\n".to_string();
        let yaml = serde_yaml::to_string(&W {
            diff: tidy_diff_for_display(diff),
        })
        .unwrap();
        assert!(
            yaml.contains("diff: |"),
            "expected a block scalar, got: {yaml:?}"
        );
    }
}
