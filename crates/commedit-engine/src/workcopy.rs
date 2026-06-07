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
use jj_lib::backend::CommitId;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPath;
use jj_lib::working_copy::{CheckoutStats, SnapshotOptions};

use crate::repo::Repo;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    pollster::block_on(f)
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

    /// The working-copy commit `@` of this workspace, if one is set.
    pub fn working_copy_commit_id(&self) -> Option<CommitId> {
        let name = self.workspace.workspace_name();
        self.repo.view().get_wc_commit_id(name).cloned()
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
