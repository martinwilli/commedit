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

use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Context, Result};
use jj_lib::backend::{ChangeId, CommitId, CopyId, TreeValue};
use jj_lib::merged_tree::MergedTree;
use jj_lib::conflicts::{
    choose_materialized_conflict_marker_len, materialize_merge_result_to_bytes,
    materialize_tree_value, resolve_file_executable, update_from_content,
    ConflictMarkerStyle, ConflictMaterializeOptions, MaterializedTreeValue,
};
use jj_lib::merge::MergedTreeValue;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::operation::Operation;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::transaction::Transaction;

use crate::repo::Repo;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    pollster::block_on(f)
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
    /// Pre-rewrite local bookmark targets, to hold unrelated branches in place
    /// at export time (see [`Repo::confine_bookmark_moves`]).
    bookmarks: Vec<(RefNameBuf, RefTarget)>,
    /// Pre-rewrite git branch heads, for the export-time backstop
    /// (see [`Repo::protect_unrelated_heads`]).
    heads: BTreeMap<String, String>,
    /// Conflicted commits, oldest first; re-derived after every resolution.
    conflicts: Vec<ConflictedCommit>,
    /// Set by a *reorder*: before falling back to manual resolution, try to
    /// auto-resolve *spurious* conflicts — adjacent-but-independent edits that
    /// conflict under jj's symmetric 3-way merge yet leave the branch tip
    /// identical to the original. See [`Repo::try_auto_resolve_spurious_reorder`].
    auto_resolve_spurious: bool,
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

    /// Commit the rewrite transaction, then either export to git (if the branch
    /// tip's ancestor chain is conflict-free) or hold the rewrite pending while
    /// the conflicts are resolved. Every mutation ends here in place of the old
    /// inline export tail.
    pub(crate) fn finish_mutation(
        &mut self,
        tx: Transaction,
        op_msg: &str,
        pre_op: Operation,
        old_head: Option<String>,
        bookmarks: Vec<(RefNameBuf, RefTarget)>,
        heads: BTreeMap<String, String>,
    ) -> Result<SaveOutcome> {
        self.finish_mutation_inner(tx, op_msg, pre_op, old_head, bookmarks, heads, false)
    }

    /// Like [`Self::finish_mutation`] but, for a reorder, opts the held-back chain
    /// into spurious-conflict auto-resolution (see [`PendingResolution`]).
    pub(crate) fn finish_mutation_auto_resolve(
        &mut self,
        tx: Transaction,
        op_msg: &str,
        pre_op: Operation,
        old_head: Option<String>,
        bookmarks: Vec<(RefNameBuf, RefTarget)>,
        heads: BTreeMap<String, String>,
    ) -> Result<SaveOutcome> {
        self.finish_mutation_inner(tx, op_msg, pre_op, old_head, bookmarks, heads, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_mutation_inner(
        &mut self,
        tx: Transaction,
        op_msg: &str,
        pre_op: Operation,
        old_head: Option<String>,
        bookmarks: Vec<(RefNameBuf, RefTarget)>,
        heads: BTreeMap<String, String>,
        auto_resolve_spurious: bool,
    ) -> Result<SaveOutcome> {
        self.repo = block_on(tx.commit(op_msg)).context("committing rewrite")?;
        self.pending = Some(PendingResolution {
            pre_op,
            old_head,
            op_msg: op_msg.to_string(),
            bookmarks,
            heads,
            conflicts: Vec::new(),
            auto_resolve_spurious,
        });
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

    /// Apply the user's edited conflict text for several files of the commit with
    /// change id `change_hex` at once: parse each back into file ids, splice every
    /// result into the commit's tree in one rewrite, rebase descendants, and
    /// re-settle the chain. Resolving a commit's conflicted paths together is
    /// sound because they are independent — no intermediate re-materialization is
    /// needed between them. Structural (non-file) paths are skipped. Returns the
    /// refreshed outcome — `Clean` once the last conflict is gone (the rewrite is
    /// exported at that point), otherwise the remaining `Conflicts`. `files` is
    /// `(path, edited_text, marker_len)` tuples.
    pub fn resolve_conflicts(
        &mut self,
        change_hex: &str,
        files: &[(String, String, usize)],
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("resolving the conflict", || {
            self.resolve_conflicts_inner(change_hex, files)
        })
    }

    fn resolve_conflicts_inner(
        &mut self,
        change_hex: &str,
        files: &[(String, String, usize)],
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

        // Parse each file's resolved text into a tree value up front (while `tree`
        // is still borrowable), then splice them all into one builder.
        let mut entries: Vec<(RepoPathBuf, MergedTreeValue)> = Vec::with_capacity(files.len());
        for (path, edited_text, marker_len) in files {
            let path: &RepoPath = RepoPath::from_internal_string(path).context("invalid path")?;
            let value = block_on(tree.path_value(path)).context("reading conflicted path")?;
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
                edited_text.as_bytes(),
                *marker_len,
            ))
            .context("parsing resolved content")?;

            // Lift the resolved/again-conflicted file ids back into a tree value,
            // preserving the executable bit.
            let merged_value: MergedTreeValue = new_ids.map(|oid| {
                oid.as_ref().map(|id| TreeValue::File {
                    id: id.clone(),
                    executable: exec,
                    copy_id: CopyId::placeholder(),
                })
            });
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
        self.repo = block_on(tx.commit("commedit: resolve conflict"))
            .context("committing resolution")?;

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
            self.repo = block_on(tx.commit("commedit: abort rewrite"))
                .context("recording the abort")?;
        }
        Ok(())
    }

    /// Roll the entire session back to its starting point: restore jj's view to
    /// the operation captured at [`Repo::open`] (the original commits *and* the
    /// session-start working copy) and re-export it to git, so plain `git` sees
    /// the original history and the working tree is reset to its session-start
    /// content. Discards every rewrite/reorder/squash/drop and every
    /// working-copy edit made this session — the in-app equivalent of
    /// `git reset --hard <session head>`.
    ///
    /// Like [`Self::abort`], the restore is *recorded* as a new operation rather
    /// than a bare reload (see that method's note on why a divergent op head
    /// would otherwise resurface the old state). Unlike `abort`, clean saves
    /// during the session already moved git refs / HEAD / the worktree, so the
    /// restored state must be exported and materialized back to disk — hence the
    /// `export_and_sync` tail. Reverting drops any pending conflicted rewrite
    /// first (git was never touched for it).
    pub fn revert_all(&mut self) -> Result<()> {
        crate::repo::catch_jj("reverting the session", || self.revert_all_inner())
    }

    fn revert_all_inner(&mut self) -> Result<()> {
        let Some(session_op) = self.session_op.clone() else {
            return Ok(());
        };
        // Drop any held-back conflicted rewrite; git was never touched for it.
        self.pending = None;
        // The export tail needs the *current* (rewritten) on-disk state to sync
        // away from and the unrelated branches to hold in place.
        let old_head = self.head_commit();
        let bookmarks = self.local_bookmark_targets();
        let heads = self.snapshot_heads();
        // jj's recorded git-ref state tracks what it last wrote to git's
        // refs/*; the session's clean saves left it at the rewritten tips. Keep
        // a copy: `set_view` below rewinds this record to the session-start
        // values, but git's actual on-disk refs are still at the rewritten tips,
        // so the export would see no bookmark/ref diff and push nothing. We
        // re-stamp these afterwards so the export reconciles git with reality.
        let on_disk_git_refs: Vec<_> = self
            .repo
            .view()
            .git_refs()
            .iter()
            .map(|(name, target)| (name.clone(), target.clone()))
            .collect();
        // Restore the session-start view and record it as a new operation.
        let view = block_on(session_op.view()).context("reading the session-start view")?;
        let mut tx = self.repo.start_transaction();
        tx.repo_mut().set_view(view.store_view().clone());
        // Re-point the recorded git refs at what git actually holds on disk, so
        // the deferred export detects bookmark(session-start) != git-ref(current)
        // and pushes the restored tips back to git.
        for (name, target) in &on_disk_git_refs {
            tx.repo_mut().set_git_ref_target(name, target.clone());
        }
        self.repo = block_on(tx.commit("commedit: revert all to session start"))
            .context("recording the revert")?;
        // Push the restored state back to git and check the original working
        // copy back out to disk. The session-start state was a clean exported
        // git history, so the restored chain is always conflict-free.
        self.export_and_sync(old_head, &bookmarks, &heads)
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

    /// Resolve `change_hex` to the commit carrying it on the *current* branch
    /// chain (the ancestors of jj's head — the same set [`Self::collect_conflicts`]
    /// walks). Conflict resolution always targets a commit on the pending
    /// rewritten chain, so scoping the lookup to that chain — rather than the
    /// store-wide `resolve_change_id` — disambiguates change ids that have
    /// divergent siblings left over from concurrent or earlier operations, which
    /// would otherwise make the global resolver bail as ambiguous.
    fn resolve_change_on_chain(&self, change_hex: &str) -> Result<CommitId> {
        let change_id = ChangeId::try_from_hex(change_hex).context("invalid change id")?;
        // The working-copy chain (@ and any split-off entries) sits above the
        // branch tip, so the ancestor walk below never sees it; match those
        // entries first.
        for wc_id in self.working_copy_chain_ids() {
            if let Ok(commit) = self.repo.store().get_commit(&wc_id) {
                if commit.change_id() == &change_id {
                    return Ok(wc_id);
                }
            }
        }
        let head = self
            .current_head_in_jj()
            .context("no current branch head to resolve the conflict against")?;
        let infos = crate::history::history(&self.repo, &head)?;
        infos
            .into_iter()
            .find(|i| i.change_id == change_id)
            .map(|i| i.id)
            .with_context(|| {
                format!("change {change_hex} is not on the current branch chain")
            })
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

    /// Walk the ancestors of `head` (oldest first) collecting the commits whose
    /// trees are conflicted, with their conflicted paths.
    fn collect_conflicts(&self, head: Option<&CommitId>) -> Result<Vec<ConflictedCommit>> {
        let Some(head) = head else {
            return Ok(Vec::new());
        };
        let infos = crate::history::history(&self.repo, head)?;
        let store = self.repo.store();
        let mut out = Vec::new();
        for info in infos.iter().rev() {
            let commit = store.get_commit(&info.id).context("loading commit")?;
            if !commit.has_conflict() {
                continue;
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
            out.push(ConflictedCommit {
                change_id: info.change_id.clone(),
                commit_id: info.id.clone(),
                subject: info.subject.clone(),
                files,
            });
        }
        // The working-copy chain (@ and any split-off entries) is a *descendant*
        // of the tip, so the ancestor walk above never sees it. Append each
        // conflicted entry, oldest first (the chain is newest-first), so an
        // overlap between the user's uncommitted changes and the rewrite defers
        // the export and is resolved in the diff pane like any other commit.
        for wc_id in self.working_copy_chain_ids().into_iter().rev() {
            let commit = store
                .get_commit(&wc_id)
                .context("loading a working-copy chain commit")?;
            if !commit.has_conflict() {
                continue;
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
            out.push(ConflictedCommit {
                change_id: commit.change_id().clone(),
                commit_id: wc_id,
                subject: "Uncommitted changes".to_string(),
                files,
            });
        }
        Ok(out)
    }

    /// After committing a rewrite/resolution, decide whether the chain is clean
    /// (export and clear pending) or still conflicted (refresh pending).
    fn settle(&mut self) -> Result<SaveOutcome> {
        let head = self.current_head_in_jj();
        let conflicts = self.collect_conflicts(head.as_ref())?;
        if conflicts.is_empty() {
            let p = self.pending.take().expect("settle requires a pending resolution");
            self.export_and_sync(p.old_head, &p.bookmarks, &p.heads)?;
            return Ok(SaveOutcome::Clean);
        }
        // A reorder whose tip is already identical to the original may have only
        // *spurious* intermediate conflicts (adjacent-but-independent edits). Try
        // to rebuild the chain clean before handing the conflicts to the user.
        // Attempt at most once: clear the flag so a failed attempt — or the
        // recursive settle below — falls straight through to manual resolution.
        let auto = self
            .pending
            .as_ref()
            .is_some_and(|p| p.auto_resolve_spurious);
        if auto {
            if let Some(p) = self.pending.as_mut() {
                p.auto_resolve_spurious = false;
            }
            if self.try_auto_resolve_spurious_reorder()? {
                // The chain was rebuilt clean in jj; settle again to export it.
                return self.settle();
            }
        }
        let p = self.pending.as_mut().expect("settle requires a pending resolution");
        p.conflicts = conflicts.clone();
        Ok(SaveOutcome::Conflicts { commits: conflicts })
    }

    /// Try to rebuild a held-back *reorder* whose conflicts are spurious — the
    /// branch tip is already byte-identical to the original, and only intermediate
    /// commits conflict because jj's symmetric 3-way merge can't place
    /// adjacent-but-independent edits. Anchored on the clean tip, it reconstructs
    /// each commit's tree top-down by *peeling* the commits above it off the tip
    /// (replaying each one's introduced change in reverse, see [`crate::replay`]),
    /// then rewrites the conflicted range — and the working copy `@` — with explicit
    /// trees so jj never re-merges. The new tip is set to the original tree, so the
    /// final result is provably unchanged.
    ///
    /// Uncommitted changes are preserved (the working copy re-parents onto the
    /// new, identical tip). Returns `Ok(true)` once it rebuilt the chain clean (the
    /// caller re-settles to export), `Ok(false)` when it bailed — a real conflict, a
    /// non-text/structural change, a split working-copy chain, or anything it can't
    /// prove safe — leaving jj at the post-reorder state for manual resolution.
    fn try_auto_resolve_spurious_reorder(&mut self) -> Result<bool> {
        let (pre_op, old_head_hex) = match self.pending.as_ref() {
            Some(p) => (p.pre_op.clone(), p.old_head.clone()),
            None => return Ok(false),
        };
        let Some(old_head_hex) = old_head_hex else {
            return Ok(false); // detached HEAD: no original branch tip to anchor on
        };
        let Some(orig_head) = CommitId::try_from_hex(old_head_hex) else {
            return Ok(false);
        };
        let store = self.repo.store().clone();

        // The post-reorder branch chain, oldest first. Bail unless the tip is clean
        // (a conflicted tip is a *true* conflict, not a spurious one).
        let Some(head) = self.current_head_in_jj() else {
            return Ok(false);
        };
        let tip = store.get_commit(&head).context("loading the reordered tip")?;
        if tip.has_conflict() {
            return Ok(false);
        }
        let expected_tip_tree = tip.tree();
        let chain_infos = crate::history::history(&self.repo, &head)?;
        if chain_infos.is_empty() {
            return Ok(false);
        }
        let mut chain = Vec::with_capacity(chain_infos.len());
        for info in chain_infos.iter().rev() {
            chain.push(store.get_commit(&info.id).context("loading a chain commit")?);
        }
        let n = chain.len() - 1; // tip index (oldest-first)
        let Some(lo) = chain.iter().position(|c| c.has_conflict()) else {
            return Ok(false); // nothing conflicted on the branch — not our case
        };
        if lo == 0 {
            return Ok(false); // the root is conflicted: not a plain reorder
        }

        // Only a simple single-`@` working copy is handled; a split chain falls
        // back. Uncommitted changes are *preserved*, not a reason to bail: the new
        // tip is byte-identical to the original, so the working copy's pre-reorder
        // tree (captured at the snapshot just before the reorder) re-parents onto it
        // cleanly — its delta applies to identical content, so it can never clash.
        if self.working_copy_chain_ids().len() > 1 {
            return Ok(false);
        }
        let pre_view = block_on(pre_op.view()).context("reading the pre-rewrite view")?;
        let orig_wc_tree = match pre_view.get_wc_commit_id(self.workspace.workspace_name()) {
            Some(id) => Some(
                store
                    .get_commit(id)
                    .context("loading the original working copy")?
                    .tree(),
            ),
            None => None,
        };

        // The original (pre-reorder) introduced change of each commit, by change id:
        // (parent tree, own tree). Used to peel a commit's change back off the tip.
        let orig_infos = crate::history::history(&self.repo, &orig_head)?;
        let mut originals: HashMap<ChangeId, (MergedTree, MergedTree)> = HashMap::new();
        for info in &orig_infos {
            let c = store.get_commit(&info.id).context("loading an original commit")?;
            let parent_tree = block_on(c.parent_tree(self.repo.as_ref()))
                .context("reading an original parent tree")?;
            originals.insert(info.change_id.clone(), (parent_tree, c.tree()));
        }

        // Reconstruct each tree from the clean tip down to `lo`, peeling off the
        // change of the commit directly above it.
        let mut trees: Vec<Option<MergedTree>> = vec![None; chain.len()];
        trees[n] = Some(expected_tip_tree.clone());
        for i in (lo..n).rev() {
            let above = trees[i + 1].clone().expect("upper tree computed");
            let change_id = chain[i + 1].change_id();
            let Some((parent_tree, own_tree)) = originals.get(change_id) else {
                return Ok(false);
            };
            match self.peel_commit_change(&store, parent_tree, own_tree, above)? {
                Some(tree) => trees[i] = Some(tree),
                None => return Ok(false), // a real overlap or a structural change
            }
        }

        // Rewrite the conflicted range `[lo, n]` with explicit trees and parents so
        // jj never re-merges, then re-parent the working copy `@` onto the new tip.
        let mut tx = self.repo.start_transaction();
        let mut parent_id = chain[lo - 1].id().clone();
        let mut new_tip = head.clone();
        for i in lo..=n {
            let tree = trees[i].clone().expect("tree computed for the conflicted range");
            let new_commit = block_on(
                tx.repo_mut()
                    .rewrite_commit(&chain[i])
                    .set_parents(vec![parent_id.clone()])
                    .set_tree(tree)
                    .write(),
            )
            .context("rewriting a reordered commit")?;
            parent_id = new_commit.id().clone();
            new_tip = new_commit.id().clone();
        }
        if let Some(wc_id) = self.working_copy_commit_id() {
            let wc = store.get_commit(&wc_id).context("loading the working copy")?;
            // Carry the working copy's pre-reorder content onto the new tip,
            // preserving any uncommitted changes (it equals the tip tree when the
            // tree was clean, so an empty `@` stays empty).
            let wc_tree = orig_wc_tree.clone().unwrap_or_else(|| expected_tip_tree.clone());
            block_on(
                tx.repo_mut()
                    .rewrite_commit(&wc)
                    .set_parents(vec![new_tip.clone()])
                    .set_tree(wc_tree)
                    .write(),
            )
            .context("re-parenting the working copy")?;
        }
        self.set_head_bookmark(tx.repo_mut(), new_tip.clone());
        // Update jj's bookkeeping (notably the working-copy pointer) for the
        // explicit rewrites; every commit is already rewritten with the right
        // parents, so nothing is actually rebased.
        block_on(tx.repo_mut().rebase_descendants()).context("settling the rebuilt chain")?;
        self.repo = block_on(tx.commit("commedit: auto-resolve spurious reorder"))
            .context("committing the rebuilt chain")?;
        Ok(true)
    }

    /// Remove one commit's introduced change (`parent_tree` → `own_tree`) from the
    /// tree `above` it, file by file, via [`crate::replay::replay_change`] in its
    /// "peel" direction. Returns `None` (so the caller falls back to manual) when
    /// the commit adds/removes a file, touches binary content, or an edit genuinely
    /// overlaps content `above` already changed.
    fn peel_commit_change(
        &self,
        store: &std::sync::Arc<jj_lib::store::Store>,
        parent_tree: &MergedTree,
        own_tree: &MergedTree,
        above: MergedTree,
    ) -> Result<Option<MergedTree>> {
        let changes = crate::diff::tree_changes(store, parent_tree, own_tree)?;
        let mut entries: Vec<(String, String)> = Vec::new();
        for ch in &changes {
            if ch.is_binary || ch.kind != crate::diff::ChangeKind::Modified {
                return Ok(None); // structural change: not safely replayable here
            }
            let path = RepoPath::from_internal_string(&ch.path).context("invalid path")?;
            let Some(above_text) = self.tree_file_text(store, &above, path)? else {
                return Ok(None);
            };
            // Peel: undo this commit's `old -> new` edit on `above`, i.e. replay the
            // change `own (= new) -> parent (= old)` onto `above`.
            let own = ch.new_text.as_deref().unwrap_or("");
            let parent = ch.old_text.as_deref().unwrap_or("");
            let Some(resolved) = crate::replay::replay_change(own, &above_text, parent) else {
                return Ok(None);
            };
            entries.push((ch.path.clone(), resolved));
        }
        let tree = crate::tree::splice_files_into_tree(above, store, &entries)?;
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
        bookmarks: &[(RefNameBuf, RefTarget)],
        heads: &BTreeMap<String, String>,
    ) -> Result<()> {
        let mut tx = self.repo.start_transaction();
        self.confine_bookmark_moves(tx.repo_mut(), bookmarks);
        crate::transparency::export_to_git(tx.repo_mut())?;
        self.repo = block_on(tx.commit("commedit: export to git"))
            .context("committing export")?;
        self.reattach_head()?;
        self.protect_unrelated_heads(heads);
        // Write the rebased working-copy commit @' back to disk (preserving the
        // user's uncommitted changes through the rewrite), in place of the old
        // git read-tree sync.
        self.materialize_after_rewrite(old_head.clone())?;
        if let Some(old) = old_head {
            self.prune_orphaned_keep_refs(&old);
        }
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
