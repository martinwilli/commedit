//! Conflict detection and resolution.
//!
//! A rewrite/reorder/abandon can leave jj-lib's `rebase_descendants` with
//! commits whose trees are *conflicted*. jj happily writes such commits, but its
//! git backend would serialize them as `.jjconflict-*` subtrees — garbage in a
//! real git history. So instead of exporting a conflicted chain we hold it back:
//! the rewrite is committed to jj's op log, but git refs / HEAD / the working
//! tree are left untouched (plain `git` keeps seeing the original clean history,
//! exactly like the keep-ref residue we already tolerate). The user then resolves
//! each conflicted commit's files in the UI, and only once the whole ancestor
//! chain of the branch tip is conflict-free do we perform the deferred export.
//!
//! This module owns that state machine. The mutation methods in [`crate::rewrite`]
//! / [`crate::tree`] all funnel their tail through [`Repo::finish_mutation`].

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{bail, Context, Result};
use jj_lib::backend::{ChangeId, CommitId, CopyId, TreeValue};
use jj_lib::commit::Commit;
use jj_lib::conflicts::{
    choose_materialized_conflict_marker_len, materialize_merge_result_to_bytes,
    materialize_tree_value, resolve_file_executable, update_from_content, ConflictMarkerStyle,
    ConflictMaterializeOptions, MaterializedTreeValue,
};
use jj_lib::merge::{Merge, MergedTreeValue};
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::operation::Operation;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::transaction::Transaction;

use crate::repo::Repo;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    pollster::block_on(f)
}

/// The first line of `commit`'s description, quoted for an op-log label
/// (e.g. `"Add feature"`), or a placeholder when the message is empty.
pub(crate) fn op_subject(commit: &Commit) -> String {
    let subject = commit.description().lines().next().unwrap_or("").trim();
    if subject.is_empty() {
        "(no message)".to_string()
    } else {
        format!("\"{subject}\"")
    }
}

/// Direction in which [`Repo::transform_tree`] replays a commit's change.
#[derive(Clone, Copy)]
enum Dir {
    /// Remove the change (peel a commit off the tree above it).
    Peel,
    /// Apply the change forward (onto a new base).
    Forward,
}

/// Outcome of a mutation (or a resolution step): either the history is now
/// conflict-free and was exported to git, or one or more commits on the branch
/// tip's ancestor chain are conflicted and the rewrite is held pending in jj
/// while git stays untouched.
#[derive(Debug, Clone)]
pub enum SaveOutcome {
    /// Conflict-free: git refs, HEAD and the working tree were updated.
    Clean,
    /// Conflicted: nothing was exported. The engine now holds a pending
    /// resolution; drive it with [`Repo::read_conflict`] / [`Repo::resolve_conflict`]
    /// (or discard it with [`Repo::abort`]).
    Conflicts { commits: Vec<ConflictedCommit> },
}

/// A description of a session mutation, built at the call site and consumed by
/// [`Repo::record_op`] once the mutation lands clean. It is held on `Repo`
/// (`pending_op_desc`) while a conflicted rewrite is still being resolved, then
/// recorded when the chain finally goes clean. `label` is the human string the
/// "Edit history" dropdown shows; `affected` is the change-id hex(es) the op
/// touched, for the dropdown's hover-highlight of history rows.
#[derive(Clone)]
pub struct OpDescriptor {
    label: String,
    affected: Vec<String>,
}

impl OpDescriptor {
    pub(crate) fn new(label: String, affected: Vec<String>) -> Self {
        Self { label, affected }
    }
}

/// One recorded operation in this session's linear op-log — the unit the
/// "Edit history" time-travel dropdown steps through. Holds the jj [`Operation`]
/// to rewind to (cheap to clone — two `Arc`s) plus its display label and the
/// change-ids it touched.
#[derive(Clone)]
pub struct OpEntry {
    op: Operation,
    label: String,
    affected: Vec<String>,
}

impl OpEntry {
    /// The human-readable label shown in the "Edit history" dropdown.
    pub fn label(&self) -> &str {
        &self.label
    }
    /// Change-id hex(es) this op touched, for hover-highlighting history rows.
    pub fn affected(&self) -> &[String] {
        &self.affected
    }
}

/// A conflicted commit awaiting resolution.
#[derive(Debug, Clone)]
pub struct ConflictedCommit {
    /// Stable across the re-rewrites resolution causes — the UI keys on this.
    pub change_id: ChangeId,
    /// Current commit id; changes every time a resolution rebases this commit.
    pub commit_id: CommitId,
    pub subject: String,
    pub files: Vec<ConflictedPath>,
}

impl ConflictedCommit {
    pub fn change_id_hex(&self) -> String {
        self.change_id.hex()
    }
}

/// One conflicted path within a [`ConflictedCommit`].
#[derive(Debug, Clone)]
pub struct ConflictedPath {
    pub path: RepoPathBuf,
    /// Whether this is a plain file-content conflict that can be resolved by
    /// editing text. `false` for modify/delete-of-a-directory, symlink,
    /// submodule and other structural conflicts, which text editing can't fix
    /// (the only escape for those is [`Repo::abort`]).
    pub resolvable: bool,
}

impl ConflictedPath {
    pub fn path_str(&self) -> String {
        self.path.as_internal_file_string().to_string()
    }
}

/// A conflicted file materialized to Git-style conflict-marker text, ready to
/// show in the editor.
#[derive(Debug, Clone)]
pub struct ConflictedFile {
    /// The materialized content, with 2-way conflict markers
    /// (`<<<<<<<` / `=======` / `>>>>>>>`).
    pub text: String,
    /// The marker length jj used; [`Repo::resolve_conflict`] must echo it back so
    /// the resolved text parses against the same conflict shape.
    pub marker_len: usize,
    pub num_sides: usize,
}

/// How to resolve one conflicted path in [`Repo::resolve_conflicts_ext`].
#[derive(Debug, Clone)]
pub enum FileResolution {
    /// Replace the path with resolved file content — all conflict markers
    /// removed, `marker_len` echoing what [`Repo::read_conflict`] reported.
    Content { text: String, marker_len: usize },
    /// Remove the path from the commit. Resolves a modify/delete conflict (or
    /// any other conflict kind, structural included) by deleting the file —
    /// the one resolution that plain content cannot express.
    Delete,
}

/// Which spurious-conflict auto-resolution to attempt before falling back to
/// manual resolution. *Spurious* conflicts are adjacent-but-independent edits
/// that conflict under jj's symmetric 3-way merge yet leave a well-defined
/// result. See [`Repo::try_auto_resolve_spurious`].
pub(crate) enum SpuriousResolve {
    /// Don't auto-resolve — message/identity/file edits and split hand any
    /// conflict straight to manual resolution.
    Off,
    /// Reorder or squash: the net change set is preserved, so the post-mutation
    /// branch tip is conflict-free and identical to the original. Anchor the
    /// reconstruction on that clean tip, peeling intermediate commits off it.
    CleanTip,
    /// Drop: a commit's change was removed, so the post-drop tip may itself be
    /// conflicted (no clean anchor). Rebuild the conflicted range forward from the
    /// clean prefix instead — the surviving commits' original changes, re-applied
    /// in order, are well-defined.
    Drop,
    /// Restore: a commit's change was re-inserted; like [`Self::Drop`] the tip may
    /// be conflicted, so rebuild forward. Identifies the restored commit (an orphan
    /// that lingers in the store) so its original change can be re-applied even
    /// though it is absent from the pre-restore history.
    Restore { commit: CommitId },
}

/// One editable branch's spurious-conflict rebuild, computed read-only by
/// [`Repo::plan_spurious_head`] and applied in the shared transaction by
/// [`Repo::try_auto_resolve_spurious`]. `chain` is the branch's current
/// (conflicted) commit chain oldest-first; `[lo, n]` (with `n` the tip index) is
/// the conflicted range to rewrite with explicit trees; `trees[i]` holds the
/// reconstructed clean tree for each `i` in that range.
struct ChainRebuild {
    chain: Vec<Commit>,
    lo: usize,
    n: usize,
    trees: Vec<Option<MergedTree>>,
}

/// Outcome of planning one editable branch's spurious rebuild.
enum HeadPlan {
    /// The branch's chain is conflict-free — nothing to rebuild here, skip it.
    Clean,
    /// The branch can't be safely auto-resolved (a true conflict at the tip, a
    /// non-linear range, a missing original, a structural change): abandon the
    /// whole attempt and hand every conflict to manual resolution.
    Bail,
    /// The branch's conflicted range was reconstructed clean.
    Rebuild(ChainRebuild),
}

/// The held-back state of a rewrite whose chain is conflicted, carried across
/// the per-file resolution steps until the chain goes clean (then exported) or
/// the user aborts (then rolled back).
pub(crate) struct PendingResolution {
    /// jj operation to roll back to on abort — the view from before the rewrite.
    pre_op: Operation,
    /// git tip from before the op, for the eventual `sync_worktree`.
    old_head: Option<String>,
    /// op-log message of the originating mutation (kept for reference).
    #[allow(dead_code)]
    op_msg: String,
    /// Pre-rewrite git branch heads, for the export-time backstop
    /// (see [`Repo::protect_unrelated_heads`]).
    heads: BTreeMap<String, String>,
    /// Conflicted commits, oldest first; re-derived after every resolution.
    conflicts: Vec<ConflictedCommit>,
    /// Which spurious-conflict auto-resolution to attempt (one-shot) before
    /// falling back to manual resolution. See [`SpuriousResolve`].
    strategy: SpuriousResolve,
}

impl Repo {
    /// Whether a conflicted rewrite is currently held pending resolution.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The conflicted commits of the pending rewrite (oldest first), or `None`
    /// when nothing is pending.
    pub fn pending_conflicts(&self) -> Option<&[ConflictedCommit]> {
        self.pending.as_ref().map(|p| p.conflicts.as_slice())
    }

    /// The branch tip as jj currently sees it — the head of the (possibly
    /// conflicted, not-yet-exported) rewritten chain. While a resolution is
    /// pending, git's HEAD still points at the pre-rewrite tip, so the UI uses
    /// this to walk and display the *new* history being resolved.
    pub fn jj_head_commit_id(&self) -> Option<CommitId> {
        self.current_head_in_jj()
    }

    /// Every editable branch's tip as jj currently sees it (the post-rewrite,
    /// not-yet-exported bookmarks), primary first and deduped. The multi-head
    /// analogue of [`Self::jj_head_commit_id`], for the conflict view: conflict
    /// detection scans all these heads (see [`Self::collect_conflicts`]), so a UI
    /// walking only the primary tip cannot reach a conflict on a sibling branch.
    pub fn jj_editable_heads(&self) -> Vec<CommitId> {
        self.editable_heads_in_jj()
    }

    /// Commit the rewrite transaction, then either export to git (if the branch
    /// tip's ancestor chain is conflict-free) or hold the rewrite pending while
    /// the conflicts are resolved. Every mutation ends here in place of the old
    /// inline export tail.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_mutation(
        &mut self,
        tx: Transaction,
        op_msg: &str,
        desc: OpDescriptor,
        pre_op: Operation,
        old_head: Option<String>,
        heads: BTreeMap<String, String>,
    ) -> Result<SaveOutcome> {
        self.finish_mutation_inner(
            tx,
            op_msg,
            desc,
            pre_op,
            old_head,
            heads,
            SpuriousResolve::Off,
        )
    }

    /// Like [`Self::finish_mutation`] but, for a reorder or squash, opts the
    /// held-back chain into spurious-conflict auto-resolution anchored on the
    /// clean post-mutation tip (see [`SpuriousResolve::CleanTip`]).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_mutation_auto_resolve(
        &mut self,
        tx: Transaction,
        op_msg: &str,
        desc: OpDescriptor,
        pre_op: Operation,
        old_head: Option<String>,
        heads: BTreeMap<String, String>,
    ) -> Result<SaveOutcome> {
        self.finish_mutation_inner(
            tx,
            op_msg,
            desc,
            pre_op,
            old_head,
            heads,
            SpuriousResolve::CleanTip,
        )
    }

    /// Like [`Self::finish_mutation`] but opts into an explicit
    /// spurious-conflict auto-resolution strategy — used by drop/restore, whose
    /// post-mutation tip may itself be conflicted (see [`SpuriousResolve`]).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_mutation_spurious(
        &mut self,
        tx: Transaction,
        op_msg: &str,
        desc: OpDescriptor,
        pre_op: Operation,
        old_head: Option<String>,
        heads: BTreeMap<String, String>,
        strategy: SpuriousResolve,
    ) -> Result<SaveOutcome> {
        self.finish_mutation_inner(tx, op_msg, desc, pre_op, old_head, heads, strategy)
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_mutation_inner(
        &mut self,
        tx: Transaction,
        op_msg: &str,
        desc: OpDescriptor,
        pre_op: Operation,
        old_head: Option<String>,
        heads: BTreeMap<String, String>,
        strategy: SpuriousResolve,
    ) -> Result<SaveOutcome> {
        // If this rewrite leaves the branch bookmark conflicted, jj can't export
        // it to git, so the edit would silently never reach git. Refuse before
        // committing — the tx is dropped here, leaving jj untouched, rather than
        // piling up an unexportable divergent commit. Checked on the tx's own
        // post-rewrite view: reorder/restore resolved the bookmark (set the head
        // explicitly) and pass; a message/identity/squash/split edit can't, so a
        // diverged branch is caught here.
        self.ensure_branch_exportable(tx.repo())?;
        self.repo = block_on(tx.commit(op_msg)).context("committing rewrite")?;
        self.pending = Some(PendingResolution {
            pre_op,
            old_head,
            op_msg: op_msg.to_string(),
            heads,
            conflicts: Vec::new(),
            strategy,
        });
        // Remember the op's description until the chain settles clean; `settle`
        // records it as a session op-log entry once the deferred export runs
        // (so a conflicted-then-resolved mutation records exactly once, on the
        // final clean step — see `record_op`).
        self.pending_op_desc = Some(desc);
        self.settle()
    }

    /// Apply the user's edited conflict text for one `(change_id, path)`. A thin
    /// wrapper over [`Repo::resolve_conflicts`] for the single-file case.
    pub fn resolve_conflict(
        &mut self,
        change_hex: &str,
        path: &str,
        edited_text: &str,
        marker_len: usize,
    ) -> Result<SaveOutcome> {
        self.resolve_conflicts(
            change_hex,
            &[(path.to_string(), edited_text.to_string(), marker_len)],
        )
    }

    /// Apply edited conflict text for several files of the commit with change id
    /// `change_hex` at once — the content-only entry point (`(path, text,
    /// marker_len)`), kept for the GTK frontend and the single-file wrapper.
    /// Delegates to [`Repo::resolve_conflicts_ext`].
    pub fn resolve_conflicts(
        &mut self,
        change_hex: &str,
        files: &[(String, String, usize)],
    ) -> Result<SaveOutcome> {
        let files: Vec<(String, FileResolution)> = files
            .iter()
            .map(|(path, text, marker_len)| {
                (
                    path.clone(),
                    FileResolution::Content {
                        text: text.clone(),
                        marker_len: *marker_len,
                    },
                )
            })
            .collect();
        self.resolve_conflicts_ext(change_hex, &files)
    }

    /// Resolve several files of the commit with change id `change_hex`, each
    /// either by edited content or by deleting the path: splice every result
    /// into the commit's tree in one rewrite, rebase descendants, and re-settle
    /// the chain. Resolving a commit's conflicted paths together is sound because
    /// they are independent — no intermediate re-materialization is needed
    /// between them. Content on a structural (non-file) path is skipped; a
    /// deletion resolves any conflict kind. Returns the refreshed outcome —
    /// `Clean` once the last conflict is gone (the rewrite is exported at that
    /// point), otherwise the remaining `Conflicts`.
    pub fn resolve_conflicts_ext(
        &mut self,
        change_hex: &str,
        files: &[(String, FileResolution)],
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("resolving the conflict", || {
            self.resolve_conflicts_inner(change_hex, files)
        })
    }

    fn resolve_conflicts_inner(
        &mut self,
        change_hex: &str,
        files: &[(String, FileResolution)],
    ) -> Result<SaveOutcome> {
        if self.pending.is_none() {
            bail!("no conflict resolution in progress");
        }
        let store = self.repo.store().clone();
        let commit_id = self.resolve_change_on_chain(change_hex)?;
        let commit = store
            .get_commit(&commit_id)
            .context("loading conflicted commit")?;
        let tree = commit.tree();

        // Turn each file's resolution into a tree value up front (while `tree`
        // is still borrowable), then splice them all into one builder.
        let mut entries: Vec<(RepoPathBuf, MergedTreeValue)> = Vec::with_capacity(files.len());
        for (path, resolution) in files {
            let path: &RepoPath = RepoPath::from_internal_string(path).context("invalid path")?;
            let value = block_on(tree.path_value(path)).context("reading conflicted path")?;
            let merged_value: MergedTreeValue = match resolution {
                // An absent value removes the path — resolves a modify/delete
                // conflict (or any kind) by deleting the file.
                FileResolution::Delete => Merge::absent(),
                FileResolution::Content { text, marker_len } => {
                    let Some(file_ids) = value.to_file_merge() else {
                        continue; // structural conflict — not text-resolvable, leave it
                    };
                    let exec = value
                        .to_executable_merge()
                        .as_ref()
                        .and_then(resolve_file_executable)
                        .unwrap_or(false);

                    let new_ids = block_on(update_from_content(
                        &file_ids,
                        &store,
                        path,
                        text.as_bytes(),
                        *marker_len,
                    ))
                    .context("parsing resolved content")?;

                    // Lift the resolved/again-conflicted file ids back into a tree
                    // value, preserving the executable bit.
                    new_ids.map(|oid| {
                        oid.as_ref().map(|id| TreeValue::File {
                            id: id.clone(),
                            executable: exec,
                            copy_id: CopyId::placeholder(),
                        })
                    })
                }
            };
            entries.push((path.to_owned(), merged_value));
        }

        let mut builder = MergedTreeBuilder::new(tree);
        for (path, merged_value) in entries {
            builder.set_or_remove(path, merged_value);
        }
        let new_tree = block_on(builder.write_tree()).context("writing resolved tree")?;

        let mut tx = self.repo.start_transaction();
        block_on(
            tx.repo_mut()
                .rewrite_commit(&commit)
                .set_tree(new_tree)
                .write(),
        )
        .context("writing resolved commit")?;
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;
        self.repo =
            block_on(tx.commit("commedit: resolve conflict")).context("committing resolution")?;

        self.settle()
    }

    /// Run the deferred export now if the chain is already clean, otherwise
    /// report the conflicts that still remain. Normally the last
    /// [`Self::resolve_conflict`] settles automatically; this is the explicit
    /// hook for a UI that wants to drive finalization itself.
    pub fn finalize(&mut self) -> Result<SaveOutcome> {
        if self.pending.is_none() {
            return Ok(SaveOutcome::Clean);
        }
        self.settle()
    }

    /// Discard a pending conflicted rewrite, rolling jj's view back to the
    /// operation before it. Git was never touched while pending, so the original
    /// history is intact; the conflicted commit objects are left as unreachable
    /// garbage (like keep-ref residue).
    ///
    /// The rollback is *recorded* as a new operation that restores the
    /// pre-rewrite view, rather than merely reloading the in-memory view at
    /// `pre_op`. A bare `reload_at` never advances the op log, so the discarded
    /// conflicted operation would linger as a second op head; the next edit then
    /// forks off the restored op, leaving two divergent heads that a later
    /// load-at-head merges straight back into the abandoned rewrite (the "old jj
    /// state" resurfacing). Committing a restore op makes the clean state the
    /// single head, mirroring jj's own `undo`/`op restore`.
    pub fn abort(&mut self) -> Result<()> {
        if let Some(p) = self.pending.take() {
            let view = block_on(p.pre_op.view()).context("reading the pre-rewrite view")?;
            let mut tx = self.repo.start_transaction();
            tx.repo_mut().set_view(view.store_view().clone());
            self.repo =
                block_on(tx.commit("commedit: abort rewrite")).context("recording the abort")?;
        }
        Ok(())
    }

    /// Roll the entire session back to its starting point — the in-app
    /// equivalent of `git reset --hard <session head>`. A thin wrapper over
    /// [`Self::set_op_cursor`]`(0)`: cursor index 0 is the session-start floor,
    /// so reverting is just time-travelling to the very first snapshot. Kept as a
    /// named method because the engine tests and the session-review flow lean on
    /// its exact semantics (restore the original commits *and* working copy).
    pub fn revert_all(&mut self) -> Result<()> {
        crate::repo::catch_jj("reverting the session", || {
            self.set_op_cursor(0).map(|_| ())
        })
    }

    /// Whether there is a recorded session op to step back to (`op_cursor > 0`).
    pub fn can_undo(&self) -> bool {
        self.op_cursor > 0
    }

    /// Whether there is a recorded session op ahead of the cursor to step
    /// forward to (`op_cursor < session_ops.len()`).
    pub fn can_redo(&self) -> bool {
        self.op_cursor < self.session_ops.len()
    }

    /// The recorded session operations, oldest first — the snapshots the
    /// "Edit history" dropdown lists. The session-start state (index 0 / the
    /// dropdown's floor) is *not* in this list; it is the implicit floor below
    /// the first entry, reached with [`Self::jump_to_op`]`(0)`.
    pub fn session_ops(&self) -> &[OpEntry] {
        &self.session_ops
    }

    /// The live cursor over [`Self::session_ops`]: `0` is the session-start
    /// floor, `session_ops.len()` is the latest recorded state.
    pub fn op_cursor(&self) -> usize {
        self.op_cursor
    }

    /// Step back one recorded operation (a no-op at the session-start floor).
    pub fn undo(&mut self) -> Result<SaveOutcome> {
        crate::repo::catch_jj("undoing", || {
            if self.op_cursor == 0 {
                return Ok(SaveOutcome::Clean);
            }
            self.set_op_cursor(self.op_cursor - 1)
        })
    }

    /// Step forward one recorded operation (a no-op at the latest state).
    pub fn redo(&mut self) -> Result<SaveOutcome> {
        crate::repo::catch_jj("redoing", || {
            if self.op_cursor >= self.session_ops.len() {
                return Ok(SaveOutcome::Clean);
            }
            self.set_op_cursor(self.op_cursor + 1)
        })
    }

    /// Travel the repository to a recorded session snapshot: `target == 0` is the
    /// session-start floor, `target == session_ops().len()` the latest state. The
    /// surface behind the "Edit history" dropdown.
    pub fn jump_to_op(&mut self, target: usize) -> Result<SaveOutcome> {
        crate::repo::catch_jj("jumping to a recorded state", || {
            if target > self.session_ops.len() {
                bail!("op-log target out of range");
            }
            self.set_op_cursor(target)
        })
    }

    /// Move the session op-cursor to `target` and materialize that state to
    /// git/disk. The shared core of [`Self::undo`]/[`Self::redo`]/
    /// [`Self::jump_to_op`]/[`Self::revert_all`].
    ///
    /// Snapshots the working copy first, so any on-disk edits not yet captured
    /// survive in jj's op log; the jump itself then replaces the working tree
    /// with the target snapshot's content (`git reset --hard`-style — uncommitted
    /// changes made since the target are reset, but remain recoverable via jj's
    /// op log). Drops any held conflicted rewrite (you cannot step the timeline
    /// mid-resolution). Every recorded op was a clean, exported state, so the
    /// rewind always lands [`SaveOutcome::Clean`].
    fn set_op_cursor(&mut self, target: usize) -> Result<SaveOutcome> {
        if self.pending.is_none() {
            // No held conflict: capture any on-disk edits into @ first, so they
            // are preserved in jj's op log even though the rewind below resets the
            // working tree. (When a conflicted rewrite is held, git/disk are still
            // the pre-rewrite state and the whole rewrite is about to be
            // discarded, so there is nothing new to snapshot.)
            self.snapshot_working_copy()?;
        }
        // Drop any held-back conflicted rewrite and its pending description; git
        // was never touched for it.
        self.pending = None;
        self.pending_op_desc = None;
        let op = if target == 0 {
            match &self.session_op {
                Some(op) => op.clone(),
                // The (unreachable) window before `open` finishes capturing it.
                None => return Ok(SaveOutcome::Clean),
            }
        } else {
            self.session_ops[target - 1].op.clone()
        };
        self.rewind_to_op(op)?;
        self.op_cursor = target;
        Ok(SaveOutcome::Clean)
    }

    /// Rewind jj's view to a previously-recorded operation and re-export it to
    /// git/disk — the generalized core of the old `revert_all`. Drops any held
    /// rewrite, then restores the target view and reconciles git with it.
    ///
    /// Like [`Self::abort`], the restore is *recorded* as a new operation rather
    /// than a bare reload (see that method's note on why a divergent op head
    /// would otherwise resurface the old state). Clean saves during the session
    /// already moved git refs / HEAD / the worktree, so the restored state must
    /// be exported and materialized back to disk — hence the `export_and_sync`
    /// tail.
    fn rewind_to_op(&mut self, op: Operation) -> Result<()> {
        // Drop any held-back conflicted rewrite; git was never touched for it.
        self.pending = None;
        // The export tail needs the *current* (rewritten) on-disk state to sync
        // away from; the git-level head backstop holds unrelated branches in place.
        let old_head = self.edited_tip();
        let heads = self.snapshot_heads();
        // Snapshot each extra worktree's current (pre-rewind) `@` id, keyed by full
        // ref name like `heads`. The rewind below restores jj's view — and each
        // worktree's `@` — to the target op, but `export_and_sync`'s extra-worktree
        // tail only re-checks-out a worktree whose branch *tip* moved. An `@`-only
        // sibling op (edit/discard) moves the sibling `@` without moving its tip, so
        // without this its files would stay stale on disk after undo/redo. We
        // re-materialize the `@`-changed worktrees ourselves after the export (the
        // rewind owns this pre-state, so it is not threaded through
        // `export_and_sync`, which has a tip-only caller in `settle` too).
        let before_wc: BTreeMap<String, CommitId> = self
            .extra_worktrees
            .iter()
            .filter_map(|v| {
                self.repo
                    .view()
                    .get_wc_commit_id(&v.name)
                    .cloned()
                    .map(|id| (v.branch.clone(), id))
            })
            .collect();
        // jj's recorded git-ref state tracks what it last wrote to git's
        // refs/*; the session's clean saves left it at the current tips. Keep a
        // copy sampled from the *live* view (so chained undo/redo reconciles in
        // either direction): `set_view` below rewinds this record to the target
        // op's values, but git's actual on-disk refs are still at the current
        // tips, so the export would see no diff and push nothing. We re-stamp
        // these afterwards so the export reconciles git with reality.
        let on_disk_git_refs: Vec<_> = self
            .repo
            .view()
            .git_refs()
            .iter()
            .map(|(name, target)| (name.clone(), target.clone()))
            .collect();
        // Restore the target view and record it as a new operation.
        let view = block_on(op.view()).context("reading the target view")?;
        let mut tx = self.repo.start_transaction();
        tx.repo_mut().set_view(view.store_view().clone());
        // Re-point the recorded git refs at what git actually holds on disk, so
        // the deferred export detects bookmark(target) != git-ref(current) and
        // pushes the target tips back to git.
        for (name, target) in &on_disk_git_refs {
            tx.repo_mut().set_git_ref_target(name, target.clone());
        }
        self.repo = block_on(tx.commit("commedit: time-travel to a recorded state"))
            .context("recording the time-travel")?;
        // Push the restored state back to git and check its working copy back out
        // to disk. Every recorded op was a clean exported git history, so the
        // restored chain is always conflict-free.
        self.export_and_sync(old_head, &heads)?;
        // Re-materialize any sibling worktree whose `@` changed but whose tip did
        // not move (the tip-moved ones were already handled inside the export).
        self.materialize_changed_worktrees(&heads, &before_wc)
    }

    /// Append a landed mutation to the session op-log the time-travel dropdown
    /// steps through. Truncates any redo tail first (a fresh edit after a
    /// back-jump makes the redo branch unreachable — standard undo-stack
    /// semantics; the orphaned commits are pruned by the normal keep-ref path),
    /// then records the current op (the clean, git-exported state) and advances
    /// the cursor to the tip.
    pub(crate) fn record_op(&mut self, desc: OpDescriptor) {
        if self.op_cursor < self.session_ops.len() {
            self.session_ops.truncate(self.op_cursor);
        }
        self.session_ops.push(OpEntry {
            op: self.repo.operation().clone(),
            label: desc.label,
            affected: desc.affected,
        });
        self.op_cursor = self.session_ops.len();
    }

    /// Build an [`OpDescriptor`] for a mutation acting on a single commit
    /// `target`: an `<action> "<subject>"` label and the commit's change id (for
    /// the dropdown's hover-highlight). Falls back to a bare label if the commit
    /// can't be loaded.
    pub(crate) fn op_desc_for(&self, action: &str, target: &CommitId) -> OpDescriptor {
        match self.repo.store().get_commit(target) {
            Ok(commit) => OpDescriptor::new(
                format!("{action} {}", op_subject(&commit)),
                vec![commit.change_id().hex()],
            ),
            Err(_) => OpDescriptor::new(action.to_string(), Vec::new()),
        }
    }

    /// Build an [`OpDescriptor`] for a mutation acting on several commits at once
    /// (the multi-select drag operations): an `<action> N commit(s)` label and the
    /// change ids of all `targets`, for the dropdown's hover-highlight. A single
    /// target reads as the singular-commit label (the `<action> "<subject>"`
    /// shape) so it matches [`Self::op_desc_for`].
    pub(crate) fn op_desc_for_many(&self, action: &str, targets: &[CommitId]) -> OpDescriptor {
        if let [only] = targets {
            return self.op_desc_for(action, only);
        }
        let affected = targets
            .iter()
            .filter_map(|id| self.repo.store().get_commit(id).ok())
            .map(|c| c.change_id().hex())
            .collect();
        OpDescriptor::new(format!("{action} {} commits", targets.len()), affected)
    }

    /// Materialize one conflicted file of the commit with change id `change_id`
    /// to Git-style 2-way conflict-marker text, for display in the editor.
    pub fn read_conflict(&self, change_hex: &str, path: &str) -> Result<ConflictedFile> {
        let path: &RepoPath = RepoPath::from_internal_string(path).context("invalid path")?;
        let store = self.repo.store();
        let commit_id = self.resolve_change_on_chain(change_hex)?;
        let commit = store.get_commit(&commit_id).context("loading commit")?;
        let tree = commit.tree();
        let value = block_on(tree.path_value(path)).context("reading conflicted path")?;
        let mat = block_on(materialize_tree_value(store, path, value, tree.labels()))
            .context("materializing conflict")?;
        match mat {
            MaterializedTreeValue::FileConflict(fc) => {
                let marker_len = choose_materialized_conflict_marker_len(&fc.contents);
                let opts = ConflictMaterializeOptions {
                    marker_style: ConflictMarkerStyle::Git,
                    marker_len: Some(marker_len),
                    merge: store.merge_options().clone(),
                };
                let bytes = materialize_merge_result_to_bytes(&fc.contents, &fc.labels, &opts);
                let text = String::from_utf8(bytes.to_vec())
                    .context("conflicted file is not valid UTF-8")?;
                let text = strip_base_sections(&text, marker_len);
                let text = simplify_marker_labels(&text, marker_len);
                Ok(ConflictedFile {
                    text,
                    marker_len,
                    num_sides: fc.ids.num_sides(),
                })
            }
            MaterializedTreeValue::OtherConflict { .. } => {
                bail!("this conflict can't be resolved as text (structural conflict)")
            }
            _ => bail!("path is not conflicted"),
        }
    }

    /// Resolve `change_hex` to the commit carrying it among the very sources
    /// conflict *detection* scans — every editable worktree's `@`
    /// ([`Self::all_worktree_chain_ids`]: the launch `@` chain plus each extra
    /// worktree's `@`) and every editable branch's rewritten range
    /// ([`Self::editable_heads_in_jj`]). Mirroring [`Self::collect_conflicts`] keeps
    /// `read_conflict`/`resolve_conflicts` in agreement with what is displayed, so a
    /// conflict shown on a *sibling* branch or a *sibling* worktree's `@` is
    /// resolvable, not just the primary's. Scoping the lookup to these chains —
    /// rather than the store-wide `resolve_change_id` — disambiguates change ids
    /// with divergent siblings left over from earlier operations, which would
    /// otherwise make the global resolver bail as ambiguous. A singleton editable
    /// set with only the launch `@` chain reproduces the old primary-only behaviour.
    fn resolve_change_on_chain(&self, change_hex: &str) -> Result<CommitId> {
        let change_id = ChangeId::try_from_hex(change_hex).context("invalid change id")?;
        // Working copies first: they sit above the tips, so the ancestor walks below
        // never reach them. Launch `@` chain, then each extra worktree's `@`.
        for wc_id in self.all_worktree_chain_ids() {
            if let Ok(commit) = self.repo.store().get_commit(&wc_id) {
                if commit.change_id() == &change_id {
                    return Ok(wc_id);
                }
            }
        }
        // Then every editable branch's rewritten range (primary-first), de-duped so
        // a shared ancestor rewritten on several branches is checked once. Only
        // rewritten commits can be conflicted, so each branch walks just that range.
        let mut seen = HashSet::new();
        for head in self.editable_heads_in_jj() {
            for info in self.rewritten_history(&head)? {
                if !seen.insert(info.id.clone()) {
                    continue;
                }
                if info.change_id == change_id {
                    return Ok(info.id);
                }
            }
        }
        bail!("change {change_hex} is not on any editable branch chain or worktree")
    }

    /// The branch history that conflict detection must scan: the range rewritten
    /// since the pending rewrite's pre-rewrite tip ([`crate::history::history_range`]),
    /// newest first. Only rewritten commits can be conflicted — the untouched
    /// ancestors below the rewrite stay clean — so this skips them, which matters on
    /// a deep history where the full ancestry is thousands of commits. Falls back to
    /// the full [`crate::history::history`] walk when there is no pending base (e.g.
    /// a detached HEAD, or the rare unrelated-tip case).
    fn rewritten_history(&self, head: &CommitId) -> Result<Vec<crate::history::CommitInfo>> {
        match self.pending.as_ref().and_then(|p| p.old_head.as_deref()) {
            Some(base_hex) => match CommitId::try_from_hex(base_hex) {
                Some(base) => crate::history::history_range(&self.repo, &base, head),
                None => crate::history::history(&self.repo, head),
            },
            None => crate::history::history(&self.repo, head),
        }
    }

    /// The branch tip as jj currently sees it (read from the checked-out
    /// bookmark). `None` on a detached HEAD, where there is no branch to scope a
    /// conflict walk to.
    pub(crate) fn current_head_in_jj(&self) -> Option<CommitId> {
        let name = self.current_bookmark()?;
        self.repo
            .view()
            .get_local_bookmark(&name)
            .as_normal()
            .cloned()
    }

    /// Each editable branch's tip as jj currently sees it (its local bookmark),
    /// primary first and deduped — the *post-rewrite* tips, read before the
    /// deferred git export, so a sibling branch's freshly-rewritten (possibly
    /// conflicted) chain is visible even while its git ref is still frozen. Empty
    /// on a detached HEAD with no bookmark. The multi-head analogue of
    /// [`Self::current_head_in_jj`], used by [`Self::collect_conflicts`].
    fn editable_heads_in_jj(&self) -> Vec<CommitId> {
        let view = self.repo.view();
        let mut heads = Vec::new();
        for short in self.editable_branches() {
            let name: jj_lib::ref_name::RefNameBuf = short.as_str().into();
            if let Some(id) = view.get_local_bookmark(&name).as_normal() {
                if !heads.contains(id) {
                    heads.push(id.clone());
                }
            }
        }
        heads
    }

    /// Build a [`ConflictedCommit`] for the commit at `id` when its tree is
    /// conflicted (else `None`), labelled `subject`. Shared by the per-branch
    /// chain walk and the working-copy `@` checks in [`Self::collect_conflicts`].
    fn conflicted_commit_at(
        &self,
        id: &CommitId,
        subject: &str,
    ) -> Result<Option<ConflictedCommit>> {
        let commit = self.repo.store().get_commit(id).context("loading commit")?;
        if !commit.has_conflict() {
            return Ok(None);
        }
        let tree = commit.tree();
        let mut files = Vec::new();
        for (path, value) in tree.conflicts() {
            let value = value.context("reading conflict entry")?;
            files.push(ConflictedPath {
                path,
                resolvable: value.to_file_merge().is_some(),
            });
        }
        Ok(Some(ConflictedCommit {
            change_id: commit.change_id().clone(),
            commit_id: id.clone(),
            subject: subject.to_string(),
            files,
        }))
    }

    /// Walk **every** editable branch's rewritten range (oldest first) plus
    /// **every** worktree's working copy, collecting the commits whose trees are
    /// conflicted. Only rewritten commits can be conflicted, so each branch scans
    /// [`Self::rewritten_history`] rather than its whole — possibly huge —
    /// ancestry; a shared ancestor rewritten on several branches is reported once
    /// (`seen`). The working copies — the launch `@` chain and every *extra*
    /// worktree's `@` chain (see [`crate::repo::WorktreeView`]) — are descendants
    /// of their tips, so the ancestor walks never reach them; they are appended,
    /// oldest first, so an overlap between any worktree's uncommitted changes and
    /// the rewrite defers the export and is resolved in the diff pane like any
    /// other commit. This is the multi-branch generalization of the old
    /// primary-chain-plus-launch-`@` scan: a singleton editable set with no extra
    /// worktrees reproduces it exactly. Empty on a detached HEAD with no bookmark
    /// (nothing to scope the walk to), matching the previous behaviour.
    fn collect_conflicts(&self) -> Result<Vec<ConflictedCommit>> {
        let heads = self.editable_heads_in_jj();
        if heads.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for head in &heads {
            for info in self.rewritten_history(head)?.iter().rev() {
                if !seen.insert(info.id.clone()) {
                    continue;
                }
                if let Some(cc) = self.conflicted_commit_at(&info.id, &info.subject)? {
                    out.push(cc);
                }
            }
        }
        // The launch `@` chain (newest-first → reverse to oldest-first), then each
        // extra worktree's `@` chain (likewise reversed to oldest-first) — a split
        // sibling chain contributes every entry, not just its leaf.
        let mut wc_ids: Vec<CommitId> = self.working_copy_chain_ids();
        wc_ids.reverse();
        for view in &self.extra_worktrees {
            wc_ids.extend(self.worktree_chain_ids(view).into_iter().rev());
        }
        for wc_id in wc_ids {
            if !seen.insert(wc_id.clone()) {
                continue;
            }
            if let Some(cc) = self.conflicted_commit_at(&wc_id, "Uncommitted changes")? {
                out.push(cc);
            }
        }
        Ok(out)
    }

    /// After committing a rewrite/resolution, decide whether the chain is clean
    /// (export and clear pending) or still conflicted (refresh pending).
    fn settle(&mut self) -> Result<SaveOutcome> {
        let conflicts = self.collect_conflicts()?;
        if conflicts.is_empty() {
            let p = self
                .pending
                .take()
                .expect("settle requires a pending resolution");
            self.export_and_sync(p.old_head, &p.heads)?;
            // The mutation landed clean and is now in git: record it as a
            // session op-log entry the "Edit history" dropdown can travel back to.
            if let Some(desc) = self.pending_op_desc.take() {
                self.record_op(desc);
            }
            return Ok(SaveOutcome::Clean);
        }
        // A reorder/squash/drop/restore whose conflicts are merely *spurious*
        // (adjacent-but-independent edits) has a well-defined result. Try to
        // rebuild the chain clean before handing the conflicts to the user.
        // Attempt at most once: take the strategy (leaving `Off`) so a failed
        // attempt — or the recursive settle below — falls straight through to
        // manual resolution.
        let strategy = self
            .pending
            .as_mut()
            .map(|p| std::mem::replace(&mut p.strategy, SpuriousResolve::Off))
            .unwrap_or(SpuriousResolve::Off);
        if !matches!(strategy, SpuriousResolve::Off) && self.try_auto_resolve_spurious(strategy)? {
            // The chain was rebuilt clean in jj; settle again to export it.
            return self.settle();
        }
        let p = self
            .pending
            .as_mut()
            .expect("settle requires a pending resolution");
        p.conflicts = conflicts.clone();
        Ok(SaveOutcome::Conflicts { commits: conflicts })
    }

    /// Try to rebuild a held-back rewrite whose conflicts are merely *spurious* —
    /// adjacent-but-independent edits that jj's symmetric 3-way merge can't place,
    /// even though the combined result is well-defined. Generalized over the whole
    /// editable set: it reconstructs **every** conflicted editable branch's chain
    /// (each anchored on *its own* pre-rewrite tip) and re-parents **every**
    /// worktree's `@` — the launch one and each extra worktree's — carrying that
    /// worktree's *own* uncommitted delta, never another's. Two reconstruction
    /// modes per branch, by [`SpuriousResolve`] strategy:
    /// - [`SpuriousResolve::CleanTip`] (reorder/squash): the net change set is
    ///   preserved, so each conflicted branch's post-mutation tip is conflict-free
    ///   and *is* the result. Anchored on that clean tip, each conflicted commit's
    ///   tree is rebuilt top-down by *peeling* the commit above it off (replaying
    ///   its introduced change in reverse, see [`crate::replay`]). A conflicted tip
    ///   means a *true* conflict and bails.
    /// - [`SpuriousResolve::Drop`] / [`SpuriousResolve::Restore`]: the change set
    ///   itself changed (a commit was removed / re-inserted), so a post-mutation
    ///   tip may be conflicted and can't anchor anything. Instead each conflicted
    ///   commit's tree is rebuilt bottom-up from the clean prefix by *applying* its
    ///   own original change forward onto the rebuilt parent.
    ///
    /// Every branch is planned read-only first ([`Self::plan_spurious_head`]), then
    /// the chains and `@`s are rewritten in *one* transaction with explicit trees
    /// so jj never re-merges. A singleton editable set with one launch `@`
    /// reproduces the old single-head behaviour exactly. Returns `Ok(true)` once it
    /// rebuilt the conflicted chains clean (the caller re-settles to export),
    /// `Ok(false)` when it bailed — a real conflict, a non-text/structural change,
    /// a split launch `@`, two heads sharing a rewritten commit, or anything it
    /// can't prove safe — leaving jj at the post-mutation state for manual
    /// resolution.
    fn try_auto_resolve_spurious(&mut self, strategy: SpuriousResolve) -> Result<bool> {
        let (pre_op, old_head_hex) = match self.pending.as_ref() {
            Some(p) => (p.pre_op.clone(), p.old_head.clone()),
            None => return Ok(false),
        };
        // CleanTip peels down from each clean tip; Drop/Restore rebuild forward from
        // the clean prefix (a tip may be conflicted).
        let forward = match &strategy {
            SpuriousResolve::Off => return Ok(false),
            SpuriousResolve::CleanTip => false,
            SpuriousResolve::Drop | SpuriousResolve::Restore { .. } => true,
        };
        let Some(old_head_hex) = old_head_hex else {
            return Ok(false); // detached HEAD: no primary tip to anchor on
        };
        let Some(orig_primary) = CommitId::try_from_hex(old_head_hex) else {
            return Ok(false);
        };
        let store = self.repo.store().clone();
        let pre_view = block_on(pre_op.view()).context("reading the pre-rewrite view")?;

        // Each editable branch's pre-rewrite tip — the anchor its `@` delta replays
        // from. The primary's is the authoritative `old_head`; the extras' come from
        // the pending pre-rewrite heads map (git was frozen, so it still holds the
        // pre-rewrite oids).
        let mut pre_tips: HashMap<String, CommitId> = HashMap::new();
        if let Some(primary) = self.target_branch_name() {
            pre_tips.insert(primary.to_string(), orig_primary);
        }
        // Only *editable* branches are imported into this session's index, so a
        // non-editable branch in the (all-branches) pending heads map must be
        // skipped — walking its history below would fail "not found in index".
        let editable: HashSet<String> = self.editable_branches().into_iter().collect();
        if let Some(p) = self.pending.as_ref() {
            for (full, hex) in &p.heads {
                let short = full.strip_prefix("refs/heads/").unwrap_or(full);
                if !editable.contains(short) {
                    continue;
                }
                if let Some(id) = CommitId::try_from_hex(hex) {
                    pre_tips.entry(short.to_string()).or_insert(id);
                }
            }
        }

        // The original introduced change of every editable branch's pre-rewrite
        // commit, by change id (a shared ancestor appears once). Drives both the
        // peel and the forward rebuild. Restore additionally seeds the restored
        // (orphan) commit, absent from the post-drop histories.
        let mut originals: HashMap<ChangeId, (MergedTree, MergedTree)> = HashMap::new();
        for orig_tip in pre_tips.values() {
            for info in crate::history::history(&self.repo, orig_tip)? {
                if originals.contains_key(&info.change_id) {
                    continue;
                }
                let c = store
                    .get_commit(&info.id)
                    .context("loading an original commit")?;
                let parent_tree = block_on(c.parent_tree(self.repo.as_ref()))
                    .context("reading an original parent tree")?;
                originals.insert(info.change_id.clone(), (parent_tree, c.tree()));
            }
        }
        if let SpuriousResolve::Restore { commit } = &strategy {
            let c = store
                .get_commit(commit)
                .context("loading the restored commit")?;
            let parent_tree = block_on(c.parent_tree(self.repo.as_ref()))
                .context("reading the restored commit's parent tree")?;
            originals.insert(c.change_id().clone(), (parent_tree, c.tree()));
        }

        // Plan every editable head's rebuild read-only. Bail the whole attempt on
        // any non-spurious / non-linear / structural case, or if two heads share a
        // rewritten commit (only manual resolution handles that safely). Each plan
        // carries its branch's worktree `@` (if dirty), re-parented onto the new
        // tip from *that* worktree's own pre-rewrite `@`.
        #[allow(clippy::type_complexity)]
        let mut plans: Vec<(String, ChainRebuild, Option<(Commit, MergedTree)>)> = Vec::new();
        let mut seen: HashSet<CommitId> = HashSet::new();
        for branch in self.editable_branches() {
            let name: jj_lib::ref_name::RefNameBuf = branch.as_str().into();
            let Some(head) = self
                .repo
                .view()
                .get_local_bookmark(&name)
                .as_normal()
                .cloned()
            else {
                continue;
            };
            let chain = match self.plan_spurious_head(&store, &head, forward, &originals)? {
                HeadPlan::Clean => continue,
                HeadPlan::Bail => return Ok(false),
                HeadPlan::Rebuild(c) => c,
            };
            // A commit rewritten by two heads can't be rebuilt twice — bail.
            for c in &chain.chain[chain.lo..=chain.n] {
                if !seen.insert(c.id().clone()) {
                    return Ok(false);
                }
            }
            // This branch's worktree `@`: the launch worktree for the primary (when
            // bound), else the extra worktree checked out on this branch.
            let is_launch = Some(branch.as_str()) == self.target_branch_name();
            let wc_name = if is_launch {
                self.is_worktree_bound()
                    .then(|| self.workspace.workspace_name().to_owned())
            } else {
                self.find_worktree(&branch).map(|v| v.name.clone())
            };
            // A split `@` chain (launch or sibling) isn't reconstructed by the
            // leaf-only replay below — fall back to manual resolution. The launch
            // reader is empty off-worktree (len 0), so this keeps the old guard for
            // the launch and extends it to a now-splittable sibling worktree.
            let chain_len = if is_launch {
                self.working_copy_chain_ids().len()
            } else {
                self.find_worktree(&branch)
                    .map(|v| self.worktree_chain_ids(v).len())
                    .unwrap_or(0)
            };
            if chain_len > 1 {
                return Ok(false);
            }
            let wc = if let Some(wc_name) = &wc_name {
                match self.repo.view().get_wc_commit_id(wc_name).cloned() {
                    Some(wc_id) => {
                        let wc = store
                            .get_commit(&wc_id)
                            .context("loading the working copy")?;
                        let new_tip_tree = chain.trees[chain.n].clone().expect("tip tree computed");
                        let orig_wc_tree = match pre_view.get_wc_commit_id(wc_name) {
                            Some(id) => Some(
                                store
                                    .get_commit(id)
                                    .context("loading the original working copy")?
                                    .tree(),
                            ),
                            None => None,
                        };
                        // Carry the worktree's uncommitted delta onto the new tip.
                        // CleanTip leaves the tip unchanged, so `@`'s pre-mutation
                        // tree re-parents directly; drop/restore move it, so replay
                        // the delta (this branch's orig tip → its orig `@`) forward.
                        // A clean `@` has an empty delta, so it stays empty.
                        let wc_tree = match &orig_wc_tree {
                            None => new_tip_tree,
                            Some(w) if !forward => w.clone(),
                            Some(w) => {
                                let Some(orig_tip) = pre_tips.get(&branch) else {
                                    return Ok(false);
                                };
                                let orig_tip_tree = store
                                    .get_commit(orig_tip)
                                    .context("loading the original tip")?
                                    .tree();
                                match self.transform_tree(
                                    &store,
                                    &orig_tip_tree,
                                    w,
                                    new_tip_tree,
                                    Dir::Forward,
                                )? {
                                    Some(t) => t,
                                    None => return Ok(false),
                                }
                            }
                        };
                        Some((wc, wc_tree))
                    }
                    None => None,
                }
            } else {
                None
            };
            plans.push((branch, chain, wc));
        }
        if plans.is_empty() {
            return Ok(false); // no conflicted chain to rebuild (e.g. an `@`-only conflict)
        }

        // Apply every plan in one transaction: rewrite each conflicted range with
        // explicit trees/parents, re-parent each worktree `@`, re-point each
        // bookmark. The ranges are disjoint, so the single `rebase_descendants` only
        // settles jj's bookkeeping — nothing actually rebases.
        let mut tx = self.repo.start_transaction();
        for (branch, chain, wc) in &plans {
            let mut parent_id = chain.chain[chain.lo - 1].id().clone();
            let mut new_tip = parent_id.clone();
            for i in chain.lo..=chain.n {
                let tree = chain.trees[i]
                    .clone()
                    .expect("tree computed for the conflicted range");
                let new_commit = block_on(
                    tx.repo_mut()
                        .rewrite_commit(&chain.chain[i])
                        .set_parents(vec![parent_id.clone()])
                        .set_tree(tree)
                        .write(),
                )
                .context("rewriting a reconstructed commit")?;
                parent_id = new_commit.id().clone();
                new_tip = new_commit.id().clone();
            }
            if let Some((wc_commit, wc_tree)) = wc {
                block_on(
                    tx.repo_mut()
                        .rewrite_commit(wc_commit)
                        .set_parents(vec![new_tip.clone()])
                        .set_tree(wc_tree.clone())
                        .write(),
                )
                .context("re-parenting the working copy")?;
            }
            self.set_branch_bookmark(tx.repo_mut(), branch, new_tip);
        }
        // Update jj's bookkeeping (notably the working-copy pointers) for the
        // explicit rewrites; every commit is already rewritten with the right
        // parents, so nothing is actually rebased.
        block_on(tx.repo_mut().rebase_descendants()).context("settling the rebuilt chain")?;
        self.repo = block_on(tx.commit("commedit: auto-resolve spurious conflict"))
            .context("committing the rebuilt chain")?;
        Ok(true)
    }

    /// Plan one editable branch's spurious rebuild (read-only): walk `head`'s chain,
    /// find the conflicted range `[lo, n]`, and reconstruct a clean tree for each
    /// commit in it. [`HeadPlan::Clean`] when the chain has no conflict (skip it),
    /// [`HeadPlan::Bail`] when it can't be proven spurious (abandon the whole
    /// attempt). This is the per-branch core the old single-head resolver ran on the
    /// primary alone.
    fn plan_spurious_head(
        &self,
        store: &std::sync::Arc<jj_lib::store::Store>,
        head: &CommitId,
        forward: bool,
        originals: &HashMap<ChangeId, (MergedTree, MergedTree)>,
    ) -> Result<HeadPlan> {
        let tip = store
            .get_commit(head)
            .context("loading the rewritten tip")?;
        // CleanTip *requires* a clean tip (a conflicted one is a true conflict);
        // drop/restore tolerate it and never read it back.
        if !forward && tip.has_conflict() {
            return Ok(HeadPlan::Bail);
        }
        let chain_infos = crate::history::history(&self.repo, head)?;
        if chain_infos.is_empty() {
            return Ok(HeadPlan::Clean);
        }
        let mut chain = Vec::with_capacity(chain_infos.len());
        for info in chain_infos.iter().rev() {
            chain.push(
                store
                    .get_commit(&info.id)
                    .context("loading a chain commit")?,
            );
        }
        let n = chain.len() - 1; // tip index (oldest-first)
        let Some(lo) = chain.iter().position(|c| c.has_conflict()) else {
            return Ok(HeadPlan::Clean); // nothing conflicted on this branch
        };
        if lo == 0 {
            return Ok(HeadPlan::Bail); // the root is conflicted: not a plain rewrite
        }
        // The rebuild rewrites `[lo, n]` as a single-parent chain anchored on
        // `chain[lo - 1]`; `history()`'s reversed order is only a parent chain when
        // that range is linear. A merge or interleaved sibling would be silently
        // linearized, so hand those to manual resolution.
        for i in lo..=n {
            if chain[i].parent_ids() != std::slice::from_ref(chain[i - 1].id()) {
                return Ok(HeadPlan::Bail);
            }
        }
        let mut trees: Vec<Option<MergedTree>> = vec![None; chain.len()];
        if forward {
            // Bottom-up: apply each commit's own original change forward onto its
            // rebuilt parent, anchored on the clean commit below `lo`.
            for i in 0..lo {
                trees[i] = Some(chain[i].tree());
            }
            for i in lo..=n {
                let below = trees[i - 1].clone().expect("tree below computed");
                let Some((parent_tree, own_tree)) = originals.get(chain[i].change_id()) else {
                    return Ok(HeadPlan::Bail);
                };
                match self.transform_tree(store, parent_tree, own_tree, below, Dir::Forward)? {
                    Some(tree) => trees[i] = Some(tree),
                    None => return Ok(HeadPlan::Bail), // a real overlap or structural change
                }
            }
        } else {
            // Top-down: anchor on the clean tip, peel the commit above each one off.
            trees[n] = Some(tip.tree());
            for i in (lo..n).rev() {
                let above = trees[i + 1].clone().expect("upper tree computed");
                let Some((parent_tree, own_tree)) = originals.get(chain[i + 1].change_id()) else {
                    return Ok(HeadPlan::Bail);
                };
                match self.transform_tree(store, parent_tree, own_tree, above, Dir::Peel)? {
                    Some(tree) => trees[i] = Some(tree),
                    None => return Ok(HeadPlan::Bail),
                }
            }
        }
        Ok(HeadPlan::Rebuild(ChainRebuild {
            chain,
            lo,
            n,
            trees,
        }))
    }

    /// Replay one commit's introduced change (`parent_tree` → `own_tree`) onto the
    /// tree `onto`, file by file, via [`crate::replay::replay_change`]. With
    /// [`Dir::Peel`] it *removes* the change (replays `own → parent`, to peel a
    /// commit off the tree above it); with [`Dir::Forward`] it *applies* the change
    /// (replays `parent → own` onto a new base). In both directions `onto` is
    /// trusted for context, so an independent adjacent edit is relocated rather
    /// than conflicting. Returns `None` (so the caller falls back to manual) when
    /// the commit adds/removes a file, touches binary content, or an edit genuinely
    /// overlaps content `onto` already changed.
    fn transform_tree(
        &self,
        store: &std::sync::Arc<jj_lib::store::Store>,
        parent_tree: &MergedTree,
        own_tree: &MergedTree,
        onto: MergedTree,
        dir: Dir,
    ) -> Result<Option<MergedTree>> {
        let changes = crate::diff::tree_changes(store, parent_tree, own_tree)?;
        let mut entries: Vec<(String, String)> = Vec::new();
        for ch in &changes {
            if ch.is_binary || ch.kind != crate::diff::ChangeKind::Modified {
                return Ok(None); // structural change: not safely replayable here
            }
            let path = RepoPath::from_internal_string(&ch.path).context("invalid path")?;
            let Some(onto_text) = self.tree_file_text(store, &onto, path)? else {
                return Ok(None);
            };
            let old = ch.old_text.as_deref().unwrap_or("");
            let new = ch.new_text.as_deref().unwrap_or("");
            let resolved = match dir {
                Dir::Peel => crate::replay::replay_change(new, &onto_text, old),
                Dir::Forward => crate::replay::replay_change(old, &onto_text, new),
            };
            let Some(resolved) = resolved else {
                return Ok(None);
            };
            entries.push((ch.path.clone(), resolved));
        }
        let tree = crate::tree::splice_files_into_tree(onto, store, &entries)?;
        Ok(Some(tree))
    }

    /// The resolved UTF-8 text of `path` in `tree`, or `None` if it is absent,
    /// conflicted, or binary.
    fn tree_file_text(
        &self,
        store: &std::sync::Arc<jj_lib::store::Store>,
        tree: &MergedTree,
        path: &RepoPath,
    ) -> Result<Option<String>> {
        let value = block_on(tree.path_value(path)).context("reading a tree path")?;
        let resolved = value.into_resolved().ok().flatten();
        let (text, binary) = crate::diff::read_text(store, path, resolved.as_ref())?;
        if binary {
            return Ok(None);
        }
        Ok(text)
    }

    /// The deferred export: push the (now conflict-free) rewrite to git in its
    /// own transaction, then re-attach HEAD and sync the working tree — the
    /// transparency tail that used to run inline in each mutation.
    fn export_and_sync(
        &mut self,
        old_head: Option<String>,
        heads: &BTreeMap<String, String>,
    ) -> Result<()> {
        let mut tx = self.repo.start_transaction();
        crate::transparency::export_to_git(tx.repo_mut())?;
        self.repo = block_on(tx.commit("commedit: export to git")).context("committing export")?;
        // jj exported the moved bookmarks into its throwaway git dir, not the user's
        // repo; mirror every editable bookmark whose tip changed into the real
        // repository. Must precede materialize_after_rewrite, which resets the index
        // to the user's HEAD (now the new tip).
        self.bridge_branches_to_git(old_head.as_deref(), heads);
        // The launch worktree's HEAD/index/worktree participate only when this
        // session is worktree-bound — i.e. its primary branch is the checked-out
        // one. Off-worktree (the primary isn't checked out here) there is no working
        // copy: HEAD/index/worktree stay frozen, only the editable refs move.
        //
        // When worktree-bound and editing a *sibling* editable branch (multi-branch
        // set), the launch branch's tip is unchanged, so re-attaching HEAD and
        // re-checking-out the launch `@` are no-ops on disk: `@` and HEAD are where
        // they were. (Phase 1b generalizes this to per-worktree materialization.)
        if self.is_worktree_bound() {
            self.reattach_head()?;
        }
        self.protect_unrelated_heads(heads);
        if self.is_worktree_bound() {
            // Write the rebased working-copy commit @' back to disk (preserving the
            // user's uncommitted changes through the rewrite), in place of the old
            // git read-tree sync. Unconditional when worktree-bound: the launch `@`
            // can move even when the launch branch's tip does not (e.g. `revert_all`
            // resets the working copy), so a tip-only gate here would skip a needed
            // re-materialization.
            self.materialize_after_rewrite(old_head)?;
        }
        // Re-materialize every *extra* worktree whose branch tip actually moved (its
        // bridged git ref differs from the pre-rewrite `before` map). A worktree
        // whose branch was untouched is left frozen, and a selected branch with no
        // worktree is a pure ref-move (none registered). Unlike the launch worktree,
        // an extra worktree's `@` only moves when its branch tip does, so a tip
        // comparison is both correct and sufficient here.
        self.materialize_moved_worktrees(heads)?;
        Ok(())
    }
}

/// Turn jj's Git diff3-style markers (which include a `|||||||` base section)
/// into plain Git 2-way markers by dropping each base section: everything from a
/// `|||||||…` line up to (but not including) the following `=======` line.
fn strip_base_sections(text: &str, marker_len: usize) -> String {
    let is_marker = |line: &str, ch: char| {
        let count = line.chars().take_while(|&c| c == ch).count();
        count >= marker_len
    };
    let mut out = String::new();
    let mut in_base = false;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        if is_marker(body, '|') {
            in_base = true;
            continue;
        }
        if is_marker(body, '=') {
            in_base = false;
            out.push_str(line);
            continue;
        }
        if in_base {
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Rewrite jj's verbose conflict-marker labels into something a plain-git user
/// recognizes. jj annotates each side with its change id, git commit id,
/// description and a role, e.g.
/// `<<<<<<< lywxrykm c2eece18 "foo" (rebase destination)`. We keep the git short
/// id and the description and drop the jj change id (meaningless without jj) and
/// the trailing role annotation, leaving `<<<<<<< c2eece18 "foo"`. Labels are
/// cosmetic — the round-trip parse keys on the marker run length, not the text
/// after it — so this only affects what the user reads.
fn simplify_marker_labels(text: &str, marker_len: usize) -> String {
    let marker_run = |line: &str, ch: char| {
        let n = line.chars().take_while(|&c| c == ch).count();
        (n >= marker_len).then_some(n)
    };
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let marker = ['<', '=', '>']
            .into_iter()
            .find_map(|ch| marker_run(body, ch).map(|n| (ch, n)));
        match marker {
            // Marker chars are ASCII, so the run length is also the byte offset of
            // the label that follows it.
            Some((_, run)) => {
                let (prefix, rest) = body.split_at(run);
                out.push_str(prefix);
                let label = simplify_label(rest.trim());
                if !label.is_empty() {
                    out.push(' ');
                    out.push_str(&label);
                }
                if line.ends_with('\n') {
                    out.push('\n');
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Reduce one marker label to `<commit id> "<description>"`: drop the leading jj
/// change-id token and any trailing ` (…)` role annotation. Returns an empty
/// string for an empty label (e.g. the bare `=======` separator).
fn simplify_label(label: &str) -> String {
    if label.is_empty() {
        return String::new();
    }
    // Drop the leading jj change-id token (jj always emits it first).
    let rest = label
        .split_once(char::is_whitespace)
        .map(|(_, r)| r.trim())
        .unwrap_or("");
    // Drop a trailing " (role)" annotation; the last " (" can't fall inside the
    // quoted description, which closes with `"` before the annotation begins.
    match rest.rsplit_once(" (") {
        Some((core, _)) if rest.ends_with(')') => core.trim().to_string(),
        _ => rest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::simplify_marker_labels;

    #[test]
    fn simplifies_jj_marker_labels() {
        let input = "\
<<<<<<< lywxrykm c2eece18 \"foo\" (rebase destination)
keep ours
=======
keep theirs
>>>>>>> mswnszso df01ec69 \"bar\" (rebased revision)
";
        let expected = "\
<<<<<<< c2eece18 \"foo\"
keep ours
=======
keep theirs
>>>>>>> df01ec69 \"bar\"
";
        assert_eq!(simplify_marker_labels(input, 7), expected);
    }

    #[test]
    fn handles_missing_description_and_bare_separator() {
        let input = "<<<<<<< abcdefgh 1234abcd (rebase destination)\n=======\n";
        let expected = "<<<<<<< 1234abcd\n=======\n";
        assert_eq!(simplify_marker_labels(input, 7), expected);
    }
}
