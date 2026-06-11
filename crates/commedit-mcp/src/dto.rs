//! Request and response types of the tool surface. No jj-lib type crosses
//! this boundary — everything is plain strings and flags.
//!
//! Doc comments on fields become JSON-schema descriptions, so they are written
//! as documentation for the calling agent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Shared response shapes

/// One commit of the current branch's history.
///
/// The identifying header — sha, change_id, subject, is_merge, refs — is always
/// present. The verbose [`CommitDetailDto`] fields (message body, identity,
/// parents) are flattened in alongside; each appears only when `list_history`'s
/// `fields` selects it (all of them by default, none for a header-only
/// overview). `show_commit` and `list_trash` always include them all.
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
    /// Merge commits cannot be reordered, dropped, split or used as a squash
    /// source (squashing *into* one is fine).
    pub is_merge: bool,
    /// Local branches and tags pointing at this commit.
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
    /// True for the checked-out branch (the one being edited).
    pub current: bool,
}

/// One file's change within a commit.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FileChangeDto {
    /// Path relative to the repository root, forward-slash form.
    pub path: String,
    /// `added`, `modified` or `removed`.
    pub kind: String,
    /// Non-UTF-8 content on either side; no diff or text is provided.
    pub is_binary: bool,
    /// Merge-commit path whose parents disagree: shown as-is, not editable.
    pub conflicted_base: bool,
    /// Unified diff of the change (absent for binary files).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
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
    /// entry is conflicted (resolve or abort via the conflict tools).
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
    schema.ensure_object().insert("type".into(), "object".into());
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
// Requests / responses per tool

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ListHistoryReq {
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
    /// The offset this page started at (0 unless paged).
    pub offset: usize,
    /// Offset to pass next to continue paging, or null at the end of history.
    pub next_offset: Option<usize>,
    /// Number of dropped commits currently in the session trash.
    pub trash_count: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ShowCommitReq {
    /// The commit to show — sha or change id, full or a unique prefix
    /// (>= 4 chars), case-insensitive — from the history, the working copy
    /// (an uncommitted entry) or the trash.
    pub commit: String,
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
    /// The commit to edit — sha or change id, full or a unique prefix
    /// (>= 4 chars), case-insensitive. Change ids are stable across rewrites,
    /// so they chain across mutations without re-listing.
    pub commit: String,
    /// The new full commit message (subject line + body).
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EditIdentityReq {
    /// The commit to edit — sha or change id, full or a unique prefix
    /// (>= 4 chars), case-insensitive. Change ids are stable across rewrites,
    /// so they chain across mutations without re-listing.
    pub commit: String,
    /// New author name; omitted fields keep their current value.
    pub author_name: Option<String>,
    pub author_email: Option<String>,
    /// `YYYY-MM-DD HH:MM:SS ±HHMM` or RFC 3339.
    pub author_time: Option<String>,
    pub committer_name: Option<String>,
    pub committer_email: Option<String>,
    /// `YYYY-MM-DD HH:MM:SS ±HHMM` or RFC 3339.
    pub committer_time: Option<String>,
}

/// One commit's edit within an `edit_commits` batch. At least one of `message`
/// or an identity field must be set; omitted identity fields keep their current
/// value. Like `edit_identity`, the committer timestamp is pinned, not re-stamped.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CommitEditDto {
    /// The commit to edit — sha or change id, full or a unique prefix
    /// (>= 4 chars), case-insensitive. Change ids are stable across rewrites.
    pub commit: String,
    /// New full commit message (subject + body). Omit to leave the message.
    pub message: Option<String>,
    /// New author name; omitted fields keep their current value.
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
pub struct EditCommitsReq {
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
    /// The commit to edit — sha or change id, full or a unique prefix
    /// (>= 4 chars), case-insensitive. Change ids are stable across rewrites,
    /// so they chain across mutations without re-listing.
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
    /// The commit to edit — sha or change id, full or a unique prefix
    /// (>= 4 chars), case-insensitive. Change ids are stable across rewrites,
    /// so they chain across mutations without re-listing.
    pub commit: String,
    /// The replacements, applied in order; several may target the same file.
    pub edits: Vec<StrReplaceDto>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReplaceInMessageReq {
    /// The commit whose message to edit — sha or change id, full or a unique
    /// prefix (>= 4 chars), case-insensitive. Change ids are stable across
    /// rewrites.
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
    /// The commit to split — sha or change id, full or a unique prefix
    /// (>= 4 chars), case-insensitive.
    pub commit: String,
    /// The content the commit should keep, per file (like `replace_files`).
    /// A new `fixup!` child commit receives the remainder, so both combined
    /// reproduce the original change.
    pub files: Vec<FileContentDto>,
}

/// Optional author/committer overrides for a newly created commit. Every
/// omitted field defaults to the repository's git-configured identity at the
/// current time. Flattened into the create/revert/commit-working-copy requests.
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
    /// The full commit message (subject line + optional body).
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
    /// The commit to drop — sha or change id, full or a unique prefix
    /// (>= 4 chars), case-insensitive.
    pub commit: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DropCommitResp {
    pub result: SaveResultDto,
    /// The dropped commit, now in the session trash. Its `parent_shas` say
    /// where it sat — useful when restoring it later.
    pub dropped: CommitDto,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReorderCommitReq {
    /// The commit to move — sha or change id, full or a unique prefix
    /// (>= 4 chars), case-insensitive.
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
    /// The trashed commit to graft back (see `list_trash`) — sha or change
    /// id, full or a unique prefix (>= 4 chars), case-insensitive.
    pub commit: String,
    /// The commit that should become its parent (same ref forms), or `root`.
    pub new_parent: String,
    /// Disambiguates a fork, as in `reorder_commit`.
    pub child: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SquashCommitReq {
    /// The commit to fold, from the history or the trash — sha or change id,
    /// full or a unique prefix. A ref present in both resolves to the
    /// history commit.
    pub source: String,
    /// The commit to fold it into (same ref forms).
    pub dest: String,
    /// `fixup` (keep destination's message), `squash` (append source's body)
    /// or `amend` (replace with source's body). Defaults to what the source's
    /// `fixup!`/`squash!`/`amend!` subject prefix requests, else `fixup`.
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SquashWorkingCopyReq {
    /// The commit the uncommitted changes should be folded into — sha or
    /// change id, full or a unique prefix.
    pub dest: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CommitWorkingCopyReq {
    /// The full commit message (subject line + optional body) for the new commit
    /// holding the committed changes.
    pub message: String,
    #[serde(flatten)]
    pub identity: IdentityFieldsDto,
    /// Optional partial selection: commit only *part* of the uncommitted changes
    /// and leave the rest in the working tree (the in-process `git add -p`).
    /// Omit `paths`, `hunks` and `patches` entirely to commit the whole working
    /// copy (the default). The three tiers compose in one call, but a given file
    /// path must appear in at most one of them.
    ///
    /// Whole files to commit, by repo-relative path: the file is taken entirely
    /// (content + mode), and a path you deleted on disk commits the deletion.
    /// This is the only tier that handles binary or executable files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    /// Whole hunks to commit, per file. First read the file's numbered `hunks`
    /// from show_commit on the working-copy entry, then list the `index` values to
    /// keep; the unlisted hunks stay uncommitted. Text files only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<HunkSelectionDto>>,
    /// Sub-hunk selections, per file: an edited unified-diff patch (à la
    /// `git add -p` → `e`) applied to the file's content at HEAD. Use when one
    /// hunk must be split finer than whole-hunk granularity. Text files only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patches: Option<Vec<PatchSelectionDto>>,
}

/// Selects whole hunks of one file for a partial commit_working_copy.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct HunkSelectionDto {
    /// Repo-relative path of the file (forward-slash form).
    pub path: String,
    /// 0-based hunk indices to commit, taken from the file's `hunks` in
    /// show_commit. Must list at least one.
    pub hunks: Vec<usize>,
}

/// Selects a sub-hunk slice of one file for a partial commit_working_copy.
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
pub struct DiscardWorkingCopyReq {
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
    /// The conflicted commit (from the mutation's `conflicts` response or
    /// `pending_status`) — change id or current sha, full or a unique
    /// prefix. Prefer the change id: shas churn on every resolution step.
    pub commit: String,
    /// The conflicted path to read.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReadConflictResp {
    /// The file with git-style conflict markers (`<<<<<<<`/`=======`/
    /// `>>>>>>>`). Resolve by producing the file without any markers.
    pub text: String,
    /// Echo this back to `resolve_conflicts` for this file.
    pub marker_len: usize,
    /// Number of conflicting sides (normally 2).
    pub num_sides: usize,
}

/// One resolved file for `resolve_conflicts` — either edited content, or a
/// deletion.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ConflictFileEditDto {
    /// The conflicted path being resolved.
    pub path: String,
    /// The file's complete resolved content, all conflict markers removed.
    /// Required unless `delete` is true.
    pub text: Option<String>,
    /// The `marker_len` `read_conflict` returned for this file. Required
    /// alongside `text`; omit when deleting.
    pub marker_len: Option<usize>,
    /// Resolve by deleting the path instead of supplying content — the way to
    /// settle a modify/delete conflict (e.g. a revert that should remove a
    /// file). Works for structural (resolvable=false) conflicts too, so it is
    /// also the text route's escape hatch besides abort_rewrite. When true,
    /// text/marker_len are ignored.
    pub delete: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ResolveConflictsReq {
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

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReloadResp {
    /// The branch tip after the fresh import.
    pub head_sha: Option<String>,
}
