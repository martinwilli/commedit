//! Snapshot the on-disk working copy into jj's working-copy commit (`@`) and
//! materialize a (rewritten) `@` back to disk.
//!
//! commedit historically kept `@` empty and synced the working tree with
//! `git read-tree`. To make uncommitted changes first-class — visible in the
//! history and carried through rewrites by jj's `rebase_descendants` — we
//! instead snapshot the working directory into `@` (so the diff `@`-vs-parent
//! *is* the uncommitted state) and check the rebased result back out afterwards.
//!
//! jj skips `.git`/`.jj` while snapshotting (its `RESERVED_DIR_NAMES`), and
//! honours the repo's in-tree `.gitignore`s plus `.git/info/exclude`, so neither
//! jj's own metadata nor ignored files leak into `@`. The one exception mirrors
//! git: a file already tracked is snapshotted even when an ignore rule covers it
//! (e.g. a force-added `m4/.keep` under an ignored `m4/`) — see the
//! `force_tracking_matcher` in [`Repo::snapshot_working_copy`].
//!
//! Files git considers **untracked** (present on disk but in no commit) are
//! deliberately *excluded* from `@`: we never auto-track new files, so they
//! don't surface as "uncommitted changes". Because jj never tracks them, they
//! also stay put on disk through every checkout/materialize — `check_out` only
//! diffs the tracked trees, so an untracked file (in neither tree) is never
//! deleted. They survive a rewrite untouched. The one way to pull a new file in
//! is the explicit `add_paths` opt-in of [`Repo::snapshot_working_copy_tracking`]
//! (e.g. `commit_working_copy` / `squash_working_copy` adding a brand-new file).

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use jj_lib::backend::{CommitId, Signature, TreeValue};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, FilesMatcher, Matcher};
use jj_lib::merge::{Merge, MergedTreeValue};
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::store::Store;
use jj_lib::working_copy::{CheckoutStats, SnapshotOptions};

use crate::conflict::{OpDescriptor, SaveOutcome};
use crate::diff::{apply_patch, render_diff, select_groups, ContextExpansion};
use crate::history::parse_timestamp;
use crate::repo::Repo;
use crate::rewrite::Identity;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    pollster::block_on(f)
}

/// A summary of the working-copy commit `@`, shown as a read-only top row in the
/// history. `None` (from [`Repo::working_copy_info`]) when the tree is clean.
#[derive(Debug, Clone)]
pub struct WorkingCopyInfo {
    /// The `@` commit, whose diff against its parent is the uncommitted change
    /// set — load it with [`crate::diff::commit_changes`].
    pub commit_id: CommitId,
    /// Number of files that differ from the checked-out tip.
    pub changed_files: usize,
    /// Whether reapplying the changes onto a rewrite left `@` conflicted (the
    /// working tree then holds conflict markers in [`Self::commit_id`]'s files).
    pub has_conflict: bool,
}

/// One entry in a working-copy *chain* — the linear stack of jj commits between
/// a worktree's branch tip (exclusive; the launch worktree's is git HEAD) and its
/// working-copy commit `@` (the leaf, inclusive). A single entry in the common
/// case; the diff view's Split peels `@` into more (see
/// [`Repo::split_working_copy_edits_at`]), for any editable worktree. None of
/// these commits is exported to git.
#[derive(Debug, Clone)]
pub struct WorkingCopyEntry {
    /// The entry as a history row, subject overridden to "Uncommitted changes".
    /// Its diff against its own parent is this entry's slice of the uncommitted
    /// changes — load it with [`crate::diff::commit_changes`].
    pub info: crate::history::CommitInfo,
    /// Number of files this entry changes relative to its own parent.
    pub changed_files: usize,
    /// The changed files' paths (relative, forward-slash form), in the order
    /// [`crate::diff::commit_changes`] lists them. Length equals
    /// [`Self::changed_files`]; the UI shows a couple of basenames from it.
    pub file_names: Vec<String>,
    /// Whether this entry's tree is conflicted (a rewrite reapplied onto it
    /// clashed with the user's uncommitted changes).
    pub has_conflict: bool,
}

/// Which worktree's working copy `@` a working-copy mutation targets. Defaults to
/// the launch worktree (the classic single-worktree path); `Worktree` names an
/// *extra* editable branch checked out in another git worktree (see
/// [`crate::repo::WorktreeView`]), resolved by short-name via
/// [`Repo::find_worktree`]. Built for a branch with [`Repo::wc_target_for_branch`].
/// The whole-`@` mutations (fold / discard / edit a file / commit) and the
/// `@`-chain operations (split, per-entry commit) all accept a non-launch target,
/// so a sibling worktree's `@` chain is split and committed like the launch one's;
/// only the [`PartialSelection`]-based partial commit/squash stays launch-only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum WcTarget {
    /// The launch worktree's working copy (`self.workspace`) — the default.
    #[default]
    Launch,
    /// An extra editable worktree's working copy, by its branch short-name.
    Worktree(String),
}

/// A subset of the uncommitted changes to commit, addressed in three tiers that
/// **compose** in one call (a given path must appear in at most one tier):
/// - `paths`: take the file whole, lifting its value from the leaf `@`
///   (binary/executable-safe; a path missing on disk commits a deletion).
/// - `hunks`: per file, the change-group hunks to keep — by *index* into the
///   diff the agent read (the same numbering [`render_diff`] produces) — the rest
///   reverting to HEAD.
/// - `patches`: per file, an edited unified-diff patch applied to the file's HEAD
///   content, for sub-hunk (`git add -p` → `e`) selection.
///
/// The `hunks`/`patches` tiers reconstruct relative to **HEAD**, i.e. against the
/// cumulative `@`-vs-HEAD diff the agent saw. An all-empty selection commits
/// nothing and is rejected.
pub struct PartialSelection<'a> {
    pub paths: &'a [String],
    pub hunks: &'a [(String, Vec<usize>)],
    pub patches: &'a [(String, String)],
}

/// One commit to carve out of the uncommitted changes: its message, optional
/// identity, and the [`PartialSelection`] of the working copy it holds. Consumed
/// by [`Repo::carve_working_copy`], which chains the entries oldest-first on top
/// of HEAD and leaves the unselected remainder uncommitted.
pub struct CarveEntry<'a> {
    pub message: &'a str,
    pub identity: Option<&'a Identity>,
    pub selection: PartialSelection<'a>,
}

impl Repo {
    /// Snapshot the on-disk working directory into the working-copy commit `@`,
    /// so uncommitted changes to **tracked** files (edits and deletions) become a
    /// real commit on top of the checked-out tip. Files git treats as untracked
    /// are excluded — we never auto-track new files — and, since jj never tracks
    /// them, they survive every later checkout untouched (see the module docs). A
    /// no-op on a detached HEAD or when nothing changed since the last snapshot.
    pub fn snapshot_working_copy(&mut self) -> Result<()> {
        self.snapshot_working_copy_tracking(&[])
    }

    /// Like [`Self::snapshot_working_copy`], but additionally begins tracking the
    /// named untracked paths — new files on disk that git doesn't track yet — so a
    /// brand-new file can be folded into a commit (the snapshot otherwise captures
    /// only edits/deletions to already-tracked files). Each named path is
    /// force-tracked, so an explicitly named file is snapshotted even when a
    /// `.gitignore` rule would cover it: naming it is explicit intent. Once
    /// snapshotted into `@` the file stays tracked for the rest of the session,
    /// like `git add`, so only this first snapshot needs to name it; a path that is
    /// already tracked or absent on disk is a harmless no-op.
    pub fn snapshot_working_copy_tracking(&mut self, add_paths: &[String]) -> Result<()> {
        // First snapshot every *extra* worktree (independent of the launch branch's
        // worktree state — an off-worktree session can still have editable branches
        // that *are* checked out elsewhere), so each worktree's uncommitted changes
        // ride through the rewrite this snapshot precedes.
        self.snapshot_extra_worktrees()?;
        // Off-worktree there is no launch working copy to snapshot: the on-disk tree
        // belongs to a different (checked-out) branch and must not be folded into
        // the edited branch's `@`. A silent no-op keeps the callers that snapshot
        // defensively (open, session_changes, every mutation) working unchanged.
        if !self.is_worktree_bound() {
            return Ok(());
        }
        // Catch up first if the caller moved git HEAD out of band (a plain
        // `git commit`): jj's view must contain the new tip before we can attach
        // `@` to it or rebase onto it. A no-op when already in sync.
        self.sync_to_git_head()?;
        // `@` must sit directly on the current tip, or its diff would be the
        // whole history rather than the uncommitted delta.
        self.ensure_working_copy_on_head()?;
        let name = self.workspace.workspace_name().to_owned();

        let extra: Vec<RepoPathBuf> = add_paths
            .iter()
            .map(|p| {
                RepoPathBuf::from_internal_string(p)
                    .with_context(|| format!("invalid path '{p}' in add_paths"))
            })
            .collect::<Result<_>>()?;

        // Auto-track the files git already tracks — the paths present in the tip
        // the working copy sits on (`@`'s parent, i.e. HEAD) — plus any caller-named
        // `extra` new files. commedit's throwaway jj workspace starts with an
        // *empty* on-disk tree state, so to the first snapshot every file on disk
        // looks brand-new: "track nothing" would drop even committed files out of
        // `@`, and "track everything" would pull in git's untracked files. Matching
        // the base tree's paths tracks exactly the committed files (capturing their
        // edits/deletions) while leaving files absent from it — git's untracked
        // files — out of `@`, unless the caller opted one in via `extra`.
        let tracked = self.tracked_paths_matcher(&extra)?;
        let base_ignores = self.base_ignores()?;
        let options = SnapshotOptions {
            base_ignores,
            progress: None,
            start_tracking_matcher: tracked.as_ref(),
            // Force-track exactly the files git already tracks. git's ignore
            // rules never apply to files already in the index, so a tracked file
            // inside a `.gitignore`d directory (e.g. a `.keep` under an ignored
            // `m4/`) must still be snapshotted into `@`. Without this jj would
            // skip the ignored directory — our throwaway workspace's tree state
            // starts empty, so there's nothing for jj's "visit only tracked
            // files" path to find — and the file would surface as a phantom
            // (deleted) uncommitted change. New, untracked files aren't in this
            // set unless the caller named them via `add_paths`, in which case
            // force-tracking is exactly what pulls them in past any ignore rule.
            force_tracking_matcher: tracked.as_ref(),
            max_new_file_size: u64::MAX,
        };

        let mut locked_ws = block_on(self.workspace.start_working_copy_mutation())
            .context("locking the working copy")?;
        let (new_tree, _stats) = block_on(locked_ws.locked_wc().snapshot(&options))
            .context("snapshotting the working copy")?;

        if let Some(wc_id) = self.repo.view().get_wc_commit_id(&name).cloned() {
            let wc = self
                .repo
                .store()
                .get_commit(&wc_id)
                .context("loading the working-copy commit")?;
            // Only rewrite `@` when the disk actually changed, to avoid churning
            // its commit id on every snapshot.
            if wc.tree().tree_ids_and_labels() != new_tree.tree_ids_and_labels() {
                let mut tx = self.repo.start_transaction();
                block_on(tx.repo_mut().rewrite_commit(&wc).set_tree(new_tree).write())
                    .context("recording the working-copy snapshot")?;
                block_on(tx.repo_mut().rebase_descendants()).context("rebasing after snapshot")?;
                self.repo = block_on(tx.commit("commedit: snapshot working copy"))
                    .context("committing the working-copy snapshot")?;
            }
        }

        let op_id = self.repo.operation().id().clone();
        block_on(locked_ws.finish(op_id)).context("saving working-copy state")?;
        Ok(())
    }

    /// The snapshot's `start_tracking_matcher`: a matcher over exactly the paths
    /// git already tracks — the files present in the tip the working copy sits on
    /// (`@`'s single parent, normally HEAD; a detached HEAD still resolves to its
    /// commit, so `@` is reattached there) — unioned with any `extra` paths the
    /// caller opted in (new files to begin tracking). See
    /// [`Self::snapshot_working_copy`] for why a path-set (rather than "everything"
    /// or "nothing") is needed. Falls back to [`EverythingMatcher`] — the
    /// pre-exclusion behaviour, which already covers any `extra` — only when there
    /// is no `@`, or it sits on the empty root with no single parent to read a base
    /// tree from (a repo with no commits). That fallback is safe: it only risks
    /// over-tracking, never dropping a committed file.
    fn tracked_paths_matcher(&self, extra: &[RepoPathBuf]) -> Result<Box<dyn Matcher>> {
        self.tracked_paths_matcher_for(self.working_copy_commit_id(), extra)
    }

    /// [`Self::tracked_paths_matcher`] for an explicit working-copy commit `@`
    /// (rather than the launch workspace's), so an extra worktree's snapshot tracks
    /// exactly the files its own branch tip holds.
    fn tracked_paths_matcher_for(
        &self,
        wc_id: Option<CommitId>,
        extra: &[RepoPathBuf],
    ) -> Result<Box<dyn Matcher>> {
        let Some(wc_id) = wc_id else {
            return Ok(Box::new(EverythingMatcher));
        };
        let wc = self
            .repo
            .store()
            .get_commit(&wc_id)
            .context("loading the working-copy commit")?;
        let parents = wc.parent_ids();
        if parents.len() != 1 {
            return Ok(Box::new(EverythingMatcher));
        }
        let base = self
            .repo
            .store()
            .get_commit(&parents[0])
            .context("loading the working-copy base commit")?;
        let mut paths: Vec<RepoPathBuf> = base.tree().entries().map(|(path, _)| path).collect();
        paths.extend(extra.iter().cloned());
        Ok(Box::new(FilesMatcher::new(paths)))
    }

    /// Snapshot every extra worktree (see [`Self::snapshot_extra_worktree`]) before
    /// a mutation. First catches each worktree up to any out-of-band `git commit`
    /// on its branch ([`Self::catch_up_extra_worktrees`]), then snapshots: the
    /// worktree list is temporarily moved out so each snapshot can borrow `&mut self`
    /// and the view together; it is always restored (including on error). A no-op in
    /// the classic singleton path (no extra worktrees).
    pub(crate) fn snapshot_extra_worktrees(&mut self) -> Result<()> {
        if self.extra_worktrees.is_empty() {
            return Ok(());
        }
        self.catch_up_extra_worktrees()?;
        let mut views = std::mem::take(&mut self.extra_worktrees);
        let result = views
            .iter_mut()
            .try_for_each(|view| self.snapshot_extra_worktree(view));
        self.extra_worktrees = views;
        result
    }

    /// Snapshot one *extra* worktree's on-disk tree into its own `@` (keyed by the
    /// view's per-worktree workspace name), so its uncommitted changes are recorded
    /// and ride through a later `rebase_descendants` exactly like the launch
    /// worktree's. The per-worktree analogue of [`Self::snapshot_working_copy`],
    /// minus the launch-only *open-time* chain reconciliation
    /// ([`Self::collapse_working_copy_chain`]) — a sibling carries a full `@` chain
    /// like the launch one, kept intact by the chain-aware re-anchor below. First
    /// re-anchors `@` onto the branch tip
    /// ([`Self::reanchor_extra_worktree`]) so an out-of-band `git commit` caught up
    /// by [`Self::catch_up_extra_worktrees`] lands on the branch rather than as a
    /// phantom uncommitted change — a no-op in the common case where `@` already
    /// sits on the tip. A no-op overall when its disk matches the last snapshot.
    pub(crate) fn snapshot_extra_worktree(
        &mut self,
        view: &mut crate::repo::WorktreeView,
    ) -> Result<()> {
        // Catch @ up if its branch tip moved out from under it (a plain `git commit`
        // in this worktree, imported by snapshot_extra_worktrees) so the snapshot
        // below records only the still-uncommitted delta, not the committed change.
        self.reanchor_extra_worktree(view)?;
        let name = view.name.clone();
        let wc_id = self.repo.view().get_wc_commit_id(&name).cloned();
        let tracked = self.tracked_paths_matcher_for(wc_id.clone(), &[])?;
        let base_ignores = self.base_ignores_at(view.workspace.workspace_root())?;
        let options = SnapshotOptions {
            base_ignores,
            progress: None,
            start_tracking_matcher: tracked.as_ref(),
            force_tracking_matcher: tracked.as_ref(),
            max_new_file_size: u64::MAX,
        };

        let mut locked_ws = block_on(view.workspace.start_working_copy_mutation())
            .context("locking the worktree working copy")?;
        let (new_tree, _stats) = block_on(locked_ws.locked_wc().snapshot(&options))
            .context("snapshotting the worktree working copy")?;

        if let Some(wc_id) = wc_id {
            let wc = self
                .repo
                .store()
                .get_commit(&wc_id)
                .context("loading the worktree working-copy commit")?;
            if wc.tree().tree_ids_and_labels() != new_tree.tree_ids_and_labels() {
                let mut tx = self.repo.start_transaction();
                block_on(tx.repo_mut().rewrite_commit(&wc).set_tree(new_tree).write())
                    .context("recording the worktree working-copy snapshot")?;
                block_on(tx.repo_mut().rebase_descendants())
                    .context("rebasing after worktree snapshot")?;
                self.repo = block_on(tx.commit("commedit: snapshot worktree working copy"))
                    .context("committing the worktree working-copy snapshot")?;
            }
        }
        let op_id = self.repo.operation().id().clone();
        block_on(locked_ws.finish(op_id)).context("saving worktree working-copy state")?;
        Ok(())
    }

    /// Reparent an extra worktree's `@` onto its branch's current tip when the tip
    /// moved out from under it — e.g. a plain `git commit` in that worktree, caught
    /// up by [`Repo::catch_up_extra_worktrees`]'s import. A fresh empty `@` is
    /// checked out on the tip (a jj-view pointer move only): the previous `@` and
    /// its delta is abandoned, but the worktree's on-disk content is untouched, so
    /// the snapshot that follows re-records the still-uncommitted delta against the
    /// new tip. The per-worktree analogue of [`Self::ensure_working_copy_on_head`];
    /// a no-op when `@` already sits directly on the tip (the common case, including
    /// right after registration).
    fn reanchor_extra_worktree(&mut self, view: &crate::repo::WorktreeView) -> Result<()> {
        let short = view
            .branch
            .strip_prefix("refs/heads/")
            .unwrap_or(&view.branch);
        let bookmark: jj_lib::ref_name::RefNameBuf = short.into();
        let Some(tip) = self
            .repo
            .view()
            .get_local_bookmark(&bookmark)
            .as_normal()
            .cloned()
        else {
            return Ok(());
        };
        // Already anchored on a clean chain descending from the tip → nothing to
        // do. Chain-aware (not just `@`'s direct parent), so a sibling split chain
        // (`tip → @' → @`) survives this re-anchor instead of being collapsed back
        // to a single `@` on the very next snapshot; only a genuinely detached `@`
        // or an out-of-band tip move still falls through to the re-checkout below.
        if self.wc_on_tip(&view.name, &tip)? {
            return Ok(());
        }
        let tip_commit = self
            .repo
            .store()
            .get_commit(&tip)
            .context("loading the worktree branch tip")?;
        let mut tx = self.repo.start_transaction();
        block_on(tx.repo_mut().check_out(view.name.clone(), &tip_commit))
            .context("re-anchoring the worktree working copy on its tip")?;
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing after re-anchor")?;
        self.repo = block_on(tx.commit("commedit: re-anchor worktree working copy"))
            .context("committing the worktree re-anchor")?;
        Ok(())
    }

    /// Re-materialize every extra worktree whose branch tip moved across the
    /// rewrite — its bridged git ref now differs from its pre-rewrite oid in
    /// `before` ([`Repo::snapshot_heads`]). A worktree whose branch was untouched is
    /// left frozen on disk. Runs in the export tail *after* the editable bookmarks
    /// have been bridged to git, so each worktree's ref already holds its new tip.
    /// A no-op in the classic singleton path (no extra worktrees).
    pub(crate) fn materialize_moved_worktrees(
        &mut self,
        before: &std::collections::BTreeMap<String, String>,
    ) -> Result<()> {
        if self.extra_worktrees.is_empty() {
            return Ok(());
        }
        let mut views = std::mem::take(&mut self.extra_worktrees);
        let result = views.iter_mut().try_for_each(|view| {
            let root = view.workspace.workspace_root();
            let new_tip = crate::transparency::ref_commit(root, &view.branch);
            let old_tip = before.get(&view.branch).map(String::as_str);
            if new_tip.as_deref() == old_tip {
                return Ok(()); // this worktree's branch was untouched: leave it frozen
            }
            self.materialize_extra_worktree(view)
        });
        self.extra_worktrees = views;
        result
    }

    /// Re-materialize every extra worktree whose `@` id changed across a rewind but
    /// whose branch tip did *not* move — the gap [`Self::materialize_moved_worktrees`]
    /// (tip-gated, run inside `export_and_sync`) leaves for an `@`-only sibling op
    /// (edit/discard uncommitted): undo/redo restores the sibling `@` in jj, but the
    /// tip-gate skips the on-disk re-checkout, so the worktree's files would stay
    /// stale. `before_tips` ([`Repo::snapshot_heads`]) and `before_wc` are the
    /// pre-rewind branch tips and `@` ids, both keyed by full ref name. A worktree
    /// whose tip moved was already re-checked-out by the tip gate, so it is skipped
    /// here to avoid a redundant checkout. Used only by the rewind path (the
    /// general export tail's tip gate is correct there — see [`crate::conflict`]); a
    /// no-op in the classic singleton path (no extra worktrees).
    pub(crate) fn materialize_changed_worktrees(
        &mut self,
        before_tips: &std::collections::BTreeMap<String, String>,
        before_wc: &std::collections::BTreeMap<String, CommitId>,
    ) -> Result<()> {
        if self.extra_worktrees.is_empty() {
            return Ok(());
        }
        let mut views = std::mem::take(&mut self.extra_worktrees);
        let result = views.iter_mut().try_for_each(|view| {
            let root = view.workspace.workspace_root();
            let new_tip = crate::transparency::ref_commit(root, &view.branch);
            if new_tip.as_deref() != before_tips.get(&view.branch).map(String::as_str) {
                return Ok(()); // tip moved: the tip gate already re-materialized it
            }
            let new_wc = self.repo.view().get_wc_commit_id(&view.name).cloned();
            if new_wc.as_ref() == before_wc.get(&view.branch) {
                return Ok(()); // @ unchanged: leave the worktree frozen
            }
            self.materialize_extra_worktree(view)
        });
        self.extra_worktrees = views;
        result
    }

    /// Materialize one extra worktree's rebased `@'` back to its own on-disk root
    /// and reset *its* git index to its branch tip, so that worktree's `git status`
    /// reflects the rewrite while preserving its uncommitted changes. The
    /// per-worktree analogue of [`Self::materialize_after_rewrite`]; called only for
    /// a worktree whose branch tip actually moved (see [`crate::conflict`]'s export
    /// tail). Like the launch path, any index-only staged content (staged then
    /// reverted/removed on disk, so invisible to jj's `@`) is pinned to a recovery
    /// ref before the index reset would drop it — namespaced by a per-worktree key
    /// so the launch's and each sibling's recovery points don't evict one another in
    /// the shared common-dir.
    pub(crate) fn materialize_extra_worktree(
        &mut self,
        view: &mut crate::repo::WorktreeView,
    ) -> Result<()> {
        let name = view.name.clone();
        let Some(wc_id) = self.repo.view().get_wc_commit_id(&name).cloned() else {
            return Ok(());
        };
        let commit = self
            .repo
            .store()
            .get_commit(&wc_id)
            .context("loading the worktree commit to check out")?;
        let op_id = self.repo.operation().id().clone();
        block_on(view.workspace.check_out(op_id, None, &commit))
            .context("checking out the worktree working copy")?;
        // Reset that worktree's index to its branch's new tip, so its `git status`
        // shows the preserved uncommitted changes against the rewritten history.
        let root = view.workspace.workspace_root();
        let key = crate::transparency::worktree_backup_key(root);
        let _ = crate::transparency::backup_index_only_content_at(root, &key);
        if let Some(new_tip) = crate::transparency::ref_commit(root, &view.branch) {
            crate::transparency::reset_index_to(root, &new_tip)?;
        }
        crate::transparency::prune_backup_refs_at(root, &key);
        Ok(())
    }

    /// Write `target`'s tree to the working directory (and update jj's
    /// working-copy state), replacing the `git read-tree` worktree sync. Used to
    /// materialize the rebased `@` after a rewrite.
    pub fn materialize_working_copy(&mut self, target: &CommitId) -> Result<CheckoutStats> {
        let commit = self
            .repo
            .store()
            .get_commit(target)
            .context("loading the commit to check out")?;
        let op_id = self.repo.operation().id().clone();
        block_on(self.workspace.check_out(op_id, None, &commit))
            .context("checking out the working copy")
    }

    /// Edit a file of a working-copy entry through the diff pane: splice
    /// `new_content` into the entry's tree and write the result to disk — like
    /// editing any commit, but the branch tip doesn't move (no history export).
    /// `new_content` of `None` removes the path (reverting a file the entry adds);
    /// `Some` writes it (editing, or restoring a file the entry deletes).
    /// `change_hex` selects which uncommitted entry to edit (its stable change id,
    /// resolved after snapshotting); `None` targets the leaf `@`. Snapshots the
    /// disk first so a concurrent external edit to another file isn't clobbered.
    pub fn edit_working_copy_file(
        &mut self,
        change_hex: Option<&str>,
        path: &str,
        new_content: Option<&str>,
    ) -> Result<()> {
        self.edit_working_copy_file_at(WcTarget::Launch, change_hex, path, new_content)
    }

    /// Like [`Self::edit_working_copy_file`] but on `target`'s worktree `@` — used
    /// to edit a sibling worktree's uncommitted changes from the unified DAG.
    pub fn edit_working_copy_file_at(
        &mut self,
        target: WcTarget,
        change_hex: Option<&str>,
        path: &str,
        new_content: Option<&str>,
    ) -> Result<()> {
        self.require_wc_target(&target, "edit the working copy")?;
        crate::repo::catch_jj("editing the working copy", || {
            self.edit_working_copy_file_inner(&target, change_hex, path, new_content)
        })
    }

    fn edit_working_copy_file_inner(
        &mut self,
        target: &WcTarget,
        change_hex: Option<&str>,
        path: &str,
        new_content: Option<&str>,
    ) -> Result<()> {
        self.snapshot_wc(target)?;
        let wc_id = self
            .resolve_wc(target, change_hex)
            .context("no working copy to edit")?;
        let commit = self
            .repo
            .store()
            .get_commit(&wc_id)
            .context("loading the working-copy commit")?;
        let change_hex = commit.change_id().hex();
        let repo_path = RepoPathBuf::from_internal_string(path).context("invalid path")?;
        let base_tree = commit.tree();

        let store = self.repo.store().clone();
        let value: MergedTreeValue = match new_content {
            Some(content) => {
                let (executable, copy_id) = crate::tree::existing_file_meta(&base_tree, &repo_path);
                let mut reader: &[u8] = content.as_bytes();
                let file_id = block_on(store.write_file(&repo_path, &mut reader))
                    .context("writing file blob")?;
                Merge::normal(TreeValue::File {
                    id: file_id,
                    executable,
                    copy_id,
                })
            }
            // Absent value: remove the path from the entry's tree.
            None => Merge::absent(),
        };
        let mut builder = MergedTreeBuilder::new(base_tree);
        builder.set_or_remove(repo_path, value);
        let new_tree = block_on(builder.write_tree()).context("writing tree")?;

        let mut tx = self.repo.start_transaction();
        block_on(
            tx.repo_mut()
                .rewrite_commit(&commit)
                .set_tree(new_tree)
                .write(),
        )
        .context("rewriting the working-copy commit")?;
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing after edit")?;
        self.repo = block_on(tx.commit("commedit: edit working copy"))
            .context("committing the working-copy edit")?;

        // Write the edited @ to disk (the branch tip is unchanged).
        self.materialize_wc(target)?;
        self.record_working_copy_op(target, "Edit uncommitted changes", change_hex);
        Ok(())
    }

    /// Crystallize the current uncommitted changes into a real commit on top of
    /// HEAD with `message` (and optional `identity`, defaulting to the repo's
    /// git-configured user at "now"), then start a fresh empty working copy so the
    /// tree ends up clean — like `git commit -a`. Unlike the working-copy-direct
    /// edits (split/discard/edit) this *moves the branch tip*, so it exports to git
    /// through the shared `finish_mutation` tail. Always lands clean: the new
    /// commit is a fresh tip with no descendants to rebase. Refuses when the tree
    /// is clean (nothing to commit) or HEAD is detached/unborn.
    pub fn commit_working_copy(
        &mut self,
        message: &str,
        identity: Option<&Identity>,
    ) -> Result<SaveOutcome> {
        self.commit_working_copy_at(WcTarget::Launch, message, identity)
    }

    /// Like [`Self::commit_working_copy`] but crystallizes `target`'s worktree `@`
    /// into a new commit on *that* branch's tip — used to commit a sibling
    /// worktree's uncommitted changes from the unified DAG.
    pub fn commit_working_copy_at(
        &mut self,
        target: WcTarget,
        message: &str,
        identity: Option<&Identity>,
    ) -> Result<SaveOutcome> {
        self.require_wc_target(&target, "commit the working copy")?;
        crate::repo::catch_jj("committing the working copy", || {
            self.commit_working_copy_inner(&target, message, identity)
        })
    }

    fn commit_working_copy_inner(
        &mut self,
        target: &WcTarget,
        message: &str,
        identity: Option<&Identity>,
    ) -> Result<SaveOutcome> {
        // Fold the on-disk changes into the @ first, then refuse if the tree turned
        // out clean (nothing to commit).
        self.snapshot_wc(target)?;
        let leaf_id = self
            .resolve_wc(target, None)
            .context("no working copy to commit")?;
        if self.wc_entry_for(&leaf_id).is_none() {
            bail!("no uncommitted changes to commit");
        }
        let Some(head) = self.wc_tip(target) else {
            bail!("the repository has no branch head; cannot commit the working copy");
        };
        let store = self.repo.store().clone();
        // The @ holds the full on-disk tree, so committing it on the branch tip
        // captures every uncommitted change as one commit.
        let tree = store
            .get_commit(&leaf_id)
            .context("loading the working-copy commit")?
            .tree();
        let name = self
            .wc_workspace_name(target)
            .context("the target worktree has no workspace")?;

        let pre_op = self.repo.operation().clone();
        let old_head = self.edited_tip();
        let heads = self.snapshot_heads();

        let mut tx = self.repo.start_transaction();
        let mut builder = tx
            .repo_mut()
            .new_commit(vec![head.clone()], tree)
            .set_description(message);
        if let Some(id) = identity {
            let author = Signature {
                name: id.author_name.clone(),
                email: id.author_email.clone(),
                timestamp: parse_timestamp(&id.author_time).context("author date")?,
            };
            let committer = Signature {
                name: id.committer_name.clone(),
                email: id.committer_email.clone(),
                timestamp: parse_timestamp(&id.committer_time).context("committer date")?,
            };
            builder = builder.set_author(author).set_committer(committer);
        }
        let created = block_on(builder.write()).context("writing the commit")?;
        let created_id = created.id().clone();
        let change_hex = created.change_id().hex();

        // Start a fresh empty @ on the new commit; check_out abandons the old @
        // (and any split chain above HEAD), so the working tree ends up clean.
        block_on(tx.repo_mut().check_out(name, &created))
            .context("starting a fresh working copy")?;
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;
        self.set_target_bookmark(tx.repo_mut(), target, created_id);

        let subject = message.lines().next().unwrap_or("").trim();
        let label = if subject.is_empty() {
            "Commit working copy".to_string()
        } else {
            format!("Commit \"{subject}\"")
        };
        let desc = OpDescriptor::new(label, vec![change_hex]);
        self.finish_mutation(
            tx,
            "commedit: commit working copy",
            desc,
            pre_op,
            old_head,
            heads,
        )
    }

    /// Commit a **subset** of the uncommitted changes as a new commit on HEAD,
    /// keeping the remainder uncommitted — the in-process equivalent of
    /// `git add -p` + `git commit` (which jj's "snapshot the whole tree" model has
    /// no concept of). Like [`Self::commit_working_copy`] this moves the branch tip
    /// and exports through the shared `finish_mutation` tail, and always lands
    /// clean (the new commit is a fresh tip with no real-history descendants to
    /// rebase). Unlike it, the rebuilt `@` still holds the **full** on-disk tree,
    /// so the working files stay byte-identical — only `git`'s notion of what is
    /// committed vs. uncommitted moves. Refuses when the tree is clean, when HEAD
    /// is detached/unborn, or when the selection turns out to commit nothing. See
    /// [`PartialSelection`] for how the selection is addressed.
    pub fn commit_working_copy_partial(
        &mut self,
        sel: PartialSelection<'_>,
        message: &str,
        identity: Option<&Identity>,
    ) -> Result<SaveOutcome> {
        self.require_worktree("commit part of the working copy")?;
        crate::repo::catch_jj("committing part of the working copy", || {
            self.commit_working_copy_partial_inner(sel, message, identity)
        })
    }

    fn commit_working_copy_partial_inner(
        &mut self,
        sel: PartialSelection<'_>,
        message: &str,
        identity: Option<&Identity>,
    ) -> Result<SaveOutcome> {
        // Snapshot the disk and build the selected subset's tree (`t_commit`) plus
        // the full on-disk tree for the remainder — shared with the partial squash.
        let (head, _head_tree, full_tree, t_commit) = self.prepare_partial_commit(&sel)?;

        let name = self.workspace.workspace_name().to_owned();
        let pre_op = self.repo.operation().clone();
        let old_head = self.edited_tip();
        let heads = self.snapshot_heads();

        let mut tx = self.repo.start_transaction();
        // C: the partial commit, holding only the selected subset, on HEAD.
        let mut builder = tx
            .repo_mut()
            .new_commit(vec![head.clone()], t_commit)
            .set_description(message);
        if let Some(id) = identity {
            let author = Signature {
                name: id.author_name.clone(),
                email: id.author_email.clone(),
                timestamp: parse_timestamp(&id.author_time).context("author date")?,
            };
            let committer = Signature {
                name: id.committer_name.clone(),
                email: id.committer_email.clone(),
                timestamp: parse_timestamp(&id.committer_time).context("committer date")?,
            };
            builder = builder.set_author(author).set_committer(committer);
        }
        let created = block_on(builder.write()).context("writing the commit")?;
        let created_id = created.id().clone();
        let change_hex = created.change_id().hex();

        // leaf': the remainder — the full on-disk tree as a child of C. We point @
        // at it with `edit` (not `check_out`, which would spawn a *fresh empty* @):
        // @ must *hold* the full tree so disk stays byte-identical and the
        // unselected delta (leaf' vs C) remains the uncommitted changes.
        let remainder = block_on(
            tx.repo_mut()
                .new_commit(vec![created_id.clone()], full_tree)
                .write(),
        )
        .context("writing the working-copy remainder")?;
        block_on(tx.repo_mut().edit(name, &remainder))
            .context("pointing the working copy at the remainder")?;
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;
        self.set_head_bookmark(tx.repo_mut(), created_id);

        let subject = message.lines().next().unwrap_or("").trim();
        let label = if subject.is_empty() {
            "Commit part of working copy".to_string()
        } else {
            format!("Commit \"{subject}\"")
        };
        let desc = OpDescriptor::new(label, vec![change_hex]);
        self.finish_mutation(
            tx,
            "commedit: commit working copy (partial)",
            desc,
            pre_op,
            old_head,
            heads,
        )
    }

    /// Carve the uncommitted changes into **several** commits in one transaction,
    /// each holding its own [`PartialSelection`] of the working copy, stacked
    /// oldest-first on top of HEAD; whatever no entry selects stays uncommitted.
    ///
    /// This is the batch form of [`Self::commit_working_copy_partial`]: every
    /// entry addresses the *same* `@`-vs-HEAD diff (the one the caller already
    /// read), so hunk indices are stable across entries — the index-shift hazard
    /// of committing one-subset-at-a-time (each commit moving HEAD and reshaping
    /// the remaining diff) doesn't arise. Each commit `Ci` holds HEAD's tree plus
    /// the cumulative selection of entries `1..=i`, so its own diff is exactly its
    /// entry's selection; the remainder `@` carries the full on-disk tree so disk
    /// stays byte-identical. Always lands clean (fresh commits on the tip). Returns
    /// the save outcome and the new commits' change ids (oldest-first, `C1..Cn`).
    ///
    /// A path may be selected by more than one entry only via the `hunks` tier
    /// with disjoint indices; a whole-file (`paths`) or `patches` selection of a
    /// path must be unique across the whole carve. Refuses an empty entry list, an
    /// entry that selects nothing, a clean tree, or a detached/unborn HEAD.
    pub fn carve_working_copy(
        &mut self,
        entries: &[CarveEntry<'_>],
    ) -> Result<(SaveOutcome, Vec<String>)> {
        self.require_worktree("carve the working copy")?;
        crate::repo::catch_jj("carving the working copy", || {
            self.carve_working_copy_inner(entries)
        })
    }

    fn carve_working_copy_inner(
        &mut self,
        entries: &[CarveEntry<'_>],
    ) -> Result<(SaveOutcome, Vec<String>)> {
        validate_carve(entries)?;

        // Snapshot + fetch HEAD/leaf trees once, and confirm the whole carve
        // commits something (the cumulative selection over every entry).
        let full_sel = cumulative_selection(entries, entries.len());
        let (head, head_tree, full_tree, _t) = self.prepare_partial_commit(&PartialSelection {
            paths: &full_sel.0,
            hunks: &full_sel.1,
            patches: &full_sel.2,
        })?;
        let store = self.repo.store().clone();

        // Build each commit's cumulative tree (HEAD + selections 1..=i) and check
        // every entry adds something over the previous one.
        let mut trees: Vec<MergedTree> = Vec::with_capacity(entries.len());
        let mut prev = head_tree.clone();
        for (i, entry) in entries.iter().enumerate() {
            let sel = cumulative_selection(entries, i + 1);
            let ti = self.splice_selection_onto(
                &head_tree,
                &full_tree,
                &store,
                &PartialSelection {
                    paths: &sel.0,
                    hunks: &sel.1,
                    patches: &sel.2,
                },
            )?;
            if ti.tree_ids() == prev.tree_ids() {
                bail!(
                    "carve commit {} ('{}') selects nothing beyond the earlier commits",
                    i + 1,
                    entry.message.lines().next().unwrap_or("").trim()
                );
            }
            prev = ti.clone();
            trees.push(ti);
        }
        let cn_tree = trees.last().expect("validated non-empty").clone();

        let name = self.workspace.workspace_name().to_owned();
        let pre_op = self.repo.operation().clone();
        let old_head = self.edited_tip();
        let heads = self.snapshot_heads();

        let mut tx = self.repo.start_transaction();
        let mut parent = head.clone();
        let mut change_hexes: Vec<String> = Vec::with_capacity(entries.len());
        let mut last = None;
        for (entry, tree) in entries.iter().zip(&trees) {
            let mut builder = tx
                .repo_mut()
                .new_commit(vec![parent.clone()], tree.clone())
                .set_description(entry.message);
            if let Some(id) = entry.identity {
                let author = Signature {
                    name: id.author_name.clone(),
                    email: id.author_email.clone(),
                    timestamp: parse_timestamp(&id.author_time).context("author date")?,
                };
                let committer = Signature {
                    name: id.committer_name.clone(),
                    email: id.committer_email.clone(),
                    timestamp: parse_timestamp(&id.committer_time).context("committer date")?,
                };
                builder = builder.set_author(author).set_committer(committer);
            }
            let created = block_on(builder.write()).context("writing a carve commit")?;
            parent = created.id().clone();
            change_hexes.push(created.change_id().hex());
            last = Some(created);
        }
        let last = last.expect("validated non-empty");

        // The remainder — the full on-disk tree as a child of the last commit,
        // holding whatever no entry selected. When the carve committed the whole
        // tree, start a fresh empty `@` instead (like commit_working_copy).
        if full_tree.tree_ids() == cn_tree.tree_ids() {
            block_on(tx.repo_mut().check_out(name, &last))
                .context("starting a fresh working copy")?;
        } else {
            let remainder = block_on(
                tx.repo_mut()
                    .new_commit(vec![last.id().clone()], full_tree.clone())
                    .write(),
            )
            .context("writing the working-copy remainder")?;
            block_on(tx.repo_mut().edit(name, &remainder))
                .context("pointing the working copy at the remainder")?;
        }
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;
        self.set_head_bookmark(tx.repo_mut(), last.id().clone());

        let desc = OpDescriptor::new(
            format!("Carve into {} commit(s)", entries.len()),
            change_hexes.clone(),
        );
        let outcome = self.finish_mutation(
            tx,
            "commedit: carve working copy",
            desc,
            pre_op,
            old_head,
            heads,
        )?;
        Ok((outcome, change_hexes))
    }

    /// Crystallize a single working-copy **entry** (identified by its stable change
    /// id, or the leaf `@` when `change_hex` is `None`) into a real commit on top of
    /// HEAD — committing exactly that entry's slice of the uncommitted changes (its
    /// diff against its own parent) and leaving every *other* entry of the chain
    /// uncommitted. This is what the GTK working-copy view's Save commits: the diff
    /// it shows is the selected entry's, so a chain peeled apart with
    /// [`Self::split_working_copy`] commits one piece at a time, the rest staying as
    /// "uncommitted changes" rows. The full on-disk tree rides on the new commit as
    /// the rebuilt `@` (disk stays byte-identical, only git's committed-vs-
    /// uncommitted line moves), collapsing to a fresh empty `@` when nothing else is
    /// left — so a lone entry behaves exactly like [`Self::commit_working_copy`].
    /// Like the partial commit it moves the branch tip and exports through
    /// `finish_mutation`, always landing clean. Refuses when the tree is clean, HEAD
    /// is detached/unborn, or the entry commits nothing.
    pub fn commit_working_copy_entry(
        &mut self,
        change_hex: Option<&str>,
        message: &str,
        identity: Option<&Identity>,
    ) -> Result<SaveOutcome> {
        self.commit_working_copy_entry_at(WcTarget::Launch, change_hex, message, identity)
    }

    /// Like [`Self::commit_working_copy_entry`] but crystallizes a single entry of
    /// `target`'s worktree `@` chain — the launch worktree's or a *sibling*
    /// editable worktree's (see [`WcTarget`]) — onto *that* branch's tip, leaving
    /// the chain's other entries uncommitted. Used by the GTK working-copy view to
    /// commit one peeled-apart slice of a sibling worktree's uncommitted changes; a
    /// lone sibling entry collapses to a fresh empty `@`, behaving like
    /// [`Self::commit_working_copy_at`].
    pub fn commit_working_copy_entry_at(
        &mut self,
        target: WcTarget,
        change_hex: Option<&str>,
        message: &str,
        identity: Option<&Identity>,
    ) -> Result<SaveOutcome> {
        self.require_wc_target(&target, "commit a working-copy entry")?;
        crate::repo::catch_jj("committing a working-copy entry", || {
            self.commit_working_copy_entry_inner(&target, change_hex, message, identity)
        })
    }

    fn commit_working_copy_entry_inner(
        &mut self,
        target: &WcTarget,
        change_hex: Option<&str>,
        message: &str,
        identity: Option<&Identity>,
    ) -> Result<SaveOutcome> {
        // Fold the on-disk changes into the leaf @ first, then refuse if the tree
        // turned out clean (nothing to commit).
        self.snapshot_wc(target)?;
        let leaf_id = self
            .resolve_wc(target, None)
            .context("no working copy to commit")?;
        if self.wc_entry_for(&leaf_id).is_none() {
            bail!("no uncommitted changes to commit");
        }
        let Some(head) = self.wc_tip(target) else {
            bail!("the repository has no branch head; cannot commit the working copy");
        };
        let entry_id = self
            .resolve_wc(target, change_hex)
            .context("no working copy entry to commit")?;
        let store = self.repo.store().clone();
        let entry_tree = store
            .get_commit(&entry_id)
            .context("loading the working-copy entry")?
            .tree();
        let head_tree = store
            .get_commit(&head)
            .context("loading the branch head")?
            .tree();
        // The leaf @ holds the full on-disk tree (the whole chain collapsed); it
        // becomes the remainder riding on the new commit.
        let full_tree = store
            .get_commit(&leaf_id)
            .context("loading the working-copy commit")?
            .tree();

        // The entry's slice is exactly the paths it changes against its own parent;
        // splice the entry's content for those paths onto HEAD so the commit holds
        // only the displayed diff, never the cumulative chain below it.
        let changed: Vec<String> = crate::diff::commit_changes(&self.repo, &entry_id)
            .map(|c| c.into_iter().map(|f| f.path).collect())
            .unwrap_or_default();
        let t_commit =
            crate::tree::splice_paths_from_tree(head_tree.clone(), &entry_tree, &changed)?;
        if t_commit.tree_ids() == head_tree.tree_ids() {
            bail!("the selected entry commits nothing");
        }

        let name = self
            .wc_workspace_name(target)
            .context("the target worktree has no workspace")?;
        let pre_op = self.repo.operation().clone();
        let old_head = self.edited_tip();
        let heads = self.snapshot_heads();

        let mut tx = self.repo.start_transaction();
        // C: the new commit on the target branch's tip, holding just the entry's slice.
        let mut builder = tx
            .repo_mut()
            .new_commit(vec![head.clone()], t_commit.clone())
            .set_description(message);
        if let Some(id) = identity {
            let author = Signature {
                name: id.author_name.clone(),
                email: id.author_email.clone(),
                timestamp: parse_timestamp(&id.author_time).context("author date")?,
            };
            let committer = Signature {
                name: id.committer_name.clone(),
                email: id.committer_email.clone(),
                timestamp: parse_timestamp(&id.committer_time).context("committer date")?,
            };
            builder = builder.set_author(author).set_committer(committer);
        }
        let created = block_on(builder.write()).context("writing the commit")?;
        let created_id = created.id().clone();
        let change_hex = created.change_id().hex();

        // The remainder — the full on-disk tree as a child of C, holding every
        // *other* entry's changes still uncommitted. When the committed entry was
        // the whole tree (a lone entry), the remainder equals C's tree, so start a
        // fresh empty @ instead (like `commit_working_copy`); otherwise point @ at
        // the remainder with `edit` (not `check_out`, which would spawn a fresh
        // empty @) so disk stays byte-identical and the remainder stays uncommitted.
        if full_tree.tree_ids() == t_commit.tree_ids() {
            block_on(tx.repo_mut().check_out(name, &created))
                .context("starting a fresh working copy")?;
        } else {
            let remainder = block_on(
                tx.repo_mut()
                    .new_commit(vec![created_id.clone()], full_tree)
                    .write(),
            )
            .context("writing the working-copy remainder")?;
            block_on(tx.repo_mut().edit(name, &remainder))
                .context("pointing the working copy at the remainder")?;
        }
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;
        self.set_target_bookmark(tx.repo_mut(), target, created_id);

        let subject = message.lines().next().unwrap_or("").trim();
        let label = if subject.is_empty() {
            "Commit working copy".to_string()
        } else {
            format!("Commit \"{subject}\"")
        };
        let desc = OpDescriptor::new(label, vec![change_hex]);
        self.finish_mutation(
            tx,
            "commedit: commit working-copy entry",
            desc,
            pre_op,
            old_head,
            heads,
        )
    }

    /// Snapshot the disk and resolve a [`PartialSelection`] into the trees a
    /// partial commit/squash needs: the branch `head` id, HEAD's tree, the leaf
    /// `@`'s full on-disk tree, and `t_commit` — HEAD's tree with the selected
    /// paths/hunks/patches spliced in (the hunks/patches tiers reconstruct text
    /// relative to HEAD). Bails when the tree is clean, HEAD is detached/unborn, or
    /// the selection reproduces HEAD exactly (selects nothing). Shared by
    /// [`Self::commit_working_copy_partial`] and the partial squash
    /// `squash_working_copy_partial_into`.
    pub(crate) fn prepare_partial_commit(
        &mut self,
        sel: &PartialSelection<'_>,
    ) -> Result<(CommitId, MergedTree, MergedTree, MergedTree)> {
        let (head, head_tree, full_tree, store) = self.partial_commit_base()?;

        // Build the selected tree on top of HEAD from the selection.
        let t_commit = self.splice_selection_onto(&head_tree, &full_tree, &store, sel)?;

        // Bail if the selection reproduces HEAD's tree exactly: a listed-but-
        // unmodified path, an empty hunk set, or a patch that changes nothing.
        if t_commit.tree_ids() == head_tree.tree_ids() {
            bail!("the selection commits nothing (it matches the branch head)");
        }
        Ok((head, head_tree, full_tree, t_commit))
    }

    /// Like [`Self::prepare_partial_commit`] but selecting a single file's content
    /// by change-group *range* rather than a [`PartialSelection`]: reconstruct
    /// `path` keeping only the change groups in `first_group..=last_group` of its
    /// `@`-vs-HEAD diff (reverting the rest to HEAD) and splice that onto HEAD.
    /// Backs [`Self::squash_working_copy_hunk_into`]; the range keys on
    /// [`crate::diff::change_groups`], stable across context expansion where a
    /// rendered hunk index is not. Same bail conditions as
    /// [`Self::prepare_partial_commit`], plus an out-of-range group.
    pub(crate) fn prepare_partial_commit_hunk(
        &mut self,
        path: &str,
        first_group: usize,
        last_group: usize,
    ) -> Result<(CommitId, MergedTree, MergedTree, MergedTree)> {
        if first_group > last_group {
            bail!(
                "invalid change-group range: first_group {first_group} > last_group {last_group}"
            );
        }
        let (head, head_tree, full_tree, store) = self.partial_commit_base()?;

        // Reconstruct `path` from its HEAD-side (`old`) and disk-side (`new`) text,
        // keeping only the selected change groups. Bound the range against the group
        // count (stable across expansion) so a stale range fails clearly.
        let (old_f, new_f) = self.partial_file_text(&head_tree, &full_tree, &store, path)?;
        let group_count =
            render_diff(&old_f, &new_f, path, &ContextExpansion::default()).group_count;
        if last_group >= group_count {
            bail!("change-group {last_group} out of range for '{path}' ({group_count} group(s))");
        }
        let kept: BTreeSet<usize> = (first_group..=last_group).collect();
        let selected = select_groups(&old_f, &new_f, &kept);
        let t_commit = crate::tree::splice_files_into_tree(
            head_tree.clone(),
            &store,
            &[(path.to_string(), selected)],
        )?;

        if t_commit.tree_ids() == head_tree.tree_ids() {
            bail!("the selection commits nothing (it matches the branch head)");
        }
        Ok((head, head_tree, full_tree, t_commit))
    }

    /// The snapshot + tree prologue shared by the partial-commit paths: fold the
    /// on-disk changes into the leaf `@`, refuse when the tree is clean / HEAD is
    /// detached or unborn / there is no working copy, and return the branch `head`
    /// id, HEAD's tree, the leaf `@`'s full on-disk tree, and the store. Each caller
    /// splices its own selected `t_commit` onto `head_tree` and rejects an empty one.
    fn partial_commit_base(&mut self) -> Result<(CommitId, MergedTree, MergedTree, Arc<Store>)> {
        // Fold the on-disk changes into the leaf @ first, then refuse if the tree
        // turned out clean (nothing to select).
        self.snapshot_working_copy()?;
        if self.working_copy_info().is_none() {
            bail!("no uncommitted changes to commit");
        }
        let Some(head) = self.head_commit_id() else {
            bail!("the repository has no branch head; cannot commit the working copy");
        };
        let leaf_id = self
            .working_copy_commit_id()
            .context("no working copy to commit")?;
        let store = self.repo.store().clone();
        // The leaf @ holds the full on-disk tree (collapsing any split chain); the
        // remainder will ride on it, and HEAD's tree is the base we splice the
        // selected subset onto.
        let full_tree = store
            .get_commit(&leaf_id)
            .context("loading the working-copy commit")?
            .tree();
        let head_tree = store
            .get_commit(&head)
            .context("loading the branch head")?
            .tree();
        Ok((head, head_tree, full_tree, store))
    }

    /// Splice a [`PartialSelection`] onto `head_tree`, pulling the selected
    /// content from `full_tree` (the leaf `@`'s on-disk tree): whole `paths` are
    /// lifted verbatim; `hunks`/`patches` reconstruct text relative to HEAD (the
    /// same numbering [`render_diff`] produces). Pure tree-building, no snapshot
    /// or emptiness check — shared by [`Self::prepare_partial_commit`] and the
    /// cumulative-tree loop in [`Self::carve_working_copy`].
    fn splice_selection_onto(
        &self,
        head_tree: &MergedTree,
        full_tree: &MergedTree,
        store: &Arc<Store>,
        sel: &PartialSelection<'_>,
    ) -> Result<MergedTree> {
        let mut t = head_tree.clone();
        if !sel.paths.is_empty() {
            t = crate::tree::splice_paths_from_tree(t, full_tree, sel.paths)?;
        }
        // The hunks/patches tiers reconstruct text content relative to HEAD; gather
        // them into one whole-file splice (blobs preserve HEAD's exec bit/copy id).
        let mut text_edits: Vec<(String, String)> = Vec::new();
        for (path, indices) in sel.hunks {
            let (old_f, new_f) = self.partial_file_text(head_tree, full_tree, store, path)?;
            let rendered = render_diff(&old_f, &new_f, path, &ContextExpansion::default());
            let mut kept: BTreeSet<usize> = BTreeSet::new();
            for &i in indices {
                let hunk = rendered.hunks.get(i).ok_or_else(|| {
                    anyhow!(
                        "hunk index {i} is out of range for '{path}' (it has {} hunk(s))",
                        rendered.hunks.len()
                    )
                })?;
                kept.extend(hunk.first_group..=hunk.last_group);
            }
            text_edits.push((path.clone(), select_groups(&old_f, &new_f, &kept)));
        }
        for (path, patch) in sel.patches {
            let (old_f, _new_f) = self.partial_file_text(head_tree, full_tree, store, path)?;
            let content = apply_patch(&old_f, patch)
                .with_context(|| format!("applying the patch for '{path}'"))?;
            text_edits.push((path.clone(), content));
        }
        if !text_edits.is_empty() {
            t = crate::tree::splice_files_into_tree(t, store, &text_edits)?;
        }
        Ok(t)
    }

    /// Read a path's HEAD-side (`old`) and leaf-side (`new`) UTF-8 text for the
    /// hunk/patch tiers, which reconstruct relative to HEAD. An absent side is the
    /// empty string (a file added on disk diffs from ""); a binary side on either
    /// tree is rejected — such files are `paths`-tier only.
    fn partial_file_text(
        &self,
        head_tree: &MergedTree,
        full_tree: &MergedTree,
        store: &Arc<Store>,
        path: &str,
    ) -> Result<(String, String)> {
        let repo_path = RepoPathBuf::from_internal_string(path).context("invalid path")?;
        let read = |tree: &MergedTree| -> Result<(Option<String>, bool)> {
            let value = block_on(tree.path_value(&repo_path)).context("reading path")?;
            let resolved = value.into_resolved().ok().flatten();
            crate::diff::read_text(store, &repo_path, resolved.as_ref())
        };
        let (old_opt, old_bin) = read(head_tree)?;
        let (new_opt, new_bin) = read(full_tree)?;
        if old_bin || new_bin {
            bail!("'{path}' is binary; select it whole via `paths`, not by hunk or patch");
        }
        Ok((old_opt.unwrap_or_default(), new_opt.unwrap_or_default()))
    }

    /// Discard a working-copy entry (identified by its stable change id, or the
    /// leaf `@` when `change_hex` is `None`) — drop its slice of the uncommitted
    /// changes. Like [`Self::split_working_copy`] this is a pure jj-side
    /// reorganization committed directly with no git export (HEAD / refs / index
    /// stay put): the entry is abandoned and any deeper chain entries rebase onto
    /// its parent, so its delta drops out, then the rebased leaf `@` is checked
    /// back out to disk. Abandoning the working-copy commit itself leaves jj's
    /// recreated empty `@` (a clean tree); abandoning an intermediate split-chain
    /// entry removes just that entry's changes and keeps the rest. There is no git
    /// object to graft back, so — unlike a dropped commit — a discarded entry is
    /// gone for good (not kept in the trash).
    pub fn drop_working_copy(&mut self, change_hex: Option<&str>) -> Result<()> {
        self.drop_working_copy_at(WcTarget::Launch, change_hex)
    }

    /// Like [`Self::drop_working_copy`] but on `target`'s worktree `@` — discards a
    /// sibling worktree's uncommitted changes from the unified DAG.
    pub fn drop_working_copy_at(
        &mut self,
        target: WcTarget,
        change_hex: Option<&str>,
    ) -> Result<()> {
        self.require_wc_target(&target, "discard the working copy")?;
        crate::repo::catch_jj("dropping the working copy", || {
            self.drop_working_copy_inner(&target, change_hex)
        })
    }

    fn drop_working_copy_inner(
        &mut self,
        target: &WcTarget,
        change_hex: Option<&str>,
    ) -> Result<()> {
        // Snapshot the disk into the leaf @ first (its commit id churns here),
        // then resolve the target entry's stable change id to its current id.
        self.snapshot_wc(target)?;
        let entry_id = self
            .resolve_wc(target, change_hex)
            .context("no working copy to drop")?;
        let commit = self
            .repo
            .store()
            .get_commit(&entry_id)
            .context("loading the working-copy entry")?;
        let change_hex = commit.change_id().hex();

        let mut tx = self.repo.start_transaction();
        // Abandon the entry; deeper chain entries (and the leaf @) rebase onto its
        // parent, so its delta drops out. When the entry is the working-copy
        // commit itself, jj recreates a fresh empty @ on the parent.
        tx.repo_mut().record_abandoned_commit(&commit);
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;
        self.repo = block_on(tx.commit("commedit: drop working copy"))
            .context("committing the working-copy drop")?;

        // Check the rebased leaf @ back out to disk (the branch tip is unchanged).
        self.materialize_wc(target)?;
        self.record_working_copy_op(target, "Drop uncommitted changes", change_hex);
        Ok(())
    }

    /// Materialize the rebased working-copy commit `@'` to disk after a rewrite,
    /// then reset the git index to the new tip so `git status` shows the
    /// preserved uncommitted changes. The jj-native replacement for the old
    /// `git read-tree` worktree sync (which is kept as a fallback when there is
    /// no working-copy commit, e.g. a detached HEAD). `old_head` is the
    /// pre-rewrite git tip, used only by that fallback.
    pub(crate) fn materialize_after_rewrite(&mut self, old_head: Option<String>) -> Result<()> {
        let root = self.workspace.workspace_root().to_owned();
        match self.working_copy_commit_id() {
            Some(wc_id) => {
                self.materialize_working_copy(&wc_id)?;
                // Resetting the index below would drop any staged content that
                // only lives in the index (not on disk, hence not in @). Pin it
                // to a recoverable backup ref first so it is never lost — a
                // silent safety net documented in the README, not surfaced in
                // the UI.
                let _ = crate::transparency::backup_index_only_content(&root);
                if let Some(new_head) = crate::transparency::head_commit(&root) {
                    crate::transparency::reset_index_to(&root, &new_head)?;
                }
                // Backup refs are transient; keep only the most recent so they
                // don't pile up one per session.
                crate::transparency::prune_backup_refs(&root);
                Ok(())
            }
            None => self.sync_worktree(old_head),
        }
    }

    /// The working-copy commit `@` of this workspace, if one is set.
    pub fn working_copy_commit_id(&self) -> Option<CommitId> {
        let name = self.workspace.workspace_name();
        self.repo.view().get_wc_commit_id(name).cloned()
    }

    /// The working-copy target for `branch` (short-name): the launch worktree when
    /// `branch` is the primary (worktree-bound), else the extra worktree checked
    /// out on it. `None` when the branch has no worktree (a pure ref-move or the
    /// off-worktree primary). The GTK passes the result to the `*_at` working-copy
    /// mutators to act on a sibling worktree's `@`.
    ///
    /// The launch worktree is matched by its branch short name — or the empty
    /// string on a worktree-bound *detached* HEAD, which has no branch name
    /// ([`Self::target_branch_name`] is `None` there but
    /// [`Self::worktree_uncommitted`] keys its `@` under `""`).
    pub fn wc_target_for_branch(&self, branch: &str) -> Option<WcTarget> {
        if branch == self.target_branch_name().unwrap_or("") {
            self.is_worktree_bound().then_some(WcTarget::Launch)
        } else {
            self.find_worktree(branch)
                .map(|_| WcTarget::Worktree(branch.to_string()))
        }
    }

    /// Refuse a working-copy mutation when its target has no worktree: the launch
    /// case reuses [`Self::require_worktree`] (off-worktree has no `@`); a named
    /// worktree must exist in the editable set.
    pub(crate) fn require_wc_target(&self, target: &WcTarget, op: &str) -> Result<()> {
        match target {
            WcTarget::Launch => self.require_worktree(op),
            WcTarget::Worktree(branch) => {
                if self.find_worktree(branch).is_some() {
                    Ok(())
                } else {
                    bail!("no editable worktree checked out on branch {branch:?}")
                }
            }
        }
    }

    /// The `@` commit id of `target`'s worktree, resolving `change_hex` to a
    /// specific entry within that worktree's chain: the launch leaf
    /// ([`Self::resolve_working_copy_change`]) or an extra worktree's chain entry
    /// (searched by stable change id, mirroring the launch resolver). Falls back to
    /// the leaf `@` when `change_hex` is `None` or matches no chain entry.
    pub(crate) fn resolve_wc(
        &self,
        target: &WcTarget,
        change_hex: Option<&str>,
    ) -> Option<CommitId> {
        match target {
            WcTarget::Launch => self.resolve_working_copy_change(change_hex),
            WcTarget::Worktree(branch) => {
                let view = self.find_worktree(branch)?;
                let leaf = self.repo.view().get_wc_commit_id(&view.name).cloned();
                let Some(change_hex) = change_hex else {
                    return leaf;
                };
                let Some(change_id) = jj_lib::backend::ChangeId::try_from_hex(change_hex) else {
                    return leaf;
                };
                for id in self.worktree_chain_ids(view) {
                    if let Ok(commit) = self.repo.store().get_commit(&id) {
                        if commit.change_id() == &change_id {
                            return Some(id);
                        }
                    }
                }
                leaf
            }
        }
    }

    /// `target`'s branch tip — the commit a freshly committed working copy lands
    /// on. The launch tip is git HEAD; an extra worktree's tip is its local
    /// bookmark target (read directly, *not* via the `@`'s parent: a split sibling
    /// chain's leaf parent is an intermediate entry, not the branch tip).
    fn wc_tip(&self, target: &WcTarget) -> Option<CommitId> {
        match target {
            WcTarget::Launch => self.head_commit_id(),
            WcTarget::Worktree(branch) => {
                let name: jj_lib::ref_name::RefNameBuf = branch.as_str().into();
                self.repo
                    .view()
                    .get_local_bookmark(&name)
                    .as_normal()
                    .cloned()
            }
        }
    }

    /// The jj workspace name keying `target`'s `@`, for `check_out`.
    fn wc_workspace_name(&self, target: &WcTarget) -> Option<jj_lib::ref_name::WorkspaceNameBuf> {
        match target {
            WcTarget::Launch => Some(self.workspace.workspace_name().to_owned()),
            WcTarget::Worktree(branch) => self.find_worktree(branch).map(|v| v.name.clone()),
        }
    }

    /// Point `target`'s branch bookmark at `id` — [`Self::set_head_bookmark`] for
    /// the launch, [`Self::set_branch_bookmark`] for an extra worktree's branch.
    fn set_target_bookmark(
        &self,
        mut_repo: &mut jj_lib::repo::MutableRepo,
        target: &WcTarget,
        id: CommitId,
    ) {
        match target {
            WcTarget::Launch => self.set_head_bookmark(mut_repo, id),
            WcTarget::Worktree(branch) => self.set_branch_bookmark(mut_repo, branch, id),
        }
    }

    /// Snapshot `target`'s on-disk tree into its `@` before a mutation. The launch
    /// case reuses [`Self::snapshot_working_copy`] (which also snapshots every
    /// extra worktree); a named target snapshots just that worktree, via the
    /// `mem::take` borrow dance [`Self::snapshot_extra_worktrees`] uses.
    pub(crate) fn snapshot_wc(&mut self, target: &WcTarget) -> Result<()> {
        match target {
            WcTarget::Launch => self.snapshot_working_copy(),
            WcTarget::Worktree(branch) => {
                let mut views = std::mem::take(&mut self.extra_worktrees);
                let found = views
                    .iter_mut()
                    .find(|v| v.branch.strip_prefix("refs/heads/").unwrap_or(&v.branch) == branch);
                let result = match found {
                    Some(v) => self.snapshot_extra_worktree(v),
                    None => Err(anyhow!(
                        "no editable worktree checked out on branch {branch:?}"
                    )),
                };
                self.extra_worktrees = views;
                result
            }
        }
    }

    /// Materialize `target`'s rebased `@` back to its worktree after a `@`-only
    /// rewrite (the branch tip didn't move, so the shared export tail wouldn't).
    /// The launch case reuses [`Self::materialize_after_rewrite`]; a named target
    /// re-checks-out just that worktree.
    pub(crate) fn materialize_wc(&mut self, target: &WcTarget) -> Result<()> {
        match target {
            WcTarget::Launch => self.materialize_after_rewrite(self.head_commit()),
            WcTarget::Worktree(branch) => {
                let mut views = std::mem::take(&mut self.extra_worktrees);
                let found = views
                    .iter_mut()
                    .find(|v| v.branch.strip_prefix("refs/heads/").unwrap_or(&v.branch) == branch);
                let result = match found {
                    Some(v) => self.materialize_extra_worktree(v),
                    None => Ok(()),
                };
                self.extra_worktrees = views;
                result
            }
        }
    }

    /// True when any entry in the **launch** working-copy chain has a conflicted
    /// tree — the launch arm of [`Self::working_copy_has_conflict_at`].
    pub(crate) fn working_copy_has_conflict(&self) -> bool {
        self.working_copy_chain_ids().iter().any(|id| {
            self.repo
                .store()
                .get_commit(id)
                .map(|c| c.has_conflict())
                .unwrap_or(false)
        })
    }

    /// Whether `target`'s working copy is conflicted. Both arms walk the whole `@`
    /// chain — the launch via [`Self::working_copy_has_conflict`], a sibling via
    /// [`Self::worktree_chain_ids`] — so a conflict on any chain entry counts. Gates
    /// recording a working-copy-direct edit as a session op: we record only clean,
    /// materialized states, so the time-travel jumps can always land
    /// [`crate::conflict::SaveOutcome::Clean`]. Keying on the *mutated* worktree's
    /// `@` (not always the launch's) keeps a sibling edit recordable even when the
    /// launch `@` is conflicted, and never records a conflicted sibling `@` as if
    /// it were clean.
    pub(crate) fn working_copy_has_conflict_at(&self, target: &WcTarget) -> bool {
        match target {
            WcTarget::Launch => self.working_copy_has_conflict(),
            WcTarget::Worktree(branch) => self
                .find_worktree(branch)
                .map(|view| {
                    self.worktree_chain_ids(view).iter().any(|id| {
                        self.repo
                            .store()
                            .get_commit(id)
                            .map(|c| c.has_conflict())
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false),
        }
    }

    /// Record a working-copy-direct edit (one that commits straight to jj with no
    /// `finish_mutation`/export) as a session op-log entry the "Edit history"
    /// dropdown can travel back to — unless it left `target`'s working copy
    /// conflicted. `change_hex` is the edited entry's change id, for the dropdown's
    /// hover-highlight.
    pub(crate) fn record_working_copy_op(
        &mut self,
        target: &WcTarget,
        label: &str,
        change_hex: String,
    ) {
        if self.working_copy_has_conflict_at(target) {
            return;
        }
        self.record_op(crate::conflict::OpDescriptor::new(
            label.to_string(),
            vec![change_hex],
        ));
    }

    /// Walk single-parent edges from `leaf` up to (but excluding) the first id in
    /// `boundary`, newest-first — the uncommitted chain between a worktree's `@`
    /// (the leaf, inclusive) and its branch tip (a boundary id, exclusive). When
    /// `@` is not a clean linear descendant of a boundary id (detached HEAD, a
    /// merge in the way, or the tip moved by plain `git`), this falls back to just
    /// the leaf `@`, matching the pre-chain single-`@` behaviour. Empty when
    /// `leaf` is `None` (no working copy). The shared walk behind both the launch
    /// reader ([`Self::working_copy_chain_ids`]) and the per-worktree reader
    /// ([`Self::worktree_chain_ids`]); they differ only in the boundary.
    fn chain_ids_from(&self, leaf: Option<CommitId>, boundary: &[CommitId]) -> Vec<CommitId> {
        let Some(leaf) = leaf else {
            return Vec::new();
        };
        let is_boundary = |id: &CommitId| boundary.contains(id);
        let mut ids = Vec::new();
        let mut id = leaf.clone();
        loop {
            if is_boundary(&id) {
                // Reached the tip: `ids` is the clean uncommitted chain, leaf first.
                return ids;
            }
            ids.push(id.clone());
            let Ok(commit) = self.repo.store().get_commit(&id) else {
                break;
            };
            let parents = commit.parent_ids();
            if parents.len() != 1 {
                break;
            }
            id = parents[0].clone();
        }
        // The walk didn't cleanly reach the tip; treat only the leaf as uncommitted.
        vec![leaf]
    }

    /// The launch worktree's uncommitted-changes chain as commit ids, newest
    /// first: the working-copy commit `@` (leaf) followed by each single-parent
    /// ancestor up to but excluding the current git HEAD. Empty when there is no
    /// working copy (including off-worktree). The boundary is two-valued: git HEAD
    /// when clean, but jj's bookmark target while a conflicted rewrite is pending
    /// (git HEAD lags behind the rewritten tip until the deferred export runs), so
    /// the walk works in both the normal and the resolving state.
    pub(crate) fn working_copy_chain_ids(&self) -> Vec<CommitId> {
        // Off-worktree there is no working copy on the edited branch: `@` is left
        // on jj's root commit, which must never surface as uncommitted changes.
        if !self.is_worktree_bound() {
            return Vec::new();
        }
        let boundary: Vec<CommitId> = [self.head_commit_id(), self.current_head_in_jj()]
            .into_iter()
            .flatten()
            .collect();
        self.chain_ids_from(self.working_copy_commit_id(), &boundary)
    }

    /// An extra worktree's uncommitted-changes chain as commit ids, newest first
    /// (its `@` leaf, then each single-parent ancestor up to but excluding its
    /// branch tip). The per-worktree analogue of [`Self::working_copy_chain_ids`]:
    /// a sibling `@` sits directly on its branch's local bookmark, so its boundary
    /// is that single tip (no pending-rewrite ambiguity to straddle). A single
    /// entry in the common case; [`Self::split_working_copy_edits_at`] peels it
    /// into more. Empty when the worktree has no `@`.
    pub(crate) fn worktree_chain_ids(&self, view: &crate::repo::WorktreeView) -> Vec<CommitId> {
        let leaf = self.repo.view().get_wc_commit_id(&view.name).cloned();
        let short = view
            .branch
            .strip_prefix("refs/heads/")
            .unwrap_or(&view.branch);
        let bookmark: jj_lib::ref_name::RefNameBuf = short.into();
        let boundary: Vec<CommitId> = self
            .repo
            .view()
            .get_local_bookmark(&bookmark)
            .as_normal()
            .cloned()
            .into_iter()
            .collect();
        self.chain_ids_from(leaf, &boundary)
    }

    /// Build a [`WorkingCopyEntry`] for one working-copy commit `@`: its
    /// git-tracked changed files, labelled "Uncommitted changes". `None` when the
    /// `@` changes nothing (an empty leaf — e.g. the fresh `@` jj recreates after
    /// the whole pile is folded into a commit — is never shown). Shared by the
    /// launch chain ([`Self::working_copy_chain`]) and the per-worktree reader
    /// ([`Self::worktree_uncommitted`]).
    fn wc_entry_for(&self, id: &CommitId) -> Option<WorkingCopyEntry> {
        let commit = self.repo.store().get_commit(id).ok()?;
        let file_names: Vec<String> = crate::diff::commit_changes(&self.repo, id)
            .map(|c| c.into_iter().map(|f| f.path).collect())
            .unwrap_or_default();
        if file_names.is_empty() {
            return None;
        }
        let mut info = crate::history::CommitInfo::from_commit(&commit);
        info.subject = "Uncommitted changes".to_string();
        Some(WorkingCopyEntry {
            info,
            changed_files: file_names.len(),
            file_names,
            has_conflict: commit.has_conflict(),
        })
    }

    /// The uncommitted-changes entries to show as read-only rows above the
    /// history, newest first (the leaf `@` first). One per commit in the
    /// working-copy chain that actually changes files — an empty leaf (e.g. the
    /// fresh `@` jj recreates after the whole pile is folded into a commit) is
    /// skipped, so an empty list means a clean tree. Kept out of
    /// [`crate::history::history`] so the reorder/drop/squash index arithmetic is
    /// unaffected.
    pub fn working_copy_chain(&self) -> Vec<WorkingCopyEntry> {
        self.working_copy_chain_ids()
            .iter()
            .filter_map(|id| self.wc_entry_for(id))
            .collect()
    }

    /// Uncommitted changes per editable worktree, for the unified multi-branch
    /// DAG. Each tuple is `(branch short-name, entries)` newest-first: the launch
    /// worktree (when its branch is checked out here) contributes its full `@`
    /// chain under the primary branch's name; every *extra* editable worktree (see
    /// [`crate::repo::WorktreeView`]) contributes its own dirty `@` chain (one
    /// entry unless [`Self::split_working_copy_edits_at`] peeled it) under its own
    /// branch's name. A clean worktree, a branch with no worktree (a pure
    /// ref-move), and the off-worktree primary all contribute nothing — an empty
    /// `entries` list is never emitted. The launch `@` chain is read exactly as
    /// [`Self::working_copy_chain`]; the extra worktrees' chains via
    /// [`Self::worktree_chain_ids`], mirroring [`Self::snapshot_extra_worktree`].
    pub fn worktree_uncommitted(&self) -> Vec<(String, Vec<WorkingCopyEntry>)> {
        let mut out = Vec::new();
        if self.is_worktree_bound() {
            let chain = self.working_copy_chain();
            if !chain.is_empty() {
                out.push((self.target_branch_name().unwrap_or("").to_string(), chain));
            }
        }
        for view in &self.extra_worktrees {
            let entries: Vec<WorkingCopyEntry> = self
                .worktree_chain_ids(view)
                .iter()
                .filter_map(|id| self.wc_entry_for(id))
                .collect();
            if !entries.is_empty() {
                let short = view
                    .branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&view.branch);
                out.push((short.to_string(), entries));
            }
        }
        out
    }

    /// Every editable worktree's working-copy commit ids: the launch `@` chain
    /// (newest first) followed by each extra worktree's `@` chain. The id-level,
    /// flattened analogue of [`Self::worktree_uncommitted`], used by conflict
    /// resolution ([`Repo::resolve_change_on_chain`]) to locate a conflicted `@` on
    /// *any* worktree — the same working-copy sources [`Repo::collect_conflicts`]
    /// scans. A singleton set with no extra worktrees yields just the launch chain.
    pub(crate) fn all_worktree_chain_ids(&self) -> Vec<CommitId> {
        let mut ids = self.working_copy_chain_ids();
        for view in &self.extra_worktrees {
            ids.extend(self.worktree_chain_ids(view));
        }
        ids
    }

    /// Every editable worktree's working-copy *entries* that change files — the
    /// launch `@` chain plus each extra worktree's dirty `@` chain, in the order
    /// [`Self::all_worktree_chain_ids`] lists them. The flat, all-worktree analogue
    /// of [`Self::working_copy_chain`], used by the GTK conflict view to render a
    /// conflicted *sibling* `@` as an inline resolvable row (a clean/empty `@` is
    /// skipped, like [`Self::working_copy_chain`]).
    pub fn worktree_chain_entries(&self) -> Vec<WorkingCopyEntry> {
        self.all_worktree_chain_ids()
            .iter()
            .filter_map(|id| self.wc_entry_for(id))
            .collect()
    }

    /// Resolve a working-copy entry's stable change id to its *current* commit id
    /// within the chain. Commit ids churn (the leaf's on every snapshot), so the
    /// UI hands edits/splits/squashes a change id and we resolve it here, after
    /// snapshotting. Falls back to the leaf `@` when `change_hex` is `None` or
    /// doesn't match a chain entry.
    pub(crate) fn resolve_working_copy_change(&self, change_hex: Option<&str>) -> Option<CommitId> {
        let leaf = self.working_copy_commit_id();
        let Some(change_hex) = change_hex else {
            return leaf;
        };
        let Some(change_id) = jj_lib::backend::ChangeId::try_from_hex(change_hex) else {
            return leaf;
        };
        for id in self.working_copy_chain_ids() {
            if let Ok(commit) = self.repo.store().get_commit(&id) {
                if commit.change_id() == &change_id {
                    return Some(id);
                }
            }
        }
        leaf
    }

    /// A summary of the uncommitted changes to show as a read-only top row in the
    /// history, or `None` when the working tree is clean. Kept out of
    /// [`crate::history::history`] so the reorder/drop/squash index arithmetic is
    /// unaffected.
    pub fn working_copy_info(&self) -> Option<WorkingCopyInfo> {
        if !self.is_worktree_bound() {
            return None;
        }
        let commit_id = self.working_copy_commit_id()?;
        let commit = self.repo.store().get_commit(&commit_id).ok()?;
        let changed_files = crate::diff::commit_changes(&self.repo, &commit_id)
            .map(|c| c.len())
            .unwrap_or(0);
        if changed_files == 0 {
            return None;
        }
        Some(WorkingCopyInfo {
            commit_id,
            changed_files,
            has_conflict: commit.has_conflict(),
        })
    }

    /// The working-copy commit `@` as a history row, labelled "Uncommitted
    /// changes". The UI prepends this to the conflict chain so a conflicted `@`
    /// is selectable and resolvable like any other commit.
    pub fn working_copy_commit_info(&self) -> Option<crate::history::CommitInfo> {
        if !self.is_worktree_bound() {
            return None;
        }
        let id = self.working_copy_commit_id()?;
        let commit = self.repo.store().get_commit(&id).ok()?;
        let mut info = crate::history::CommitInfo::from_commit(&commit);
        info.subject = "Uncommitted changes".to_string();
        Some(info)
    }

    /// Whether the `@` of the workspace named `name` sits on a clean linear chain
    /// rooted at `tip`: walk single-parent edges up from that `@` and return `true`
    /// iff we reach `tip`. This keeps a split chain (`tip → @' → @`) intact — `@`'s
    /// parent need not be `tip` directly, only an ancestor reached through our own
    /// uncommitted commits. Returns `false` (→ re-attach) on a merge in the way,
    /// the root, or when `tip` isn't an ancestor (e.g. plain `git` moved the tip).
    /// Shared by the launch wrapper [`Self::working_copy_on_head`] and the
    /// per-worktree re-anchor [`Self::reanchor_extra_worktree`].
    fn wc_on_tip(&self, name: &jj_lib::ref_name::WorkspaceName, tip: &CommitId) -> Result<bool> {
        let Some(mut id) = self.repo.view().get_wc_commit_id(name).cloned() else {
            return Ok(false);
        };
        loop {
            if &id == tip {
                return Ok(true);
            }
            let commit = self
                .repo
                .store()
                .get_commit(&id)
                .context("loading a working-copy chain commit")?;
            let parents = commit.parent_ids();
            if parents.len() != 1 {
                return Ok(false);
            }
            id = parents[0].clone();
        }
    }

    /// The launch wrapper over [`Self::wc_on_tip`]: whether the launch `@` sits on a
    /// clean linear chain rooted at `head`.
    fn working_copy_on_head(&self, head: &CommitId) -> Result<bool> {
        self.wc_on_tip(self.workspace.workspace_name(), head)
    }

    /// Re-parent `@` onto the current git HEAD when it isn't already there (e.g.
    /// right after open, or after the user committed with plain `git`), so a
    /// subsequent snapshot records only the uncommitted delta. A fresh `@` is an
    /// empty commit on top of HEAD. A split chain that still descends from HEAD is
    /// left intact (see [`Self::working_copy_on_head`]). No-op on a detached HEAD.
    fn ensure_working_copy_on_head(&mut self) -> Result<()> {
        let Some(head) = self.head_commit_id() else {
            return Ok(());
        };
        if !self.working_copy_on_head(&head)? {
            self.reattach_working_copy_to_head(&head)?;
        }
        Ok(())
    }

    /// Collapse a persisted working-copy *chain* back to a single `@` on HEAD.
    /// The split structure (`HEAD → @' → @`) lives only in jj's operation log; git
    /// sees one unstaged pile (the leaf materialized to disk). A fresh session
    /// reconciles to git's view: if `@` isn't a single commit directly on HEAD,
    /// re-attach it there — abandoning the intermediate entries — so the following
    /// snapshot records the uncommitted changes as one entry. Called once at
    /// [`Repo::open`]; a no-op when `@` is already a lone commit on HEAD (or on a
    /// detached HEAD). Run *before* the open-time snapshot.
    pub(crate) fn collapse_working_copy_chain(&mut self) -> Result<()> {
        let Some(head) = self.head_commit_id() else {
            return Ok(());
        };
        let single_on_head = match self.working_copy_commit_id() {
            Some(wc_id) => {
                let wc = self
                    .repo
                    .store()
                    .get_commit(&wc_id)
                    .context("loading the working-copy commit")?;
                wc.parent_ids() == std::slice::from_ref(&head)
            }
            None => true, // no @ yet — nothing to collapse
        };
        if !single_on_head {
            self.reattach_working_copy_to_head(&head)?;
        }
        Ok(())
    }

    /// Replace `@` with a fresh empty commit directly on `head`, abandoning the
    /// previous working-copy commit and any split chain above HEAD. The on-disk
    /// content is untouched (this only moves jj's view pointer); the caller's
    /// snapshot then re-records the disk into the new `@`.
    fn reattach_working_copy_to_head(&mut self, head: &CommitId) -> Result<()> {
        let name = self.workspace.workspace_name().to_owned();
        let head_commit = self
            .repo
            .store()
            .get_commit(head)
            .context("loading the head commit")?;
        let mut tx = self.repo.start_transaction();
        block_on(tx.repo_mut().check_out(name, &head_commit))
            .context("attaching the working copy to head")?;
        // check_out abandons the previous @ (and any chain above HEAD); rebase
        // before commit.
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing after attach")?;
        self.repo = block_on(tx.commit("commedit: attach working copy to head"))
            .context("committing working-copy attach")?;
        Ok(())
    }

    /// The base gitignore for snapshotting the launch worktree: its
    /// `.git/info/exclude` (in-tree `.gitignore`s are chained automatically as jj
    /// descends).
    fn base_ignores(&self) -> Result<Arc<GitIgnoreFile>> {
        self.base_ignores_at(self.workspace.workspace_root())
    }

    /// [`Self::base_ignores`] for an arbitrary worktree `root`. A *linked* worktree's
    /// `.git` is a file, not a directory, so its `info/exclude` is resolved via git
    /// (`rev-parse --git-path`) rather than the literal `.git/info/exclude` path;
    /// the launch worktree resolves to the same place.
    fn base_ignores_at(&self, root: &std::path::Path) -> Result<Arc<GitIgnoreFile>> {
        let exclude = crate::transparency::git_path(root, "info/exclude")
            .unwrap_or_else(|| root.join(".git").join("info").join("exclude"));
        GitIgnoreFile::empty()
            .chain_with_file(RepoPath::root(), exclude)
            .context("reading info/exclude")
    }
}

/// The owned three tiers of a [`PartialSelection`], ready to borrow into one.
type OwnedSelection = (
    Vec<String>,
    Vec<(String, Vec<usize>)>,
    Vec<(String, String)>,
);

/// The cumulative selection of the first `n` carve entries, merged into one
/// [`PartialSelection`]'s owned tiers. Paths and patches are unique across
/// entries (enforced by [`validate_carve`]) so they concatenate; a path split
/// across entries' `hunks` tiers has its indices merged. Because every entry
/// addresses the same `@`-vs-HEAD diff, the merged hunk indices stay valid.
fn cumulative_selection(entries: &[CarveEntry<'_>], n: usize) -> OwnedSelection {
    let mut paths: Vec<String> = Vec::new();
    let mut hunks: Vec<(String, Vec<usize>)> = Vec::new();
    let mut patches: Vec<(String, String)> = Vec::new();
    for entry in &entries[..n] {
        let sel = &entry.selection;
        paths.extend(sel.paths.iter().cloned());
        patches.extend(sel.patches.iter().cloned());
        for (path, indices) in sel.hunks {
            match hunks.iter_mut().find(|(p, _)| p == path) {
                Some((_, acc)) => acc.extend(indices.iter().copied()),
                None => hunks.push((path.clone(), indices.clone())),
            }
        }
    }
    (paths, hunks, patches)
}

/// Validate a carve's entries before any work: at least one entry, each entry
/// selects something, and per path the selections don't overlap — a whole-file
/// (`paths`) or `patches` selection of a path is unique across the carve, while
/// the `hunks` tier may split a path across entries only with disjoint indices.
fn validate_carve(entries: &[CarveEntry<'_>]) -> Result<()> {
    if entries.is_empty() {
        bail!("carve needs at least one commit to create");
    }
    // A path selected whole or by patch is exclusive; hunk indices accumulate.
    let mut exclusive: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut hunk_indices: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();
    for (i, entry) in entries.iter().enumerate() {
        let sel = &entry.selection;
        if sel.paths.is_empty() && sel.hunks.is_empty() && sel.patches.is_empty() {
            bail!(
                "carve commit {} selects no changes; every entry must commit something",
                i + 1
            );
        }
        for path in sel.paths.iter().chain(sel.patches.iter().map(|(p, _)| p)) {
            if exclusive.contains(path) || hunk_indices.contains_key(path) {
                bail!(
                    "path '{path}' is selected by more than one carve commit; a whole-file \
                     or patch selection of a path must be unique"
                );
            }
            exclusive.insert(path.clone());
        }
        for (path, indices) in sel.hunks {
            if indices.is_empty() {
                bail!(
                    "carve commit {} lists '{path}' in hunks but selects no indices",
                    i + 1
                );
            }
            if exclusive.contains(path) {
                bail!("path '{path}' is selected both whole (or by patch) and by hunk; pick one");
            }
            let acc = hunk_indices.entry(path.clone()).or_default();
            for &idx in indices {
                if acc.contains(&idx) {
                    bail!("hunk {idx} of '{path}' is selected by more than one carve commit");
                }
                acc.push(idx);
            }
        }
    }
    Ok(())
}
