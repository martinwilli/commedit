//! Create brand-new commits and insert them into history.
//!
//! [`Repo::create_commit`] synthesizes a commit from given file contents;
//! [`Repo::revert_commit`] from the inverse of an existing commit's diff. Both
//! splice the new commit into the graph the same way [`Repo::restore_commit`]
//! grafts a trashed commit back — a fresh commit is structurally a "restore" of
//! one that was never in the history. The shared [`Repo::insert_new_commit`]
//! mirrors `splice_commit_inner`: create the commit, `move_commits` it between
//! the chosen parents/children (injecting the working-copy chain's bottom as the
//! child for a top-of-HEAD insert, so uncommitted changes ride on top untouched),
//! point the branch at the new tip, and run the deferred export through
//! `finish_mutation`.

use anyhow::{bail, Context, Result};
use jj_lib::backend::{CommitId, Signature};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::{
    move_commits, MoveCommitsLocation, MoveCommitsTarget, RebaseOptions, RebasedCommit,
};

use crate::conflict::{OpDescriptor, SaveOutcome, SpuriousResolve};
use crate::history::parse_timestamp;
use crate::repo::Repo;
use crate::rewrite::Identity;
use crate::tree::{splice_edits_into_tree, FileEdit};

impl Repo {
    /// Create a brand-new commit with `message` and `edits` (whole-file writes or
    /// deletions, spliced onto the parent's tree; empty → an empty commit) and
    /// splice it between `new_parent_ids` and `new_child_ids` — the slot a
    /// reorder/restore plan resolves (`new_child_ids` empty puts it on top of
    /// HEAD as the new tip, beneath any uncommitted changes). `identity` overrides
    /// the author/committer; `None` uses the repo's git-configured user at "now".
    /// Exported to git in one transaction; descendants of a mid-history insert may
    /// report conflicts.
    pub fn create_commit(
        &mut self,
        new_parent_ids: Vec<CommitId>,
        new_child_ids: Vec<CommitId>,
        message: &str,
        identity: Option<&Identity>,
        edits: &[FileEdit],
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("creating the commit", || {
            self.create_commit_inner(new_parent_ids, new_child_ids, message, identity, edits)
        })
    }

    fn create_commit_inner(
        &mut self,
        new_parent_ids: Vec<CommitId>,
        new_child_ids: Vec<CommitId>,
        message: &str,
        identity: Option<&Identity>,
        edits: &[FileEdit],
    ) -> Result<SaveOutcome> {
        let parent = self
            .repo
            .store()
            .get_commit(&new_parent_ids[0])
            .context("loading the parent commit")?;
        let store = self.repo.store().clone();
        let tree = if edits.is_empty() {
            parent.tree()
        } else {
            splice_edits_into_tree(parent.tree(), &store, edits)?
        };
        let subject = message.lines().next().unwrap_or("").trim();
        let label = if subject.is_empty() {
            "Create commit".to_string()
        } else {
            format!("Create \"{subject}\"")
        };
        self.insert_new_commit(
            new_parent_ids,
            new_child_ids,
            tree,
            message,
            identity,
            label,
        )
    }

    /// Create a commit that reverts `target`'s change (its inverse diff applied
    /// onto the insertion parent's tree, like `git revert`) and splice it in at
    /// the slot given by `new_parent_ids`/`new_child_ids` (see
    /// [`Self::create_commit`]). A merge commit cannot be reverted (no single
    /// parent to invert). Exported to git in one transaction; the revert may
    /// itself conflict where the insertion point diverged from `target`.
    pub fn revert_commit(
        &mut self,
        target: &CommitId,
        new_parent_ids: Vec<CommitId>,
        new_child_ids: Vec<CommitId>,
        identity: Option<&Identity>,
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("reverting the commit", || {
            self.revert_commit_inner(target, new_parent_ids, new_child_ids, identity)
        })
    }

    fn revert_commit_inner(
        &mut self,
        target: &CommitId,
        new_parent_ids: Vec<CommitId>,
        new_child_ids: Vec<CommitId>,
        identity: Option<&Identity>,
    ) -> Result<SaveOutcome> {
        let store = self.repo.store().clone();
        let commit = store
            .get_commit(target)
            .context("loading the commit to revert")?;
        if commit.parent_ids().len() != 1 {
            bail!("cannot revert a merge commit");
        }
        let reverted_parent = store
            .get_commit(&commit.parent_ids()[0])
            .context("loading the reverted commit's parent")?;
        let insert_parent = store
            .get_commit(&new_parent_ids[0])
            .context("loading the parent commit")?;
        // Apply the inverse of `target`'s diff onto the insertion parent's tree as
        // a 3-way merge: base = target's tree, "theirs" = target's parent's tree
        // (the reverted state for the files target touched), "ours" = the
        // insertion parent's tree. Paths target left alone stay as ours; paths it
        // changed revert to its parent's content; an overlap conflicts.
        let tree = pollster::block_on(MergedTree::merge(Merge::from_vec(vec![
            (insert_parent.tree(), "revert destination".to_string()),
            (commit.tree(), "the reverted commit".to_string()),
            (
                reverted_parent.tree(),
                "before the reverted commit".to_string(),
            ),
        ])))
        .context("computing the reverted tree")?;
        let subject = commit.description().lines().next().unwrap_or("").trim();
        let message = format!(
            "Revert \"{subject}\"\n\nThis reverts commit {}.\n",
            target.hex()
        );
        let label = format!("Revert \"{subject}\"");
        self.insert_new_commit(
            new_parent_ids,
            new_child_ids,
            tree,
            &message,
            identity,
            label,
        )
    }

    /// Shared body of [`Self::create_commit`]/[`Self::revert_commit`]: write a new
    /// commit holding `tree`, splice it between `new_parent_ids`/`new_child_ids`,
    /// rebase descendants, point the branch at the resulting tip, and export — all
    /// in one transaction. See the module docs for the splice/working-copy story.
    #[allow(clippy::too_many_arguments)]
    fn insert_new_commit(
        &mut self,
        new_parent_ids: Vec<CommitId>,
        new_child_ids: Vec<CommitId>,
        tree: MergedTree,
        message: &str,
        identity: Option<&Identity>,
        label: String,
    ) -> Result<SaveOutcome> {
        // Capture the on-disk working copy into @ so it rebases with the insert.
        self.snapshot_working_copy()?;
        let pre_op = self.repo.operation().clone();
        let old_head = self.head_commit();
        let head_id = self.head_commit_id();
        let heads = self.snapshot_heads();

        // A top-gap insert (no new children) puts the commit above the old head —
        // where the working-copy chain also sits, and with no child to rebase
        // nothing would carry @ onto the new tip (it would be a sibling of @).
        // Splice between the head and the chain's bottom entry instead, so the
        // uncommitted changes ride the rebase onto the new tip (mirrors
        // splice_commit_inner).
        let is_top = new_child_ids.is_empty();
        let mut new_child_ids = new_child_ids;
        if new_child_ids.is_empty() {
            if let Some(bottom) = self.working_copy_chain_ids().last() {
                new_child_ids.push(bottom.clone());
            }
        }

        let mut tx = self.repo.start_transaction();
        let mut builder = tx
            .repo_mut()
            .new_commit(new_parent_ids.clone(), tree)
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
        let created = pollster::block_on(builder.write()).context("writing the new commit")?;
        let created_id = created.id().clone();
        let affected = vec![created.change_id().hex()];

        let loc = MoveCommitsLocation {
            new_parent_ids,
            new_child_ids,
            target: MoveCommitsTarget::Commits(vec![created_id.clone()]),
        };
        let stats =
            pollster::block_on(move_commits(tx.repo_mut(), &loc, &RebaseOptions::default()))
                .context("splicing the new commit")?;
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;

        // The branch tip: a top-gap insert makes the new commit the newest real
        // commit (@ rides above it, unexported); otherwise the old head, rewritten
        // as its ancestry rebased onto the inserted commit.
        let tip = if is_top {
            created_id.clone()
        } else {
            match head_id.as_ref().and_then(|h| stats.rebased_commits.get(h)) {
                Some(RebasedCommit::Rewritten(c)) => c.id().clone(),
                Some(RebasedCommit::Abandoned { .. }) => bail!("the head commit became empty"),
                None => head_id.clone().unwrap_or_else(|| created_id.clone()),
            }
        };
        self.set_head_bookmark(tx.repo_mut(), tip);

        // Inserting a commit adds a change to the set, exactly like restoring a
        // trashed one, so opt into the same forward-rebuild spurious-conflict
        // auto-resolve; a genuine overlap falls through to the manual flow.
        let desc = OpDescriptor::new(label, affected);
        self.finish_mutation_spurious(
            tx,
            "commedit: create commit",
            desc,
            pre_op,
            old_head,
            heads,
            SpuriousResolve::Restore { commit: created_id },
        )
    }
}
