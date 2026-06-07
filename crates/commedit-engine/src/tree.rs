//! Rewrite a file's content within a historical commit's tree — the engine
//! primitive behind hunk editing. Reuses the same rewrite/rebase/transparency
//! pipeline as message editing.

use anyhow::{Context, Result};
use jj_lib::backend::{CommitId, CopyId, TreeValue};
use jj_lib::matchers::FilesMatcher;
use jj_lib::merge::{Merge, MergedTreeValue};
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};

use crate::conflict::SaveOutcome;
use crate::repo::Repo;

impl Repo {
    /// Replace the content of `path` in commit `target` with `new_content`,
    /// rebase descendants onto the rewritten commit, and export to git — all in
    /// a single transaction. The file's executable bit is preserved.
    pub fn rewrite_file(
        &mut self,
        target: &CommitId,
        path: &str,
        new_content: &str,
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("editing the file", || {
            self.rewrite_file_inner(target, path, new_content)
        })
    }

    fn rewrite_file_inner(
        &mut self,
        target: &CommitId,
        path: &str,
        new_content: &str,
    ) -> Result<SaveOutcome> {
        // Capture the on-disk working copy into @ so it rebases with the rewrite.
        self.snapshot_working_copy()?;
        let pre_op = self.repo.operation().clone();
        let old_head = self.head_commit();
        let bookmarks = self.local_bookmark_targets();
        let heads = self.snapshot_heads();
        let repo_path = RepoPathBuf::from_internal_string(path).context("invalid path")?;
        let commit = self
            .repo
            .store()
            .get_commit(target)
            .context("loading target commit")?;
        let base_tree = commit.tree();
        let (executable, copy_id) = existing_file_meta(&base_tree, &repo_path);

        // Write the new blob and splice it into the commit's tree.
        let store = self.repo.store().clone();
        let mut reader: &[u8] = new_content.as_bytes();
        let file_id = pollster::block_on(store.write_file(&repo_path, &mut reader))
            .context("writing file blob")?;
        let value = TreeValue::File {
            id: file_id,
            executable,
            copy_id,
        };
        let mut builder = MergedTreeBuilder::new(base_tree);
        let merged: MergedTreeValue = Merge::normal(value);
        builder.set_or_remove(repo_path, merged);
        let new_tree = pollster::block_on(builder.write_tree()).context("writing tree")?;

        let mut tx = self.repo.start_transaction();
        pollster::block_on(
            tx.repo_mut()
                .rewrite_commit(&commit)
                .set_tree(new_tree)
                .write(),
        )
        .context("writing rewritten commit")?;
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;

        self.finish_mutation(
            tx,
            "commedit: edit file content",
            pre_op,
            old_head,
            bookmarks,
            heads,
        )
    }
}

/// Look up the executable bit and copy id of an existing file at `repo_path`,
/// so a content edit preserves them. Defaults for a path that isn't a resolved
/// file (e.g. newly added).
fn existing_file_meta(base_tree: &MergedTree, repo_path: &RepoPath) -> (bool, CopyId) {
    let matcher = FilesMatcher::new([repo_path]);
    for (_path, value) in base_tree.entries_matching(&matcher) {
        if let Ok(merged) = value {
            if let Some(TreeValue::File {
                executable,
                copy_id,
                ..
            }) = merged.into_resolved().ok().flatten()
            {
                return (executable, copy_id);
            }
        }
    }
    (false, CopyId::placeholder())
}
