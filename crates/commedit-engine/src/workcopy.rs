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
//! jj's own metadata nor ignored files leak into `@`.

use std::sync::Arc;

use anyhow::{Context, Result};
use jj_lib::backend::{CommitId, TreeValue};
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::merge::{Merge, MergedTreeValue};
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::working_copy::{CheckoutStats, SnapshotOptions};

use crate::repo::Repo;

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

/// How the user's uncommitted changes fared across the most recent rewrite, for
/// the UI to surface once. An overlap between a local edit and the rewrite is
/// handled by the deferred conflict-resolution flow (the working copy `@` shows
/// up as a conflicted commit), so the only thing left to report here is staged
/// content that only lived in the git index and was pinned to a backup ref.
#[derive(Debug, Clone)]
pub struct WorkingCopyAdvisory {
    /// A `refs/commedit/backup/index-*` ref pinning staged content that only
    /// lived in the index (see [`crate::transparency::backup_index_only_content`]).
    pub index_backup_ref: String,
}

impl Repo {
    /// Snapshot the on-disk working directory into the working-copy commit `@`,
    /// so uncommitted changes (tracked edits **and** untracked, non-ignored
    /// files) become a real commit on top of the checked-out tip. A no-op on a
    /// detached HEAD or when nothing changed since the last snapshot.
    pub fn snapshot_working_copy(&mut self) -> Result<()> {
        // `@` must sit directly on the current tip, or its diff would be the
        // whole history rather than the uncommitted delta.
        self.ensure_working_copy_on_head()?;
        let name = self.workspace.workspace_name().to_owned();

        let base_ignores = self.base_ignores()?;
        let everything = EverythingMatcher;
        let nothing = NothingMatcher;
        let options = SnapshotOptions {
            base_ignores,
            progress: None,
            // Start tracking any new file that isn't ignored...
            start_tracking_matcher: &everything,
            // ...but never force-track ignored or oversized files.
            force_tracking_matcher: &nothing,
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

    /// Edit a file of the working copy through the diff pane: splice `new_content`
    /// into `@`'s tree and write the result to disk — like editing any commit,
    /// but the branch tip doesn't move (no history export). Snapshots the disk
    /// first so a concurrent external edit to another file isn't clobbered.
    pub fn edit_working_copy_file(&mut self, path: &str, new_content: &str) -> Result<()> {
        crate::repo::catch_jj("editing the working copy", || {
            self.edit_working_copy_file_inner(path, new_content)
        })
    }

    fn edit_working_copy_file_inner(&mut self, path: &str, new_content: &str) -> Result<()> {
        self.snapshot_working_copy()?;
        let wc_id = self
            .working_copy_commit_id()
            .context("no working copy to edit")?;
        let commit = self
            .repo
            .store()
            .get_commit(&wc_id)
            .context("loading the working-copy commit")?;
        let repo_path = RepoPathBuf::from_internal_string(path).context("invalid path")?;
        let base_tree = commit.tree();
        let (executable, copy_id) = crate::tree::existing_file_meta(&base_tree, &repo_path);

        let store = self.repo.store().clone();
        let mut reader: &[u8] = new_content.as_bytes();
        let file_id = block_on(store.write_file(&repo_path, &mut reader))
            .context("writing file blob")?;
        let value: MergedTreeValue = Merge::normal(TreeValue::File {
            id: file_id,
            executable,
            copy_id,
        });
        let mut builder = MergedTreeBuilder::new(base_tree);
        builder.set_or_remove(repo_path, value);
        let new_tree = block_on(builder.write_tree()).context("writing tree")?;

        let mut tx = self.repo.start_transaction();
        block_on(tx.repo_mut().rewrite_commit(&commit).set_tree(new_tree).write())
            .context("rewriting the working-copy commit")?;
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing after edit")?;
        self.repo = block_on(tx.commit("commedit: edit working copy"))
            .context("committing the working-copy edit")?;

        // Write the edited @ to disk (the branch tip is unchanged).
        self.materialize_after_rewrite(self.head_commit())
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
                // to a recoverable backup ref first so it is never lost.
                let index_backup = crate::transparency::backup_index_only_content(&root);
                if let Some(backup) = &index_backup {
                    eprintln!(
                        "commedit: staged changes not present on disk were preserved at {backup}; \
                         recover with `git read-tree {backup}` or `git checkout {backup} -- .`"
                    );
                }
                if let Some(new_head) = crate::transparency::head_commit(&root) {
                    crate::transparency::reset_index_to(&root, &new_head)?;
                }
                // A conflicted @ never reaches here — it defers via the conflict
                // flow — so the only advisory left is the index backup, if any.
                self.wc_advisory = index_backup.map(|r| WorkingCopyAdvisory { index_backup_ref: r });
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

    /// A summary of the uncommitted changes to show as a read-only top row in the
    /// history, or `None` when the working tree is clean. Kept out of
    /// [`crate::history::history`] so the reorder/drop/squash index arithmetic is
    /// unaffected.
    pub fn working_copy_info(&self) -> Option<WorkingCopyInfo> {
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

    /// Take (and clear) the advisory describing how uncommitted changes fared in
    /// the last rewrite, for the UI to surface once.
    pub fn take_working_copy_advisory(&mut self) -> Option<WorkingCopyAdvisory> {
        self.wc_advisory.take()
    }

    /// The working-copy commit `@` as a history row, labelled "Uncommitted
    /// changes". The UI prepends this to the conflict chain so a conflicted `@`
    /// is selectable and resolvable like any other commit.
    pub fn working_copy_commit_info(&self) -> Option<crate::history::CommitInfo> {
        let id = self.working_copy_commit_id()?;
        let commit = self.repo.store().get_commit(&id).ok()?;
        let mut info = crate::history::CommitInfo::from_commit(&commit);
        info.subject = "Uncommitted changes".to_string();
        Some(info)
    }

    /// Re-parent `@` onto the current git HEAD when it isn't already there (e.g.
    /// right after open, or after the user committed with plain `git`), so a
    /// subsequent snapshot records only the uncommitted delta. A fresh `@` is an
    /// empty commit on top of HEAD. No-op on a detached HEAD.
    fn ensure_working_copy_on_head(&mut self) -> Result<()> {
        let name = self.workspace.workspace_name().to_owned();
        let Some(head) = self.head_commit_id() else {
            return Ok(());
        };
        let on_head = match self.repo.view().get_wc_commit_id(&name) {
            Some(wc_id) => {
                let wc = self
                    .repo
                    .store()
                    .get_commit(wc_id)
                    .context("loading the working-copy commit")?;
                wc.parent_ids() == std::slice::from_ref(&head)
            }
            None => false,
        };
        if !on_head {
            let head_commit = self
                .repo
                .store()
                .get_commit(&head)
                .context("loading the head commit")?;
            let mut tx = self.repo.start_transaction();
            block_on(tx.repo_mut().check_out(name, &head_commit))
                .context("attaching the working copy to head")?;
            // check_out abandons the previous (empty) @; rebase before commit.
            block_on(tx.repo_mut().rebase_descendants()).context("rebasing after attach")?;
            self.repo = block_on(tx.commit("commedit: attach working copy to head"))
                .context("committing working-copy attach")?;
        }
        Ok(())
    }

    /// The base gitignore for snapshotting: the repo-local `.git/info/exclude`
    /// (in-tree `.gitignore`s are chained automatically as jj descends).
    fn base_ignores(&self) -> Result<Arc<GitIgnoreFile>> {
        let exclude = self
            .workspace
            .workspace_root()
            .join(".git")
            .join("info")
            .join("exclude");
        GitIgnoreFile::empty()
            .chain_with_file(RepoPath::root(), exclude)
            .context("reading .git/info/exclude")
    }
}
