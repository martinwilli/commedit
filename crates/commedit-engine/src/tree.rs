//! Rewrite a file's content within a historical commit's tree — the engine
//! primitive behind hunk editing. Reuses the same rewrite/rebase/transparency
//! pipeline as message editing.

use std::collections::BTreeMap;
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

/// A whole-file edit for splicing into a tree: write `content`, or delete the
/// path when `content` is `None`. Shared by [`Repo::rewrite_files`],
/// [`Repo::create_commit`] and [`Repo::revert_commit`].
#[derive(Debug, Clone)]
pub struct FileEdit {
    /// Path relative to the repository root, forward-slash form.
    pub path: String,
    /// The file's complete new content, or `None` to delete the path.
    pub content: Option<String>,
}

impl FileEdit {
    /// A write edit: set `path` to `content`.
    pub fn write(path: String, content: String) -> Self {
        Self {
            path,
            content: Some(content),
        }
    }

    /// A delete edit: remove `path` from the tree (a no-op if it is absent).
    pub fn delete(path: String) -> Self {
        Self {
            path,
            content: None,
        }
    }
}

/// One targeted text replacement within a file of a commit: substitute `new`
/// for `old`. The surgical counterpart to a whole-file [`FileEdit`].
#[derive(Debug, Clone)]
pub struct StrReplace {
    /// Path relative to the repository root, forward-slash form.
    pub path: String,
    /// The exact text to find.
    pub old: String,
    /// The text to substitute in.
    pub new: String,
    /// Replace every occurrence instead of requiring a unique match.
    pub all: bool,
}

/// A caller-fixable replacement failure: the fix is to amend `old`, so the MCP
/// layer downcasts these onto an `invalid` response rather than `internal`.
#[derive(Debug, thiserror::Error)]
pub enum ReplaceError {
    #[error("{path}: `old` was not found")]
    NotFound { path: String },
    #[error(
        "{path}: `old` matched {count} times; extend it with surrounding text \
         to make it unique, or set replace_all"
    )]
    Ambiguous { path: String, count: usize },
    #[error("{path}: not an editable text file in this commit (binary or absent)")]
    NotText { path: String },
}

/// Apply a unique-by-default text replacement: substitute `new` for `old` in
/// `text`. Requires exactly one occurrence of `old` unless `all` is set (then
/// every occurrence is replaced). On failure returns the occurrence count
/// (`0` = not found, `>1` = ambiguous) so each caller shapes its own error.
/// The shared core of the `replace_in_file` / `replace_in_message` MCP tools;
/// callers must pass a non-empty `old` (they report an empty one as a caller
/// error, since `str::matches("")` would spuriously match everywhere).
pub fn replace_checked(text: &str, old: &str, new: &str, all: bool) -> Result<String, usize> {
    let count = text.matches(old).count();
    match count {
        1 => Ok(text.replacen(old, new, 1)),
        n if all && n > 0 => Ok(text.replace(old, new)),
        n => Err(n),
    }
}

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

    /// Replace the content of several files in commit `target` at once, in one
    /// transaction. A write-only convenience over [`Repo::rewrite_files_edits`]
    /// for the `(path, content)` callers; each file's executable bit and copy id
    /// are preserved.
    pub fn rewrite_files(
        &mut self,
        target: &CommitId,
        files: &[(String, String)],
    ) -> Result<SaveOutcome> {
        let edits: Vec<FileEdit> = files
            .iter()
            .map(|(path, content)| FileEdit::write(path.clone(), content.clone()))
            .collect();
        self.rewrite_files_edits(target, &edits)
    }

    /// Apply several whole-file edits to commit `target` at once, splicing every
    /// blob (or deletion) into the commit's tree in one [`MergedTreeBuilder`] pass
    /// and one transaction (so a single Save touching many files is one rewrite).
    /// Each written file's executable bit and copy id are preserved; a [`FileEdit`]
    /// with no content removes the path. Descendants are rebased and may conflict.
    pub fn rewrite_files_edits(
        &mut self,
        target: &CommitId,
        edits: &[FileEdit],
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("editing the file", || {
            self.rewrite_files_inner(target, edits)
        })
    }

    fn rewrite_files_inner(
        &mut self,
        target: &CommitId,
        edits: &[FileEdit],
    ) -> Result<SaveOutcome> {
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
        let store = self.repo.store().clone();
        let new_tree = splice_edits_into_tree(commit.tree(), &store, edits)?;
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
            heads,
        )
    }

    /// Apply targeted `old`→`new` text replacements across files of commit
    /// `target` — the surgical counterpart to [`Repo::rewrite_files_edits`].
    /// Each file's current content is read from `target`'s tree, the
    /// replacement applied (unique unless `StrReplace::all`), and the result
    /// spliced in via the same rewrite/rebase/export pipeline. Several edits
    /// may target one path; they compose in order. A miss, an ambiguous match
    /// or a non-text path returns a [`ReplaceError`] (downcastable for a clean
    /// caller-facing message); descendants are rebased and may conflict.
    pub fn replace_in_files(
        &mut self,
        target: &CommitId,
        replaces: &[StrReplace],
    ) -> Result<SaveOutcome> {
        let commit = self
            .repo
            .store()
            .get_commit(target)
            .context("loading target commit")?;
        let store = self.repo.store().clone();
        let tree = commit.tree();
        // Edited content per path, so repeated edits to one file compose.
        let mut edited: BTreeMap<String, String> = BTreeMap::new();
        for r in replaces {
            let current = match edited.remove(&r.path) {
                Some(text) => text,
                None => read_path_text(&tree, &store, &r.path)?.ok_or_else(|| {
                    ReplaceError::NotText {
                        path: r.path.clone(),
                    }
                })?,
            };
            let next = replace_checked(&current, &r.old, &r.new, r.all).map_err(|count| {
                if count == 0 {
                    ReplaceError::NotFound {
                        path: r.path.clone(),
                    }
                } else {
                    ReplaceError::Ambiguous {
                        path: r.path.clone(),
                        count,
                    }
                }
            })?;
            edited.insert(r.path.clone(), next);
        }
        let edits: Vec<FileEdit> = edited
            .into_iter()
            .map(|(path, content)| FileEdit::write(path, content))
            .collect();
        self.rewrite_files_edits(target, &edits)
    }
}

/// Splice new content for several files into `base_tree`, returning the written
/// tree. A write-only thin wrapper over [`splice_edits_into_tree`] for the
/// `(path, content)` callers ([`Repo::split_commit`] and the spurious-conflict
/// rebuild in [`crate::conflict`]).
pub(crate) fn splice_files_into_tree(
    base_tree: MergedTree,
    store: &Arc<Store>,
    files: &[(String, String)],
) -> Result<MergedTree> {
    let edits: Vec<FileEdit> = files
        .iter()
        .map(|(path, content)| FileEdit::write(path.clone(), content.clone()))
        .collect();
    splice_edits_into_tree(base_tree, store, &edits)
}

/// Splice whole-file *values* for `paths` from `source` into `base_tree`,
/// returning the written tree. Unlike [`splice_files_into_tree`] this copies each
/// path's `TreeValue` verbatim — the executable bit, copy id and (binary)
/// content ride inside the value rather than round-tripping through `String` — so
/// it is the binary/exec-safe way to lift a file from one tree to another. A path
/// absent in `source` is committed as a deletion. Used by the `paths` tier of
/// [`Repo::commit_working_copy_partial`] to take a whole file from the leaf `@`.
pub(crate) fn splice_paths_from_tree(
    base_tree: MergedTree,
    source: &MergedTree,
    paths: &[String],
) -> Result<MergedTree> {
    let mut entries: Vec<(RepoPathBuf, MergedTreeValue)> = Vec::with_capacity(paths.len());
    for path in paths {
        let repo_path = RepoPathBuf::from_internal_string(path).context("invalid path")?;
        // `path_value` yields `Merge::absent()` for a path missing in `source`,
        // which `set_or_remove` turns into a deletion.
        let value = pollster::block_on(source.path_value(&repo_path)).context("reading path")?;
        entries.push((repo_path, value));
    }
    let mut builder = MergedTreeBuilder::new(base_tree);
    for (repo_path, value) in entries {
        builder.set_or_remove(repo_path, value);
    }
    pollster::block_on(builder.write_tree()).context("writing tree")
}

/// Apply whole-file edits to `base_tree`, returning the written tree. Each
/// written blob goes to `store` and is set into a single [`MergedTreeBuilder`]
/// pass (so a multi-file save is one tree write); a [`FileEdit`] with no content
/// removes the path. A written file's executable bit and copy id are preserved
/// from `base_tree`. Shared by [`Repo::rewrite_files`], [`Repo::create_commit`]
/// and [`Repo::revert_commit`].
pub(crate) fn splice_edits_into_tree(
    base_tree: MergedTree,
    store: &Arc<Store>,
    edits: &[FileEdit],
) -> Result<MergedTree> {
    // Write each new blob and gather the spliced (path, value) pairs up front,
    // while `base_tree` is still borrowable for the metadata lookups; then move
    // it into the builder.
    let mut entries: Vec<(RepoPathBuf, MergedTreeValue)> = Vec::with_capacity(edits.len());
    for edit in edits {
        let repo_path = RepoPathBuf::from_internal_string(&edit.path).context("invalid path")?;
        let value = match &edit.content {
            Some(content) => {
                let (executable, copy_id) = existing_file_meta(&base_tree, &repo_path);
                let mut reader: &[u8] = content.as_bytes();
                let file_id = pollster::block_on(store.write_file(&repo_path, &mut reader))
                    .context("writing file blob")?;
                Merge::normal(TreeValue::File {
                    id: file_id,
                    executable,
                    copy_id,
                })
            }
            // An absent value removes the path (a no-op if it isn't there).
            None => Merge::absent(),
        };
        entries.push((repo_path, value));
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

/// Read a path's UTF-8 content from `tree` (reusing [`crate::diff::read_text`]).
/// `None` when the path is absent or its content is binary — both are reported
/// to the caller as a non-text replacement target. Err only on a real backend
/// failure.
fn read_path_text(tree: &MergedTree, store: &Arc<Store>, path: &str) -> Result<Option<String>> {
    let repo_path = RepoPathBuf::from_internal_string(path).context("invalid path")?;
    let value = pollster::block_on(tree.path_value(&repo_path)).context("reading path")?;
    let resolved = value.into_resolved().ok().flatten();
    Ok(crate::diff::read_text(store, &repo_path, resolved.as_ref())?.0)
}

#[cfg(test)]
mod tests {
    use super::replace_checked;

    #[test]
    fn unique_match_is_replaced() {
        assert_eq!(
            replace_checked("the bulck form of edit", "bulck", "bulk", false).unwrap(),
            "the bulk form of edit"
        );
    }

    #[test]
    fn missing_match_reports_zero() {
        assert_eq!(
            replace_checked("nothing to see", "bulck", "bulk", false),
            Err(0)
        );
    }

    #[test]
    fn ambiguous_match_reports_the_count() {
        assert_eq!(replace_checked("a a a", "a", "b", false), Err(3));
    }

    #[test]
    fn replace_all_takes_every_occurrence() {
        assert_eq!(replace_checked("a a a", "a", "b", true).unwrap(), "b b b");
    }
}
