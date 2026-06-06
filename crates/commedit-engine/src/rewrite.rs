//! Rewrite a historical commit and cascade-rebase its descendants — the
//! "implicit amend + auto-rebase" core. Message editing is implemented here;
//! tree/hunk editing reuses the same transaction shape.

use anyhow::{bail, Context, Result};
use jj_lib::backend::{CommitId, Signature};
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::{
    move_commits, MoveCommitsLocation, MoveCommitsTarget, RebaseOptions, RebasedCommit,
};

use crate::history::{plan_reorder, parse_timestamp, CommitInfo, ReorderMove};
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
        self.sync_worktree(old_head.clone())?;
        if let Some(old) = old_head {
            self.prune_orphaned_keep_refs(&old);
        }
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
        self.sync_worktree(old_head.clone())?;
        if let Some(old) = old_head {
            self.prune_orphaned_keep_refs(&old);
        }
        Ok(())
    }

    /// Plan a drag-to-reorder of the commit at display index `from` to the
    /// insertion gap `to`, against the current branch's linear chain. Returns
    /// `None` for an out-of-range/no-op drop, an off-branch row, or when HEAD is
    /// unknown. See [`crate::history::plan_reorder`].
    pub fn plan_reorder(
        &self,
        commits: &[CommitInfo],
        from: usize,
        to: usize,
    ) -> Option<ReorderMove> {
        let head = self.head_commit_id()?;
        plan_reorder(commits, &head, from, to)
    }

    /// Move `target` to a new slot in the linear history: rebased onto
    /// `new_parent_ids`, with `new_child_ids` rebased on top of it, cascading to
    /// all descendants. `new_tip` is the (pre-move) id of the commit that should
    /// end up as the branch head once the dust settles. Exported to git in one
    /// transaction.
    ///
    /// Reordering is a true rebase — each moved commit's diff is re-applied onto
    /// its new parent — so commits that don't commute may rebase with conflicts.
    pub fn reorder_commit(
        &mut self,
        target: &CommitId,
        new_parent_ids: Vec<CommitId>,
        new_child_ids: Vec<CommitId>,
        new_tip: &CommitId,
    ) -> Result<()> {
        let old_head = self.head_commit();
        let loc = MoveCommitsLocation {
            new_parent_ids,
            new_child_ids,
            target: MoveCommitsTarget::Commits(vec![target.clone()]),
        };

        let mut tx = self.repo.start_transaction();
        let stats = pollster::block_on(move_commits(
            tx.repo_mut(),
            &loc,
            &RebaseOptions::default(),
        ))
        .context("reordering commit")?;
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;

        // Point the branch at the new head. A reorder need not rewrite the old
        // head commit, so jj's automatic bookmark moves can leave the branch
        // behind; set it explicitly. The head keeps its change id, but its commit
        // id changes if it was rebased onto a new parent.
        let new_tip_id = match stats.rebased_commits.get(new_tip) {
            Some(RebasedCommit::Rewritten(commit)) => commit.id().clone(),
            Some(RebasedCommit::Abandoned { .. }) => bail!("the new head commit became empty"),
            None => new_tip.clone(),
        };
        self.set_head_bookmark(tx.repo_mut(), new_tip_id);

        transparency::export_to_git(tx.repo_mut())?;
        self.repo = pollster::block_on(tx.commit("commedit: reorder commit"))
            .context("committing reorder")?;
        self.reattach_head()?;
        self.sync_worktree(old_head.clone())?;
        if let Some(old) = old_head {
            self.prune_orphaned_keep_refs(&old);
        }
        Ok(())
    }
}
