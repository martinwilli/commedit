//! Rewrite a file's content within a historical commit's tree — the engine
//! primitive behind hunk editing. Reuses the same rewrite/rebase/transparency
//! pipeline as message editing.

use std::sync::Arc;

use anyhow::{Context, Result};
use jj_lib::backend::{CommitId, CopyId, TreeValue};
use jj_lib::matchers::FilesMatcher;
use jj_lib::merge::{Merge, MergedTreeValue};
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::store::Store;

use crate::conflict::SaveOutcome;
use crate::repo::Repo;

impl Repo {
    /// Replace the content of `path` in commit `target` with `new_content`,
    /// rebase descendants onto the rewritten commit, and export to git — all in
    /// a single transaction. The file's executable bit is preserved. A thin
    /// wrapper over [`Repo::rewrite_files`] for the single-file case.
    pub fn rewrite_file(
        &mut self,
        target: &CommitId,
        path: &str,
        new_content: &str,
    ) -> Result<SaveOutcome> {
        self.rewrite_files(target, &[(path.to_string(), new_content.to_string())])
    }

    /// Replace the content of several files in commit `target` at once, splicing
    /// every blob into the commit's tree in one [`MergedTreeBuilder`] pass and one
    /// transaction (so a single Save touching many files is one rewrite). Each
    /// file's executable bit and copy id are preserved. `files` is `(path,
    /// content)` pairs.
    pub fn rewrite_files(
        &mut self,
        target: &CommitId,
        files: &[(String, String)],
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("editing the file", || {
            self.rewrite_files_inner(target, files)
        })
    }

    fn rewrite_files_inner(
        &mut self,
        target: &CommitId,
        files: &[(String, String)],
    ) -> Result<SaveOutcome> {
        // Capture the on-disk working copy into @ so it rebases with the rewrite.
        self.snapshot_working_copy()?;
        let pre_op = self.repo.operation().clone();
        let old_head = self.head_commit();
        let bookmarks = self.local_bookmark_targets();
        let heads = self.snapshot_heads();
        let commit = self
            .repo
            .store()
            .get_commit(target)
            .context("loading target commit")?;
        let store = self.repo.store().clone();
        let new_tree = splice_files_into_tree(commit.tree(), &store, files)?;
        let desc = self.op_desc_for("Edit files of", target);

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
            desc,
            pre_op,
            old_head,
            bookmarks,
            heads,
        )
    }
}

/// Splice new content for several files into `base_tree`, returning the written
/// tree. Each file's blob is written to `store` and set into a single
/// [`MergedTreeBuilder`] pass; each file's executable bit and copy id are preserved
/// from `base_tree`. Shared by [`Repo::rewrite_files`] and [`Repo::split_commit`].
pub(crate) fn splice_files_into_tree(
    base_tree: MergedTree,
    store: &Arc<Store>,
    files: &[(String, String)],
) -> Result<MergedTree> {
    // Write each new blob and gather the spliced (path, value) pairs up front,
    // while `base_tree` is still borrowable for the metadata lookups; then move
    // it into the builder.
    let mut entries: Vec<(RepoPathBuf, MergedTreeValue)> = Vec::with_capacity(files.len());
    for (path, content) in files {
        let repo_path = RepoPathBuf::from_internal_string(path).context("invalid path")?;
        let (executable, copy_id) = existing_file_meta(&base_tree, &repo_path);
        let mut reader: &[u8] = content.as_bytes();
        let file_id = pollster::block_on(store.write_file(&repo_path, &mut reader))
            .context("writing file blob")?;
        let value = TreeValue::File {
            id: file_id,
            executable,
            copy_id,
        };
        entries.push((repo_path, Merge::normal(value)));
    }
    let mut builder = MergedTreeBuilder::new(base_tree);
    for (repo_path, value) in entries {
        builder.set_or_remove(repo_path, value);
    }
    pollster::block_on(builder.write_tree()).context("writing tree")
}

/// Look up the executable bit and copy id of an existing file at `repo_path`,
/// so a content edit preserves them. Defaults for a path that isn't a resolved
/// file (e.g. newly added).
pub(crate) fn existing_file_meta(base_tree: &MergedTree, repo_path: &RepoPath) -> (bool, CopyId) {
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
