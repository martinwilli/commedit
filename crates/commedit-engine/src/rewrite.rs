//! Rewrite a historical commit and cascade-rebase its descendants — the
//! "implicit amend + auto-rebase" core. Message editing is implemented here;
//! tree/hunk editing reuses the same transaction shape.

use anyhow::{Context, Result};
use jj_lib::backend::{CommitId, Signature};
use jj_lib::repo::Repo as _;

use crate::history::parse_timestamp;
use crate::repo::Repo;
use crate::transparency;

/// The author and committer identity of a commit, as edited in the UI. Names and
/// emails are free text; the timestamps are parsed by [`parse_timestamp`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub author_name: String,
    pub author_email: String,
    pub author_time: String,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_time: String,
}

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

    /// Replace the author and committer identity (name, email, timestamp) of
    /// `target`, rebase descendants, and export to git in one transaction.
    ///
    /// Both signatures are set explicitly so this also overrides jj's habit of
    /// stamping the committer to "now" on a rewrite; run it last in a save so the
    /// edited values win over the side effects of message/content edits.
    pub fn rewrite_identity(&mut self, target: &CommitId, id: &Identity) -> Result<()> {
        let old_head = self.head_commit();
        let commit = self
            .repo
            .store()
            .get_commit(target)
            .context("loading target commit")?;

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

        let mut tx = self.repo.start_transaction();
        pollster::block_on(
            tx.repo_mut()
                .rewrite_commit(&commit)
                .set_author(author)
                .set_committer(committer)
                .write(),
        )
        .context("writing rewritten commit")?;
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;
        transparency::export_to_git(tx.repo_mut())?;

        self.repo = pollster::block_on(tx.commit("commedit: edit commit identity"))
            .context("committing rewrite")?;
        self.reattach_head()?;
        self.sync_worktree(old_head)?;
        Ok(())
    }
}
