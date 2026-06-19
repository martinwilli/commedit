//! Split a commit into two — the engine primitive behind the diff view's "Split"
//! button. The user edits a commit's diff, then asks to split: the commit is
//! rewritten to the *edited* diff state (keeping its change id, message and
//! author), and a new commit holding the commit's *original* tree is inserted as
//! its child. The two together reproduce the original commit's diff, so every
//! descendant — and the branch tip and the working copy — is left unchanged.
//!
//! Mechanically this is jj's `split` with a fixed orientation: rewrite `C -> C'`
//! to the edited tree, `new_commit` `N` with the original tree as `C'`'s child,
//! then `set_rewritten_commit(C, N)` so `rebase_descendants` (and the bookmark and
//! `@`) follow `C -> N` rather than the `C -> C'` that `rewrite_commit` recorded.
//! `N` restores the original tree, the exact base the descendants were built on,
//! so the rebase is clean. No explicit head-bookmark move is needed (unlike
//! `reorder`): `C` is genuinely rewritten, so jj carries the bookmark for us.

use anyhow::{bail, Context, Result};
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::Repo as _;

use crate::conflict::SaveOutcome;
use crate::repo::Repo;
use crate::tree::FileEdit;

impl Repo {
    /// Split commit `target` from `(path, content)` write edits — the write-only
    /// convenience over [`Repo::split_commit_edits`] for the callers that never
    /// delete a path (the MCP tool, tests).
    pub fn split_commit(
        &mut self,
        target: &CommitId,
        files: &[(String, String)],
    ) -> Result<SaveOutcome> {
        let edits: Vec<FileEdit> = files
            .iter()
            .map(|(path, content)| FileEdit::write(path.clone(), content.clone()))
            .collect();
        self.split_commit_edits(target, &edits)
    }

    /// Split commit `target`: rewrite it to the edited diff given by `edits`
    /// ([`FileEdit`]s, as produced for [`Repo::rewrite_files_edits`] — a write
    /// edits content, a delete drops a file the commit added), and insert a new
    /// "fixup! …" commit holding the original tree as its child so the two
    /// combined reproduce the original commit. Descendants rebase onto the
    /// inserted commit through the shared export pipeline.
    pub fn split_commit_edits(
        &mut self,
        target: &CommitId,
        edits: &[FileEdit],
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("splitting the commit", || {
            self.split_commit_inner(target, edits)
        })
    }

    fn split_commit_inner(&mut self, target: &CommitId, edits: &[FileEdit]) -> Result<SaveOutcome> {
        if edits.is_empty() {
            bail!("nothing to split: the diff has no edits");
        }
        // Capture the on-disk working copy into @ so it rebases with the rewrite.
        self.snapshot_working_copy()?;
        let pre_op = self.repo.operation().clone();
        let old_head = self.edited_tip();
        let heads = self.snapshot_heads();
        let commit = self
            .repo
            .store()
            .get_commit(target)
            .context("loading target commit")?;
        let store = self.repo.store().clone();
        // T_orig: the commit's original tree, restored by the inserted commit so
        // descendants rebase onto exactly the base they were built on.
        let orig_tree = commit.tree();
        // T_edited: the same tree with the user's diff edits spliced in.
        let edited_tree = crate::tree::splice_edits_into_tree(commit.tree(), &store, edits)?;
        let author = commit.author().clone();
        let message = split_message(commit.description());
        let desc = self.op_desc_for("Split", target);

        let mut tx = self.repo.start_transaction();
        // C': the current commit, rewritten to the edited diff. Keeps its change
        // id, message and author.
        let edited = pollster::block_on(
            tx.repo_mut()
                .rewrite_commit(&commit)
                .set_tree(edited_tree)
                .write(),
        )
        .context("writing edited commit")?;
        // N: a new commit holding the original tree, inserted as C''s child.
        let split = pollster::block_on(
            tx.repo_mut()
                .new_commit(vec![edited.id().clone()], orig_tree)
                .set_description(message)
                .set_author(author)
                .write(),
        )
        .context("writing split commit")?;
        // Redirect the original commit's descendants — and the branch bookmark and
        // the working copy @ — onto N, overwriting the C->C' rewrite that
        // `rewrite_commit` recorded above. N restores the original tree, so they
        // rebase unchanged.
        tx.repo_mut()
            .set_rewritten_commit(commit.id().clone(), split.id().clone());
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;

        self.finish_mutation(tx, "commedit: split commit", desc, pre_op, old_head, heads)
    }

    /// Split a working-copy entry (identified by its stable change id, or the
    /// leaf `@` when `change_hex` is `None`) the same way [`Self::split_commit`]
    /// splits a history commit — but as a pure jj-side reorganization: the
    /// transaction is committed directly with no git export (like
    /// [`Self::edit_working_copy_file`]), so HEAD / refs / index / working tree
    /// are untouched and the on-disk content is byte-identical. The entry is
    /// rewritten to the edited diff and a new commit holding its original tree is
    /// inserted as its child, peeling the edit into a separate "uncommitted
    /// changes" entry (see [`Self::working_copy_chain`]).
    pub fn split_working_copy(
        &mut self,
        change_hex: Option<&str>,
        files: &[(String, String)],
    ) -> Result<()> {
        let edits: Vec<FileEdit> = files
            .iter()
            .map(|(path, content)| FileEdit::write(path.clone(), content.clone()))
            .collect();
        self.split_working_copy_edits(change_hex, &edits)
    }

    /// The [`FileEdit`] form of [`Self::split_working_copy`], so a revert that
    /// drops a file the entry adds peels through as a deletion.
    pub fn split_working_copy_edits(
        &mut self,
        change_hex: Option<&str>,
        edits: &[FileEdit],
    ) -> Result<()> {
        self.require_worktree("split the working copy")?;
        crate::repo::catch_jj("splitting the working copy", || {
            self.split_working_copy_inner(change_hex, edits)
        })
    }

    fn split_working_copy_inner(
        &mut self,
        change_hex: Option<&str>,
        edits: &[FileEdit],
    ) -> Result<()> {
        if edits.is_empty() {
            bail!("nothing to split: the diff has no edits");
        }
        // Snapshot the disk into the leaf @ first (its commit id churns here),
        // then resolve the target entry's stable change id to its current id.
        self.snapshot_working_copy()?;
        let target = self
            .resolve_working_copy_change(change_hex)
            .context("no working copy to split")?;
        let commit = self
            .repo
            .store()
            .get_commit(&target)
            .context("loading the working-copy entry")?;
        let change_hex = commit.change_id().hex();
        let store = self.repo.store().clone();
        let orig_tree = commit.tree();
        let edited_tree = crate::tree::splice_edits_into_tree(commit.tree(), &store, edits)?;

        let mut tx = self.repo.start_transaction();
        // E': the entry, rewritten to the edited diff (keeps its change id).
        let edited = pollster::block_on(
            tx.repo_mut()
                .rewrite_commit(&commit)
                .set_tree(edited_tree)
                .write(),
        )
        .context("writing edited entry")?;
        // N: a new commit holding the original tree, inserted as E''s child. It
        // restores the exact tree the leaf (and any deeper entries) were built
        // on, so they rebase unchanged; redirecting E -> N (overwriting the
        // E -> E' rewrite above) carries the working-copy pointer to N.
        let split = pollster::block_on(
            tx.repo_mut()
                .new_commit(vec![edited.id().clone()], orig_tree)
                .write(),
        )
        .context("writing split entry")?;
        tx.repo_mut()
            .set_rewritten_commit(commit.id().clone(), split.id().clone());
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;
        self.repo = pollster::block_on(tx.commit("commedit: split working copy"))
            .context("committing the working-copy split")?;

        // The leaf @ holds the unchanged full tree, so this re-checkout is a
        // no-op on disk; it just resets the git index to HEAD (unchanged).
        self.materialize_after_rewrite(self.head_commit())?;
        self.record_working_copy_op("Split uncommitted changes", change_hex);
        Ok(())
    }
}

/// Compose the inserted commit's message from the original commit's description:
/// `fixup! <subject>` — git's autosquash prefix pointing back at the original, so
/// the split-out commit reads (and re-folds, via `--autosquash`) as a fixup of the
/// edited remainder — where the subject is the first non-empty line. An empty
/// description yields just `fixup!`.
fn split_message(original: &str) -> String {
    let subject = original
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    format!("fixup! {subject}").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::split_message;

    #[test]
    fn split_message_uses_first_nonempty_line() {
        assert_eq!(
            split_message("Add feature\n\nbody text"),
            "fixup! Add feature"
        );
        assert_eq!(
            split_message("  \n  Real subject\nmore"),
            "fixup! Real subject"
        );
    }

    #[test]
    fn split_message_handles_empty_description() {
        assert_eq!(split_message(""), "fixup!");
        assert_eq!(split_message("\n\n"), "fixup!");
    }
}
