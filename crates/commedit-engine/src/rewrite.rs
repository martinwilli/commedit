//! Rewrite a historical commit and cascade-rebase its descendants — the
//! "implicit amend + auto-rebase" core. Message editing is implemented here;
//! tree/hunk editing reuses the same transaction shape.

use anyhow::{bail, Context, Result};
use jj_lib::backend::{CommitId, Signature};
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::{
    move_commits, MoveCommitsLocation, MoveCommitsTarget, RebaseOptions, RebasedCommit,
};

use crate::conflict::{OpDescriptor, SaveOutcome, SpuriousResolve};
use crate::graph::GraphLayout;
use crate::history::{
    plan_drop, plan_reorder_candidates, plan_restore_candidates, parse_timestamp, CommitInfo,
    ReorderCandidate,
};
use crate::repo::Repo;

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
    pub fn rewrite_message(&mut self, target: &CommitId, message: &str) -> Result<SaveOutcome> {
        crate::repo::catch_jj("editing the message", || self.rewrite_message_inner(target, message))
    }

    fn rewrite_message_inner(&mut self, target: &CommitId, message: &str) -> Result<SaveOutcome> {
        // Capture the on-disk working copy into @ so it rebases with the rewrite.
        self.snapshot_working_copy()?;
        let pre_op = self.repo.operation().clone();
        let old_head = self.head_commit();
        let heads = self.snapshot_heads();
        let commit = self
            .repo
            .store()
            .get_commit(target)
            .context("loading target commit")?;
        let desc = self.op_desc_for("Edit message of", target);

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

        self.finish_mutation(
            tx,
            "commedit: edit commit message",
            desc,
            pre_op,
            old_head,
            heads,
        )
    }

    /// Replace the author and committer identity (name, email, timestamp) of
    /// `target`, rebase descendants, and export to git in one transaction.
    ///
    /// Both signatures are set explicitly so this also overrides jj's habit of
    /// stamping the committer to "now" on a rewrite; run it last in a save so the
    /// edited values win over the side effects of message/content edits.
    pub fn rewrite_identity(&mut self, target: &CommitId, id: &Identity) -> Result<SaveOutcome> {
        crate::repo::catch_jj("editing the identity", || self.rewrite_identity_inner(target, id))
    }

    fn rewrite_identity_inner(&mut self, target: &CommitId, id: &Identity) -> Result<SaveOutcome> {
        // Capture the on-disk working copy into @ so it rebases with the rewrite.
        self.snapshot_working_copy()?;
        let pre_op = self.repo.operation().clone();
        let old_head = self.head_commit();
        let heads = self.snapshot_heads();
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
        let desc = self.op_desc_for("Edit identity of", target);

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

        self.finish_mutation(
            tx,
            "commedit: edit commit identity",
            desc,
            pre_op,
            old_head,
            heads,
        )
    }

    /// All destination lines for dragging the commit at display index `from` to
    /// the insertion gap `to` — one candidate per ancestry line crossing the
    /// gap. Empty for an out-of-range/no-op drop, a merge, an off-branch row, or
    /// when HEAD is unknown. See [`crate::history::plan_reorder_candidates`].
    pub fn plan_reorder_candidates(
        &self,
        commits: &[CommitInfo],
        layout: &GraphLayout,
        from: usize,
        to: usize,
    ) -> Vec<ReorderCandidate> {
        let Some(head) = self.head_commit_id() else {
            return Vec::new();
        };
        plan_reorder_candidates(commits, &head, layout, &self.root_commit_id(), from, to)
    }

    /// All destination lines for grafting the trashed commit `restored` back
    /// into the history at insertion gap `to`. Empty for an out-of-range drop or
    /// when HEAD is unknown. See [`crate::history::plan_restore_candidates`].
    pub fn plan_restore_candidates(
        &self,
        commits: &[CommitInfo],
        layout: &GraphLayout,
        restored: &CommitInfo,
        to: usize,
    ) -> Vec<ReorderCandidate> {
        let Some(head) = self.head_commit_id() else {
            return Vec::new();
        };
        plan_restore_candidates(commits, &head, layout, &self.root_commit_id(), restored, to)
    }

    /// The id of the commit at display `index` if it can be dropped to the trash,
    /// or `None` (a merge, an off-branch row, or the branch's only commit). See
    /// [`crate::history::plan_drop`].
    pub fn plan_drop(&self, commits: &[CommitInfo], index: usize) -> Option<CommitId> {
        let head = self.head_commit_id()?;
        plan_drop(commits, &head, index)
    }

    /// Move `target` to a new slot in the history graph: rebased onto
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
    ) -> Result<SaveOutcome> {
        let desc = self.op_desc_for("Reorder", target);
        self.splice_commit(
            target,
            new_parent_ids,
            new_child_ids,
            new_tip,
            "commedit: reorder commit",
            SpuriousResolve::CleanTip,
            desc,
        )
    }

    /// Graft a previously-dropped commit back into the linear history. Mechanically
    /// identical to [`Self::reorder_commit`] — `move_commits` resolves its target
    /// by id, so a commit that is no longer on the branch (but still in the store)
    /// is spliced in just the same — only the op-log message differs.
    pub fn restore_commit(
        &mut self,
        target: &CommitId,
        new_parent_ids: Vec<CommitId>,
        new_child_ids: Vec<CommitId>,
        new_tip: &CommitId,
    ) -> Result<SaveOutcome> {
        let desc = self.op_desc_for("Restore", target);
        self.splice_commit(
            target,
            new_parent_ids,
            new_child_ids,
            new_tip,
            "commedit: restore commit from trash",
            SpuriousResolve::Restore {
                commit: target.clone(),
            },
            desc,
        )
    }

    /// Shared body of [`Self::reorder_commit`] and [`Self::restore_commit`]: move
    /// `target` between `new_parent_ids` and `new_child_ids`, rebase descendants,
    /// point the branch at `new_tip`, and export — all in one transaction.
    #[allow(clippy::too_many_arguments)]
    fn splice_commit(
        &mut self,
        target: &CommitId,
        new_parent_ids: Vec<CommitId>,
        new_child_ids: Vec<CommitId>,
        new_tip: &CommitId,
        op_msg: &str,
        strategy: SpuriousResolve,
        desc: OpDescriptor,
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("moving the commit", || {
            self.splice_commit_inner(
                target,
                new_parent_ids,
                new_child_ids,
                new_tip,
                op_msg,
                strategy,
                desc,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn splice_commit_inner(
        &mut self,
        target: &CommitId,
        new_parent_ids: Vec<CommitId>,
        new_child_ids: Vec<CommitId>,
        new_tip: &CommitId,
        op_msg: &str,
        strategy: SpuriousResolve,
        desc: OpDescriptor,
    ) -> Result<SaveOutcome> {
        // Capture the on-disk working copy into @ so it rebases with the rewrite.
        self.snapshot_working_copy()?;
        let pre_op = self.repo.operation().clone();
        let old_head = self.head_commit();
        let heads = self.snapshot_heads();
        // A top-gap splice (no new children) puts the target above the old head
        // — where the working-copy chain also sits, and with no child to rebase,
        // nothing would carry it onto the new tip (the snapshot above just
        // re-attached it to the old head). Splice between the head and the
        // chain's bottom entry instead, so the uncommitted changes ride the
        // rebase onto the new tip like in any other splice.
        let mut new_child_ids = new_child_ids;
        if new_child_ids.is_empty() {
            if let Some(bottom) = self.working_copy_chain_ids().last() {
                new_child_ids.push(bottom.clone());
            }
        }
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
        .context("splicing commit")?;
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;

        // Point the branch at the new head. A splice need not rewrite the old
        // head commit, so jj's automatic bookmark moves can leave the branch
        // behind; set it explicitly. The head keeps its change id, but its commit
        // id changes if it was rebased onto a new parent. Setting it here (in the
        // rewrite transaction) is also what lets `finish_mutation` read the new
        // tip back from the bookmark to scope its conflict walk.
        let new_tip_id = match stats.rebased_commits.get(new_tip) {
            Some(RebasedCommit::Rewritten(commit)) => commit.id().clone(),
            Some(RebasedCommit::Abandoned { .. }) => bail!("the new head commit became empty"),
            None => new_tip.clone(),
        };
        self.set_head_bookmark(tx.repo_mut(), new_tip_id);

        self.finish_mutation_spurious(tx, op_msg, desc, pre_op, old_head, heads, strategy)
    }

    /// Drop `target` from history entirely: its descendants are rebased onto its
    /// parent(s) and the branch bookmark follows, in one transaction exported to
    /// git. The commit object itself survives for the session (we never run
    /// `git gc`), so [`Self::restore_commit`] can graft it back.
    pub fn abandon_commit(&mut self, target: &CommitId) -> Result<SaveOutcome> {
        crate::repo::catch_jj("dropping the commit", || self.abandon_commit_inner(target))
    }

    fn abandon_commit_inner(&mut self, target: &CommitId) -> Result<SaveOutcome> {
        // Capture the on-disk working copy into @ so it rebases with the rewrite.
        self.snapshot_working_copy()?;
        let pre_op = self.repo.operation().clone();
        let old_head = self.head_commit();
        let heads = self.snapshot_heads();
        let commit = self
            .repo
            .store()
            .get_commit(target)
            .context("loading target commit")?;
        let desc = self.op_desc_for("Drop", target);

        let mut tx = self.repo.start_transaction();
        // Record the abandon, then rebase: children re-parent onto the commit's
        // parents and any bookmark at it moves to the parent (jj's default keeps
        // abandoned bookmarks rather than deleting them).
        tx.repo_mut().record_abandoned_commit(&commit);
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;

        // Dropping removes the commit's change, so a descendant that edited an
        // adjacent-but-independent line conflicts only spuriously — including when
        // that descendant is the tip itself. Auto-resolve it by rebuilding the
        // conflicted range forward from the surviving commits' original changes.
        self.finish_mutation_spurious(
            tx,
            "commedit: drop commit",
            desc,
            pre_op,
            old_head,
            heads,
            SpuriousResolve::Drop,
        )
    }
}
