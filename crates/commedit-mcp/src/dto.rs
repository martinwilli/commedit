//! Request and response types of the tool surface. No jj-lib type crosses
//! this boundary — everything is plain strings and flags.
//!
//! Doc comments on fields become JSON-schema descriptions, so they are written
//! as documentation for the calling agent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `skip_serializing_if` for a bool whose documented default is `false`: the
/// field is omitted when false, so an absent field reads as false. Keeps the
/// common case (a non-merge commit, a text file, a clean entry…) off the wire.
fn is_false(b: &bool) -> bool {
    !*b
}

/// `skip_serializing_if` for a count whose documented default is `0`.
fn is_zero(n: &usize) -> bool {
    *n == 0
}

// ---------------------------------------------------------------------------
// Shared response shapes

/// One commit of the current branch's history.
///
/// The core header — sha, change_id, subject — is always present; `is_merge`
/// and `refs` ride alongside but are omitted at their default (a non-merge, an
/// undecorated commit), so a plain commit reduces to those three keys plus the
/// selected detail. The verbose [`CommitDetailDto`] fields (message body,
/// identity, parents) are flattened in alongside; each appears only when
/// `list_history`'s `fields` selects it (all of them by default, none for a
/// header-only overview). `show_commit` and `list_trash` always include them all.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommitDto {
    /// Full commit id. Every mutation rewrites ids — address commits by their
    /// change_id instead of reusing shas across mutations.
    pub sha: String,
    /// jj change id: stable across rewrites, identifies the logical commit
    /// while its sha churns — the preferred ref for chaining mutations.
    pub change_id: String,
    /// First line of the commit message.
    pub subject: String,
    /// Merge commits cannot be reordered, dropped, split, reverted,
    /// cherry-picked or used as a squash source (squashing *into* one is fine).
    /// Omitted when false (the default) — present and true only on a merge.
    #[serde(skip_serializing_if = "is_false")]
    pub is_merge: bool,
    /// Local branches and tags pointing at this commit. Omitted when empty (the
    /// default) — present only on a decorated commit.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<RefDto>,
    /// The verbose fields, each present only when selected (see `CommitField`).
    #[serde(flatten)]
    pub detail: CommitDetailDto,
}

/// The verbose fields of a [`CommitDto`]. Each is present only when
/// `list_history`'s `fields` selects the matching [`CommitField`] (all of them
/// for `show_commit` / `list_trash`); an unselected field is omitted entirely.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommitDetailDto {
    /// Full commit message, including the subject line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_email: Option<String>,
    /// `YYYY-MM-DD HH:MM:SS ±HHMM`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committer_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committer_email: Option<String>,
    /// `YYYY-MM-DD HH:MM:SS ±HHMM`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committer_time: Option<String>,
    /// Parent shas; empty for the root commit of the repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_shas: Option<Vec<String>>,
}

/// A selectable verbose field of a listed commit — the [`CommitDetailDto`] set.
/// `list_history`'s `fields` names which of these to include per commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommitField {
    /// The full commit message (subject + body) — usually the largest field.
    Description,
    AuthorName,
    AuthorEmail,
    AuthorTime,
    CommitterName,
    CommitterEmail,
    CommitterTime,
    /// The parent shas.
    Parents,
}

/// A branch or tag decoration on a commit.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RefDto {
    pub name: String,
    /// `branch` or `tag`.
    pub kind: String,
    /// True for the checked-out branch (the one being edited). Omitted when
    /// false (the default).
    #[serde(skip_serializing_if = "is_false")]
    pub current: bool,
}

/// One file's change within a commit.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FileChangeDto {
    /// Path relative to the repository root, forward-slash form.
    pub path: String,
    /// `added`, `modified` or `removed`.
    pub kind: String,
    /// Non-UTF-8 content on either side; no diff or text is provided. Omitted
    /// when false (the default).
    #[serde(skip_serializing_if = "is_false")]
    pub is_binary: bool,
    /// Merge-commit path whose parents disagree: shown as-is, not editable.
    /// Omitted when false (the default).
    #[serde(skip_serializing_if = "is_false")]
    pub conflicted_base: bool,
    /// Unified diff of the change (absent for binary files). Capped at a
    /// per-file line limit; when it was cut, `truncated` is set and `total_lines`
    /// gives the full count. Re-read a specific file in full via show_commit's
    /// `paths` (it caps per file, so a large single file is still capped —
    /// inspect its `hunks` for structure, or use include_contents for a side).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// The `diff` was cut at the per-file line cap. Omitted when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// The diff's full line count, present only when `truncated`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub total_lines: usize,
    /// The diff's hunks, numbered for partial selection (absent for binary files
    /// or a file with no textual change). To commit only some of a file's
    /// uncommitted hunks, pass these `index` values in `commit_working_copy.hunks`
    /// — do NOT count `@@` markers yourself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<HunkDto>>,
    /// Full content before the commit, when requested and text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    /// Full content after the commit, when requested and text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
}

/// One hunk of a text file's diff, numbered so a partial commit_working_copy can
/// select it by `index` without counting `@@` markers.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct HunkDto {
    /// 0-based position of this hunk within the file's diff. Pass it in
    /// `commit_working_copy.hunks[].hunks` to commit exactly this hunk.
    pub index: usize,
    /// The hunk's `@@ -a,b +c,d @@` header line, for orientation only.
    pub header: String,
}

/// One entry of uncommitted changes (the working copy), shown above history.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkingCopyEntryDto {
    /// Commit id backing this entry. It churns on every disk edit — do not
    /// store it; `show_commit` accepts it for reading the uncommitted diff.
    pub sha: String,
    /// Stable change id of this entry.
    pub change_id: String,
    /// Number of files changed relative to the branch tip.
    pub changed_files: usize,
    /// The changed files' paths.
    pub files: Vec<String>,
    /// True when a rewrite clashed with these uncommitted changes and the
    /// entry is conflicted (resolve or abort via the conflict tools). Omitted
    /// when false (the default).
    #[serde(skip_serializing_if = "is_false")]
    pub has_conflict: bool,
}

/// A commit left conflicted by a rewrite, awaiting resolution.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConflictedCommitDto {
    /// Address `read_conflict`/`resolve_conflicts` with THIS id — commit shas
    /// churn on every resolution step, change ids don't.
    pub change_id: String,
    /// Current commit id (informational; changes per resolution step).
    pub sha: String,
    pub subject: String,
    /// The conflicted paths of this commit.
    pub files: Vec<ConflictedPathDto>,
}

/// One conflicted path within a conflicted commit.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConflictedPathDto {
    pub path: String,
    /// True when the conflict is plain file content, resolvable by editing
    /// text. False for structural conflicts (file-vs-directory, symlink,
    /// binary…) — those cannot be resolved here; `abort_rewrite` is the only
    /// way out.
    pub resolvable: bool,
}

/// One recorded session operation (an undo point).
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OpEntryDto {
    /// 1-based position for `jump_to_operation`; 0 is the session start.
    pub index: usize,
    /// What the operation did, e.g. `Edit message of "subject"`.
    pub label: String,
    /// Change ids the operation touched.
    pub affected_change_ids: Vec<String>,
}

/// Mark a tagged-enum schema as an object at the root: every variant
/// serializes to an object, and the MCP spec requires `outputSchema` (and any
/// type rmcp embeds in one) to carry a root `"type": "object"`, which schemars
/// omits on its `oneOf` rendering.
fn tagged_enum_is_an_object(schema: &mut schemars::Schema) {
    schema
        .ensure_object()
        .insert("type".into(), "object".into());
}

/// How a topology-changing rewrite reshaped the graph, so the result can be
/// verified without a follow-up read. Present on reorder/restore/squash/split/
/// drop/create/revert/cherry_pick (and squash_working_copy); absent on plain
/// message/identity/file edits, whose shape is unchanged.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TopologyDto {
    /// The rewritten commit(s) and where they landed: the moved/created/restored
    /// commit, the squash destination, a split's commit and its new fixup child,
    /// or the parent a drop's children rebased onto — each with its new parents
    /// AND children by change_id.
    pub affected: Vec<AdjacencyDto>,
    /// Set only when the new branch tip is a merge — the shape a linear history
    /// can't show: its parents, by change_id. Null for an ordinary single-parent
    /// tip (and omitted when the tip already appears in `affected`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_tip: Option<AdjacencyDto>,
}

/// One commit's place in the graph after a rewrite.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AdjacencyDto {
    /// The commit, by stable change_id — pass it straight back to chain edits.
    pub change_id: String,
    pub subject: String,
    /// Its parents, by change_id (empty for the repository's root commit).
    pub parents: Vec<String>,
    /// The commits rebased directly on top of it, by change_id (empty at the tip).
    pub children: Vec<String>,
}

/// Outcome of a mutation: either the rewrite is clean and exported to git, or
/// it is held back with conflicts to resolve.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
#[schemars(transform = tagged_enum_is_an_object)]
pub enum SaveResultDto {
    /// The rewrite landed: git refs, HEAD and the working tree are updated.
    Clean {
        /// The new branch tip.
        head_sha: Option<String>,
        /// How the rewrite reshaped the graph — present on topology-changing
        /// mutations, absent on plain message/identity/file edits. Verify the
        /// result here instead of a follow-up list_history.
        #[serde(skip_serializing_if = "Option::is_none")]
        topology: Option<TopologyDto>,
    },
    /// The rewrite is held back in full — git is untouched — until every
    /// conflict is resolved (`resolve_conflicts`) or the rewrite is aborted.
    Conflicts {
        /// The conflicted commits, oldest first. Resolve in this order.
        commits: Vec<ConflictedCommitDto>,
        /// How to proceed from here.
        guidance: String,
    },
}

// ---------------------------------------------------------------------------
// Session addressing

/// The session a tool operates on. The server hosts several independent editing
/// sessions over one repository, so every session-operating tool names which one
/// — there is no implicit default. Flattened into the request DTOs that already
/// carry arguments, and used standalone as the whole request of the otherwise
/// argument-less tools.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SessionSel {
    /// Session to operate on: the branch short-name it edits (its id from
    /// list_sessions/open_session, e.g. `main`), or `HEAD` for a detached/unborn
    /// HEAD. Stable across rewrites. Required on every session-operating tool.
    pub session: String,
}

// ---------------------------------------------------------------------------
// Requests / responses per tool

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ListHistoryReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Maximum number of commits to return, newest first. Omit for the default
    /// (30). Raise it (or page with `offset`) to see deeper history.
    pub limit: Option<usize>,
    /// 0-based index of the first commit to return, newest first. Omit to start
    /// at HEAD. Pass the previous response's `next_offset` to get the next page.
    pub offset: Option<usize>,
    /// Which verbose fields to include per commit, on top of the always-present
    /// header (sha, change_id, subject, is_merge, refs). Omit for ALL of them
    /// (full detail); pass an explicit subset to save tokens — e.g.
    /// `["author_time", "committer_time"]` when re-dating, `["description"]` to
    /// scan messages, or `[]` for a header-only overview. Selectable:
    /// description, author_name, author_email, author_time, committer_name,
    /// committer_email, committer_time, parents.
    pub fields: Option<Vec<CommitField>>,
    /// Set true to also include the uncommitted-changes status in `working_copy`
    /// (same content as the working_copy_status tool), saving a round-trip when
    /// you need both at once — e.g. before folding the working copy into a commit.
    /// Omit (or false) to skip it; the field is then null.
    pub working_copy: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListHistoryResp {
    /// The branch tip, or null on a detached/unborn HEAD.
    pub head_sha: Option<String>,
    /// Ancestors of HEAD, newest first (like `git log`). Shas and change_ids are
    /// abbreviated to the shortest repo-unique prefix (>= 8 chars) — pass them
    /// straight back to any tool as a commit ref.
    pub commits: Vec<CommitDto>,
    /// True when the limit cut the walk short — more commits remain below.
    pub has_more: bool,
    /// The offset this page started at. Omitted when 0 (the default, an
    /// unpaged listing starting at HEAD).
    #[serde(skip_serializing_if = "is_zero")]
    pub offset: usize,
    /// Offset to pass next to continue paging. Omitted at the end of history
    /// (paging done); its absence mirrors `has_more: false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// Number of dropped commits currently in the session trash. Omitted when 0
    /// (the default, an empty trash).
    #[serde(skip_serializing_if = "is_zero")]
    pub trash_count: usize,
    /// The uncommitted-changes status, present only when the request set
    /// `working_copy: true` (else null) — the same payload as working_copy_status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_copy: Option<WorkingCopyStatusResp>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ShowCommitReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Commit to show — change_id or sha (full/unique prefix >= 4 chars), from
    /// the history, the working copy (an uncommitted entry) or the trash.
    pub commit: String,
    /// Restrict the returned files to these repo-relative paths (forward-slash
    /// form); omit for every changed file. Use it to re-read one file of a large
    /// commit without pulling the whole diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// Also return each text file's full old/new content, not just the diff.
    pub include_contents: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShowCommitResp {
    pub commit: CommitDto,
    /// The files the commit changes, relative to its parent.
    pub files: Vec<FileChangeDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListTrashResp {
    /// Dropped commits, restorable via `restore_commit` or `squash_commit`.
    pub commits: Vec<CommitDto>,
}

/// The whole branch as a graph — the standalone read of the same shape a
/// topology-changing mutation folds into its result.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ShowGraphResp {
    /// The branch tip, or null on a detached/unborn HEAD.
    pub head_sha: Option<String>,
    /// Every commit reachable from HEAD (newest first), each with its parents
    /// and children by change_id — the merge/branch structure at a glance. The
    /// repository's root has no parents; a tip has no children.
    pub commits: Vec<AdjacencyDto>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SuggestSquashReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The commit you intend to fold, from the history or the trash — sha or
    /// change id, full or a unique prefix. Its leading `fixup!`/`squash!`/`amend!`
    /// subject token names the target whose matching branch commit(s) are
    /// suggested as the squash destination.
    pub source: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SuggestSquashResp {
    /// The squash mode the source's subject prefix requests (`fixup`, `squash` or
    /// `amend`), or null when it carries no autosquash prefix — in which case
    /// there is nothing to suggest and both lists below are empty.
    pub mode: Option<String>,
    /// The recommended destination(s): branch commits whose subject is the
    /// source's bare target subject (the prefix stripped), newest first. Usually
    /// exactly one — pass its change_id straight back as `squash_commit`'s `dest`.
    pub targets: Vec<CommitDto>,
    /// Other autosquash-prefixed branch commits aimed at the same target, which
    /// you may want to fold in as well.
    pub siblings: Vec<CommitDto>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct BlameSquashReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The change to find a squash target for — sha or change id, full or a
    /// unique prefix, from the history or the working copy (an uncommitted
    /// entry). Omit to blame the working copy (all uncommitted changes) — the
    /// default and primary case.
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BlameSquashResp {
    /// The autosquash mode the source's subject prefix requests (`fixup`,
    /// `squash`, `amend`), set only for a history-commit source carrying such a
    /// prefix (the working copy has no subject); else null. Independent of the
    /// candidates, which are derived from content, not the subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Candidate squash destinations, most-owned first: the branch commits that
    /// introduced the lines the source removes/modifies, each with how many of
    /// those lines it owns. Pass the top candidate's change_id straight to
    /// squash_commit as `dest` (or to squash_working_copy for the default
    /// working-copy source). Empty when nothing could be attributed.
    pub candidates: Vec<BlameCandidateDto>,
    /// Changed lines that couldn't be attributed to a listed commit — they trace
    /// to a merge / history boundary, or to an ancestor outside the history.
    /// Omitted when 0.
    #[serde(skip_serializing_if = "is_zero")]
    pub unattributed: usize,
}

/// A ranked squash-target candidate: a commit plus how many of the source's
/// changed lines it introduced.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BlameCandidateDto {
    #[serde(flatten)]
    pub commit: CommitDto,
    /// How many of the source's removed/modified lines this commit introduced —
    /// the ranking weight, highest first.
    pub lines: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkingCopyStatusResp {
    /// True when the working tree matches the branch tip.
    pub clean: bool,
    /// Uncommitted-change entries, newest first (normally at most one).
    pub entries: Vec<WorkingCopyEntryDto>,
    /// Git HEAD as of session start (what `jump_to_operation 0` restores).
    pub session_start_head_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionDiffResp {
    /// Everything that changed since the session started — the combined
    /// content delta of all edits this session, including uncommitted ones.
    pub files: Vec<FileChangeDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListOperationsResp {
    /// Recorded operations, oldest first.
    pub ops: Vec<OpEntryDto>,
    /// Current position: 0 = session start, `ops.len()` = latest state.
    pub cursor: usize,
    pub can_undo: bool,
    pub can_redo: bool,
    /// True while a conflicted rewrite is held pending resolution.
    pub pending: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PendingStatusResp {
    /// True while a conflicted rewrite is held pending resolution.
    pub pending: bool,
    /// The branch tip as git sees it (pre-rewrite while pending).
    pub git_head_sha: Option<String>,
    /// The not-yet-exported tip of the held rewrite (differs while pending).
    pub jj_head_sha: Option<String>,
    /// The commits still conflicted, oldest first.
    pub conflicts: Vec<ConflictedCommitDto>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditMessageReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Commit to edit — change_id (stable, preferred) or sha; full or a unique
    /// prefix (>= 4 chars), case-insensitive.
    pub commit: String,
    /// The new full commit message (subject line + body). Stored verbatim and
    /// not reflowed — wrap the body at ~72 columns; keep the subject one line.
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditIdentityReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Commit to edit — change_id (stable, preferred) or sha; full or a unique
    /// prefix (>= 4 chars), case-insensitive.
    pub commit: String,
    /// New author/committer fields; omitted fields keep their current value.
    #[serde(flatten)]
    pub identity: IdentityFieldsDto,
}

/// One commit's edit within an `edit_commits` batch. At least one of `message`
/// or an identity field must be set; omitted identity fields keep their current
/// value. Like `edit_identity`, the committer timestamp is pinned, not re-stamped.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CommitEditDto {
    /// Commit to edit — change_id (stable, preferred) or sha; full or a unique
    /// prefix (>= 4 chars), case-insensitive.
    pub commit: String,
    /// New full commit message (subject + body). Omit to leave it. Stored
    /// verbatim, not reflowed — wrap the body at ~72 columns.
    pub message: Option<String>,
    /// New author/committer fields; omitted fields keep their current value.
    #[serde(flatten)]
    pub identity: IdentityFieldsDto,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditCommitsReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The per-commit edits, applied together in ONE transaction with a single
    /// rebase. Prefer this over many edit_identity/edit_message calls for bulk
    /// changes: it's atomic (a conflict holds the whole batch back) and avoids
    /// re-stamping committers across the cascade. A commit may appear only once.
    pub edits: Vec<CommitEditDto>,
}

/// A whole-file replacement within a commit.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FileContentDto {
    /// Path relative to the repository root, forward-slash form.
    pub path: String,
    /// The file's complete new content.
    pub content: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReplaceFilesReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Commit to edit — change_id (stable, preferred) or sha; full or a unique
    /// prefix (>= 4 chars), case-insensitive.
    pub commit: String,
    /// Files to write, each with its complete new content (a path the commit
    /// doesn't have yet is added).
    pub files: Vec<FileContentDto>,
    /// Paths to delete from the commit (a path the commit doesn't have is
    /// ignored). At least one of `files`/`delete_paths` must be non-empty.
    pub delete_paths: Option<Vec<String>>,
}

/// One targeted text replacement within a file (no patch format, no line
/// numbers): find `old`, substitute `new`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct StrReplaceDto {
    /// Path relative to the repository root, forward-slash form.
    pub path: String,
    /// The exact text to find. Must occur exactly once in the file's current
    /// content unless `replace_all` is set — include enough surrounding text
    /// to make it unique. The untouched content is never resent, so it can't
    /// drift.
    pub old: String,
    /// The text to substitute in.
    pub new: String,
    /// Replace every occurrence instead of requiring a unique match.
    pub replace_all: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReplaceInFileReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Commit to edit — change_id (stable, preferred) or sha; full or a unique
    /// prefix (>= 4 chars), case-insensitive.
    pub commit: String,
    /// The replacements, applied in order; several may target the same file.
    pub edits: Vec<StrReplaceDto>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReplaceInMessageReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Commit whose message to edit — change_id (stable, preferred) or sha;
    /// full or a unique prefix (>= 4 chars), case-insensitive.
    pub commit: String,
    /// The exact text to find in the message. Must occur exactly once unless
    /// `replace_all` is set.
    pub old: String,
    /// The text to substitute in.
    pub new: String,
    /// Replace every occurrence instead of requiring a unique match.
    pub replace_all: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SplitCommitReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Commit to split — change_id or sha (full/unique prefix >= 4 chars).
    pub commit: String,
    /// Whole-file content this commit should KEEP, per changed path (spliced
    /// onto the original tree, like `replace_files`); a new `fixup!` child gets
    /// the remainder. To move a file's change OUT to the child, pass it at its
    /// PARENT (pre-commit) content; an omitted changed file stays here. Content
    /// leaving the tree unchanged (an empty child) is refused.
    pub files: Vec<FileContentDto>,
}

/// Optional author/committer fields (name, email, date). An *omitted* field:
/// a new-commit tool (create/revert/cherry_pick/commit_working_copy) fills it
/// from the git-configured identity at "now"; an edit tool
/// (edit_identity/edit_commits) keeps the commit's current value.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct IdentityFieldsDto {
    pub author_name: Option<String>,
    pub author_email: Option<String>,
    /// `YYYY-MM-DD HH:MM:SS ±HHMM` or RFC 3339.
    pub author_time: Option<String>,
    pub committer_name: Option<String>,
    pub committer_email: Option<String>,
    /// `YYYY-MM-DD HH:MM:SS ±HHMM` or RFC 3339.
    pub committer_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CreateCommitReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The full commit message (subject line + optional body). Stored verbatim
    /// and not reflowed — wrap the body at ~72 columns; keep the subject one line.
    pub message: String,
    /// Files to put in the commit, each with its complete content, spliced onto
    /// the parent's tree. Omit (with no `delete_paths`) for an empty commit.
    #[serde(default)]
    pub files: Vec<FileContentDto>,
    /// Paths to delete relative to the parent (a path the parent lacks is
    /// ignored).
    pub delete_paths: Option<Vec<String>>,
    /// The commit that becomes the new commit's parent — sha or change id, full
    /// or a unique prefix, or the literal `root` for the very first position.
    /// Omitted means the top of HEAD: the new commit becomes the branch tip
    /// (uncommitted changes, if any, ride on top of it untouched).
    pub new_parent: Option<String>,
    /// When several lines converge on `new_parent` (a fork), the child the new
    /// commit should be spliced above (same ref forms), as in `reorder_commit`.
    pub child: Option<String>,
    #[serde(flatten)]
    pub identity: IdentityFieldsDto,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RevertCommitReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The commit to revert — sha or change id, full or a unique prefix
    /// (>= 4 chars), case-insensitive. Merge commits cannot be reverted.
    pub commit: String,
    /// Where to place the revert commit — the commit that becomes its parent
    /// (same ref forms) or `root`. Omitted means the top of HEAD.
    pub new_parent: Option<String>,
    /// Disambiguates a fork, as in `reorder_commit`.
    pub child: Option<String>,
    #[serde(flatten)]
    pub identity: IdentityFieldsDto,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CherryPickCommitReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The commit to cherry-pick. A commit in the current branch history takes
    /// a sha or change id, full or a unique prefix (>= 4 chars). A commit from
    /// *outside* the history (e.g. on another branch) takes its full 40-char
    /// sha — get it from `git log <branch>`; a prefix or change id only resolves
    /// within the history. Merge commits cannot be cherry-picked.
    pub commit: String,
    /// Where to place the new commit — the commit that becomes its parent (sha
    /// or change id, or `root`). Omitted means the top of HEAD.
    pub new_parent: Option<String>,
    /// Disambiguates a fork, as in `reorder_commit`.
    pub child: Option<String>,
    #[serde(flatten)]
    pub identity: IdentityFieldsDto,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DropCommitReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Commit to drop — change_id or sha (full/unique prefix >= 4 chars).
    pub commit: String,
    /// When true, "uncommit" instead of trashing: the commit leaves history and
    /// its diff becomes *unstaged* changes in the working tree (git's
    /// `reset --mixed`), rather than moving to the session trash. The returned
    /// `working_copy` then reflects the new uncommitted state. Merge commits and
    /// the branch's only commit still cannot be dropped. Defaults to false.
    #[serde(default)]
    pub keep_changes: bool,
}

/// The result of `drop_commit`. The mutation outcome's `status` and fields are
/// flattened to the top level — uniform with every other mutation's bare
/// `SaveResultDto` — and `dropped` rides alongside as an extra sibling.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DropCommitResp {
    /// The mutation outcome, flattened in: `status` (`clean`/`conflicts`) and
    /// its fields sit at this object's top level, exactly as the other
    /// mutations return them.
    #[serde(flatten)]
    pub result: SaveResultDto,
    /// The dropped commit. Without `keep_changes` it is now in the session trash
    /// (its `parent_shas` say where it sat — useful when restoring it later); with
    /// `keep_changes` it left history entirely and its diff is now uncommitted.
    pub dropped: CommitDto,
    /// Present only on a clean `keep_changes` drop: the resulting uncommitted
    /// state, with the dropped commit's diff now among the working-copy entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_copy: Option<WorkingCopyStatusResp>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReorderCommitReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Commit to move — change_id or sha (full/unique prefix >= 4 chars).
    pub commit: String,
    /// The commit that should become its parent (same ref forms), or the
    /// literal string `root` to make it the repository's first commit.
    pub new_parent: String,
    /// When several lines converge on the new parent (a fork), the child the
    /// moved commit should be spliced under (same ref forms). Usually
    /// omitted; an ambiguous move fails listing the choices.
    pub child: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RestoreCommitReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The trashed commit to graft back (see `list_trash`) — change_id or sha
    /// (full/unique prefix >= 4 chars).
    pub commit: String,
    /// The commit that should become its parent (same ref forms), or `root`.
    pub new_parent: String,
    /// Disambiguates a fork, as in `reorder_commit`.
    pub child: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MergeOutReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The single-parent commit C to merge out — sha or change id, full or a
    /// unique prefix (>= 4 chars), case-insensitive. A merge M is introduced
    /// directly above it; merge commits and the repository root (which have no
    /// single parent) cannot be merged out.
    pub commit: String,
    /// When several lines converge above C (a fork), the child line the merge
    /// should take over (same ref forms), as in `reorder_commit`. Usually
    /// omitted.
    pub child: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SquashCommitReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The commit to fold, from the history or the trash — sha or change id,
    /// full or a unique prefix. A ref present in both resolves to the
    /// history commit.
    pub source: String,
    /// The commit to fold it into (same ref forms).
    pub dest: String,
    /// `fixup` (keep destination's message), `squash` (append source's body)
    /// or `amend` (replace with source's body). Defaults to what the source's
    /// `fixup!`/`squash!`/`amend!` subject prefix requests, else `fixup`. Ignored
    /// when `message` is given.
    pub mode: Option<String>,
    /// Optional: the destination's full message after the fold, set verbatim.
    /// Overrides `mode`'s message handling — use it to fold and reword in one
    /// call instead of a follow-up edit_message. Omit to let `mode` decide.
    /// Stored verbatim and not reflowed — wrap the body at ~72 columns.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SquashWorkingCopyReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The commit the uncommitted changes should be folded into — sha or
    /// change id, full or a unique prefix.
    pub dest: String,
    /// Optional: the destination's full message after the fold, set verbatim.
    /// The fold is a fixup (the destination's message is kept by default, since
    /// uncommitted changes carry no message of their own); set this to reword the
    /// destination in the same call instead of a follow-up edit_message. Stored
    /// verbatim and not reflowed — wrap the body at ~72 columns.
    pub message: Option<String>,
    /// Optional partial fold — fold only PART of the changes (in-process `git
    /// add -p`), leaving the rest uncommitted; omit all three for the whole
    /// working copy. A file may appear in one tier only. `paths`: whole files
    /// (content + mode; the only tier for binary/exec; an untracked file also
    /// needs `add_paths`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// `hunks`: whole hunks per file by their show_commit indices; unlisted hunks
    /// stay. Text files only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<HunkSelectionDto>>,
    /// `patches`: a unified-diff patch applied to the file's HEAD content, to
    /// split finer than a whole hunk. Text files only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patches: Option<Vec<PatchSelectionDto>>,
    /// `add_paths`: brand-new untracked files to fold in (naming begins tracking
    /// past `.gitignore`); also list them under `paths` for a partial fold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add_paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CommitWorkingCopyReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The full commit message (subject line + optional body) for the new commit
    /// holding the committed changes. Stored verbatim and not reflowed — wrap the
    /// body at ~72 columns; keep the subject one line.
    pub message: String,
    #[serde(flatten)]
    pub identity: IdentityFieldsDto,
    /// Optional partial selection — commit only PART of the changes (in-process
    /// `git add -p`), leaving the rest uncommitted; omit all three for the whole
    /// working copy. A file may appear in one tier only. `paths`: whole files
    /// (content + mode; the only tier for binary/exec; an untracked file also
    /// needs `add_paths`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// `hunks`: whole hunks per file by their show_commit indices; unlisted hunks
    /// stay. Text files only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<HunkSelectionDto>>,
    /// `patches`: a unified-diff patch applied to the file's HEAD content, to
    /// split finer than a whole hunk. Text files only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patches: Option<Vec<PatchSelectionDto>>,
    /// `add_paths`: brand-new untracked files to commit (naming begins tracking
    /// past `.gitignore`); also list them under `paths` for a partial commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add_paths: Option<Vec<String>>,
}

/// The result of `commit_working_copy`. The mutation outcome's `status` and
/// fields are flattened to the top level (uniform with every other mutation);
/// the new commit and the remaining working copy ride alongside as siblings.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CommitWorkingCopyResp {
    /// The mutation outcome, flattened in: `status` (always `clean` here — a fresh
    /// commit on HEAD has no descendants to conflict) and its fields sit at this
    /// object's top level.
    #[serde(flatten)]
    pub result: SaveResultDto,
    /// The freshly committed commit on HEAD — its sha and stable change_id, so you
    /// can chain a follow-up edit without a list_history. Present on a clean commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub committed: Option<CommitDto>,
    /// The uncommitted changes that remain after the commit: clean for a whole
    /// commit, the unselected remainder for a partial one — so a partial commit is
    /// verifiable (what landed + what's left) without a follow-up read. Present on
    /// a clean commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_copy: Option<WorkingCopyStatusResp>,
}

/// The result of `squash_working_copy`. The mutation outcome (including its
/// `topology` slice) is flattened to the top level; the remaining working copy
/// rides alongside.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SquashWorkingCopyResp {
    /// The mutation outcome, flattened in: `status`, `head_sha` and (on a clean
    /// fold) the `topology` slice showing the destination after the fold.
    #[serde(flatten)]
    pub result: SaveResultDto,
    /// The uncommitted changes that remain after the fold: clean for a whole fold,
    /// the unselected remainder for a partial one. Present on a clean fold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_copy: Option<WorkingCopyStatusResp>,
}

/// Selects whole hunks of one file for a partial commit_working_copy /
/// squash_working_copy.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HunkSelectionDto {
    /// Repo-relative path of the file (forward-slash form).
    pub path: String,
    /// 0-based hunk indices to select, taken from the file's `hunks` in
    /// show_commit. Must list at least one.
    pub hunks: Vec<usize>,
}

/// Selects a sub-hunk slice of one file for a partial commit_working_copy /
/// squash_working_copy.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PatchSelectionDto {
    /// Repo-relative path of the file (forward-slash form).
    pub path: String,
    /// A unified-diff patch — the `@@ … @@` hunk(s), no `diff --git`/`---`/`+++`
    /// header needed — to apply to the file's content at HEAD. Context and `-`
    /// lines must match HEAD exactly or the commit is rejected.
    pub patch: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CarveWorkingCopyReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The commits to carve out of the uncommitted changes, **oldest-first** —
    /// each is stacked on the previous one on top of HEAD, holding only its own
    /// selection. Whatever no commit selects stays uncommitted. Every selection
    /// addresses the *same* working-copy diff you already read (hunk indices from
    /// show_commit on the working-copy entry), so they don't shift between
    /// commits — the reason this beats several commit_working_copy calls.
    pub commits: Vec<CarveCommitDto>,
    /// Brand-new untracked files to include (invisible until named; naming begins
    /// tracking past `.gitignore`). Name them here, then select them under a
    /// commit's `paths`. Tracked/absent paths are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_paths: Option<Vec<String>>,
}

/// One commit in a `carve_working_copy` request: its message, optional identity,
/// and the partial selection of the working copy it holds. The `paths`/`hunks`/
/// `patches` tiers work exactly as in commit_working_copy; across the whole carve
/// a path may be split by `hunks` (disjoint indices) but a whole-file/`patches`
/// selection of a path must be unique.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CarveCommitDto {
    /// The full commit message (subject + optional body), stored verbatim.
    pub message: String,
    #[serde(flatten)]
    pub identity: IdentityFieldsDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<HunkSelectionDto>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patches: Option<Vec<PatchSelectionDto>>,
}

/// The result of `carve_working_copy`. The mutation outcome (always `clean` — a
/// stack of fresh commits on HEAD has no descendants to conflict) is flattened in;
/// the new commits and the remaining working copy ride alongside.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CarveWorkingCopyResp {
    #[serde(flatten)]
    pub result: SaveResultDto,
    /// The commits created, **oldest-first** — each with its sha and stable
    /// change_id, ready to chain further edits without a list_history.
    pub committed: Vec<CommitDto>,
    /// The uncommitted changes left after the carve (the unselected remainder).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_copy: Option<WorkingCopyStatusResp>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AbsorbWorkingCopyReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Restrict the absorb to these files (repo-relative, forward-slash form).
    /// Omit to consider every changed file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// Preview only: return the routing plan (which hunks would fold where, what
    /// is skipped, what would stay uncommitted) without changing anything.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dry_run: bool,
}

/// The result of `absorb_working_copy`, for both the dry-run preview and the
/// applied rewrite.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AbsorbWorkingCopyResp {
    /// True when this was a preview: nothing was changed.
    pub dry_run: bool,
    /// Per destination commit, ancestors-first, the hunks that route into it.
    /// Empty when nothing could be attributed to a single commit.
    pub plan: Vec<AbsorbPlanEntryDto>,
    /// Paths skipped wholesale, each with jj's reason (binary, symlink, a
    /// conflict, a submodule) — those can't be absorbed as text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<AbsorbSkipDto>,
    /// Whether any uncommitted change remains after the absorb (an ambiguous or
    /// unattributable hunk that stays in the working copy).
    pub remaining: bool,
    /// The mutation outcome — present only when applied (absent on a dry run, or
    /// when nothing was attributable so no rewrite ran). `status: conflicts` here
    /// means the fold couldn't merge cleanly and is held back like any rewrite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<SaveResultDto>,
    /// The uncommitted changes left after a clean apply (the unattributed
    /// remainder). Present only when applied and clean.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_copy: Option<WorkingCopyStatusResp>,
}

/// One destination commit in an absorb plan.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AbsorbPlanEntryDto {
    /// The target commit's stable change id — survives the rewrite, so it stays a
    /// valid ref after applying.
    pub change_id: String,
    /// Its current sha (churns on the rewrite).
    pub sha: String,
    pub subject: String,
    /// The changed files whose hunks route to this target.
    pub files: Vec<AbsorbFileStatDto>,
}

/// One file's routed hunks within an [`AbsorbPlanEntryDto`].
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AbsorbFileStatDto {
    pub path: String,
    /// Lines the routed hunks add to the target.
    pub added: usize,
    /// Lines the routed hunks remove from the target.
    pub removed: usize,
    /// Number of contiguous hunks routed here for this file.
    pub hunks: usize,
}

/// A path absorb left untouched, with why.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AbsorbSkipDto {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DiscardWorkingCopyReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Must be true. Discarded uncommitted changes cannot be recovered
    /// through this server — undo steps over the discard but restores only
    /// previously recorded states, which never contain them.
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OkResp {
    pub ok: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReadConflictReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The conflicted commit (from the mutation's `conflicts` response or
    /// `pending_status`) — change id or current sha, full or a unique
    /// prefix. Prefer the change id: shas churn on every resolution step.
    pub commit: String,
    /// A single conflicted path to read. Combine with `paths`, or omit both to
    /// read every resolvable file of the commit at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Several conflicted paths to read in one call (in addition to `path`).
    /// Omit `path` and `paths` both to read every resolvable file at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReadConflictResp {
    /// One entry per file read, in request order (or all resolvable files when
    /// neither `path` nor `paths` was given).
    pub files: Vec<ConflictFileContentDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ConflictFileContentDto {
    /// The conflicted path.
    pub path: String,
    /// The file with git-style conflict markers (`<<<<<<<`/`=======`/
    /// `>>>>>>>`). Resolve by producing the file without any markers.
    pub text: String,
    /// Echo this back to `resolve_conflicts` for this file.
    pub marker_len: usize,
    /// Number of conflicting sides (normally 2).
    pub num_sides: usize,
}

/// One resolved file for `resolve_conflicts`. Pick exactly one of three
/// mutually-exclusive modes: full resolved content (`text` + `marker_len`),
/// targeted patch edits against the conflict-marker text (`edits`), or a
/// deletion (`delete`).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ConflictFileEditDto {
    /// The conflicted path being resolved.
    pub path: String,
    /// The file's complete resolved content, all conflict markers removed.
    /// Required to resolve with full content unless `delete` or `edits` is
    /// given.
    pub text: Option<String>,
    /// The `marker_len` `read_conflict` returned for this file. Required
    /// alongside `text`; omit for `edits` (a fresh one is derived) or `delete`.
    pub marker_len: Option<usize>,
    /// Resolve by patching the conflict-marker text `read_conflict` returned:
    /// each edit finds `old` and substitutes `new` (see `ConflictPatchEditDto`),
    /// composed in order. Cheaper than resending the whole file and can't
    /// corrupt untouched content — prefer it over `text` for anything but a
    /// tiny file. Mutually exclusive with `text` and `delete`.
    pub edits: Option<Vec<ConflictPatchEditDto>>,
    /// Resolve by deleting the path instead of supplying content — the way to
    /// settle a modify/delete conflict (e.g. a revert that should remove a
    /// file). Works for structural (resolvable=false) conflicts too, so it is
    /// also the text route's escape hatch besides abort_rewrite. Mutually
    /// exclusive with `text` and `edits`.
    pub delete: Option<bool>,
}

/// One targeted `old`→`new` edit applied to a conflicted file's materialized
/// conflict-marker text (what `read_conflict` returned). Pathless on purpose:
/// the path is the outer `ConflictFileEditDto.path`, so repeating it per edit
/// would be redundant.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ConflictPatchEditDto {
    /// The exact text to find in the conflict-marked content — typically the
    /// whole `<<<<<<< … ======= … >>>>>>>` block, replaced with the chosen
    /// resolution. Must occur exactly once unless `replace_all` is set —
    /// include enough surrounding text to make it unique.
    pub old: String,
    /// The text to substitute in.
    pub new: String,
    /// Replace every occurrence instead of requiring a unique match.
    pub replace_all: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ResolveConflictsReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// The conflicted commit being resolved — change id or current sha, full
    /// or a unique prefix. Prefer the change id: shas churn on every
    /// resolution step.
    pub commit: String,
    /// The resolved files (any subset of the commit's conflicted files).
    pub files: Vec<ConflictFileEditDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AbortResp {
    pub ok: bool,
    /// The branch tip after the rollback (the pre-rewrite history).
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct JumpToOperationReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Target position: 0 = session start, the `index` of an entry from
    /// `list_operations` = the state right after that operation.
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TimeTravelResp {
    /// The branch tip at the restored state.
    pub head_sha: Option<String>,
    /// The new cursor position (0 = session start).
    pub cursor: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReloadRepoReq {
    #[serde(flatten)]
    pub session: SessionSel,
    /// Optional path to re-home this session to a *different worktree of the
    /// same repository* — its main checkout or any linked worktree (they share
    /// a git common dir). Omit to reload the current repository in place. A path
    /// outside this repository's worktrees is refused.
    pub path: Option<String>,
    /// Optional branch to edit, which need NOT be checked out: reopens the
    /// session editing this branch's history, moving only its ref and leaving
    /// HEAD/index/worktree frozen (so working-copy tools are then unavailable).
    /// Omit to keep editing the current branch (or, when re-homing via `path`,
    /// the branch checked out in that worktree). Refused if the branch doesn't
    /// exist or is checked out in another worktree. NOTE: switching the branch
    /// re-keys the session — its id becomes the new branch's short-name (returned
    /// in `session`); refused if a session for that branch is already open.
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReloadResp {
    /// The session id after the reload — its branch short-name (or `HEAD`). It
    /// changes when `branch` switched the edited branch; pass this id to later
    /// tools, not the one you reloaded.
    pub session: String,
    /// The branch tip after the fresh import.
    pub head_sha: Option<String>,
    /// The repository root the session is now pointed at.
    pub root: String,
    /// The branch whose history the session now edits (its short name), or null
    /// on a detached HEAD with no branch selected.
    pub branch: Option<String>,
    /// Whether the edited branch is the one checked out in the worktree. `false`
    /// means an off-worktree session: only the branch ref moves, there is no
    /// working copy, and working-copy tools are refused.
    pub worktree_bound: bool,
}

// ---------------------------------------------------------------------------
// Session registry tools

/// One open editing session in the registry. The server hosts several at once,
/// each over a distinct branch of the one repository it launched against.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionInfoDto {
    /// The session id: the short name of the branch this session edits (or the
    /// reserved `HEAD` for a detached/unborn-HEAD session). Pass it as the
    /// `session` selector on every session-operating tool.
    pub session: String,
    /// The worktree this session is anchored at (absolute path).
    pub root: String,
    /// The branch whose history this session edits (short name), or null on a
    /// detached HEAD. Equal to `session` except for the reserved `HEAD` id.
    pub branch: Option<String>,
    /// Whether the edited branch is the one checked out at `root`. `false` is an
    /// off-worktree session: only the branch ref moves and working-copy tools are
    /// refused (there is no working copy).
    pub worktree_bound: bool,
    /// The branch tip this session currently sits on, or null on a detached/unborn
    /// HEAD.
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ListSessionsResp {
    /// Every open session, by id. Use a session's id as the `session` selector on
    /// the other tools. Never empty — the launch session can't be closed away.
    pub sessions: Vec<SessionInfoDto>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct OpenSessionReq {
    /// The branch to open an editing session over (short name or full
    /// `refs/heads/…`). git's branch→worktree mapping decides the anchor: a
    /// branch checked out in a worktree opens worktree-bound there (live working
    /// copy); a branch checked out nowhere opens off-worktree (only its ref moves,
    /// no working copy). The branch must exist; a branch already open as a session,
    /// or checked out in a worktree commedit can't bind, is refused.
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct OpenSessionResp {
    /// The id of the new session (the branch short-name) — its `session` selector.
    pub session: String,
    /// Whether it opened worktree-bound (live working copy) or off-worktree.
    pub worktree_bound: bool,
    /// The branch tip the new session sits on.
    pub head_sha: Option<String>,
    /// The full session list after opening, for orientation.
    pub sessions: Vec<SessionInfoDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CloseSessionResp {
    /// The id of the session that was closed.
    pub closed: String,
    /// The remaining open sessions after closing.
    pub sessions: Vec<SessionInfoDto>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_detail() -> CommitDetailDto {
        CommitDetailDto {
            description: None,
            author_name: None,
            author_email: None,
            author_time: None,
            committer_name: None,
            committer_email: None,
            committer_time: None,
            parent_shas: None,
        }
    }

    #[test]
    fn default_header_flags_are_omitted_from_a_listed_commit() {
        // A plain non-merge, undecorated commit reduces to sha/change_id/subject
        // (plus selected detail) — is_merge: false and refs: [] never reach the
        // wire, since their absence is documented to mean exactly that.
        let plain = CommitDto {
            sha: "abc123".into(),
            change_id: "zzzz".into(),
            subject: "ordinary".into(),
            is_merge: false,
            refs: Vec::new(),
            detail: empty_detail(),
        };
        let v = serde_json::to_value(&plain).unwrap();
        assert!(v.get("is_merge").is_none(), "is_merge omitted when false");
        assert!(v.get("refs").is_none(), "refs omitted when empty");

        // A merge with a current branch decoration carries both — and the ref's
        // `current` flag, which is likewise only present when true.
        let decorated = CommitDto {
            is_merge: true,
            refs: vec![RefDto {
                name: "main".into(),
                kind: "branch".into(),
                current: true,
            }],
            ..plain
        };
        let v = serde_json::to_value(&decorated).unwrap();
        assert_eq!(v["is_merge"], true);
        assert_eq!(v["refs"][0]["name"], "main");
        assert_eq!(v["refs"][0]["current"], true);
    }

    #[test]
    fn drop_commit_resp_flattens_status_to_the_top_level() {
        // drop_commit must report its outcome like every other mutation —
        // `status` at the top level — with `dropped` as an extra sibling, not a
        // result nested under `result`.
        let resp = DropCommitResp {
            result: SaveResultDto::Clean {
                head_sha: Some("abc123".into()),
                topology: None,
            },
            dropped: CommitDto {
                sha: "def456".into(),
                change_id: "zzzz".into(),
                subject: "dropped one".into(),
                is_merge: false,
                refs: Vec::new(),
                detail: empty_detail(),
            },
            working_copy: None,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["status"], "clean");
        assert_eq!(v["head_sha"], "abc123");
        assert!(
            v.get("result").is_none(),
            "status must not stay nested under `result`"
        );
        assert_eq!(v["dropped"]["sha"], "def456");
        assert!(
            v.get("working_copy").is_none(),
            "working_copy is omitted unless a keep_changes drop populates it"
        );
    }
}
