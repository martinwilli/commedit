//! Rewrite a historical commit and cascade-rebase its descendants — the
//! "implicit amend + auto-rebase" core. Message editing is implemented here;
//! tree/hunk editing reuses the same transaction shape.

use anyhow::{Context, Result};
use jj_lib::backend::CommitId;
use jj_lib::repo::Repo as _;

use crate::repo::Repo;
use crate::transparency;

impl Repo {
    /// Replace the description of `target` with `message`, rebase all
    /// descendants onto the rewritten commit, and export the result to git in a
    /// single transaction.
    pub fn rewrite_message(&mut self, target: &CommitId, message: &str) -> Result<()> {
        let old_head = self.head_commit();
        let commit = self
            .repo
            .store()
            .get_commit(target)
            .context("loading target commit")?;

        let mut tx = self.repo.start_transaction();
        pollster::block_on(
            tx.repo_mut()
                .rewrite_commit(&commit)
                .set_description(message)
                .write(),
        )
        .context("writing rewritten commit")?;
        pollster::block_on(tx.repo_mut().rebase_descendants())
            .context("rebasing descendants")?;
        transparency::export_to_git(tx.repo_mut())?;

        self.repo = pollster::block_on(tx.commit("commedit: edit commit message"))
            .context("committing rewrite")?;
        self.reattach_head()?;
        self.sync_worktree(old_head)?;
        Ok(())
    }
}
