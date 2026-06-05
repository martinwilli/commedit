//! Extract the per-file changes a commit introduces (vs. its parent), with text
//! content for the history/hunk view.

use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use jj_lib::backend::{CommitId, TreeValue};
use jj_lib::matchers::EverythingMatcher;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::repo_path::RepoPath;
use jj_lib::store::Store;
use tokio::io::AsyncReadExt;

/// How a file changed in a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

/// A single file's change within a commit.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Path relative to the repo root (internal, forward-slash form).
    pub path: String,
    pub kind: ChangeKind,
    /// File content in the parent (`None` if absent or binary).
    pub old_text: Option<String>,
    /// File content in the commit (`None` if absent or binary).
    pub new_text: Option<String>,
    /// True if either side is non-UTF-8 (not editable as text).
    pub is_binary: bool,
}

/// List the file changes a commit introduces relative to its parent tree.
pub fn commit_changes(repo: &ReadonlyRepo, commit_id: &CommitId) -> Result<Vec<FileChange>> {
    let store = repo.store().clone();
    let commit = store.get_commit(commit_id).context("loading commit")?;
    let new_tree = commit.tree();
    let parent_tree = pollster::block_on(commit.parent_tree(repo)).context("parent tree")?;

    let entries = pollster::block_on(
        parent_tree
            .diff_stream(&new_tree, &EverythingMatcher)
            .collect::<Vec<_>>(),
    );

    let mut changes = Vec::new();
    for entry in entries {
        let diff = entry.values.context("computing file diff")?;
        let before = diff.before.into_resolved().ok().flatten();
        let after = diff.after.into_resolved().ok().flatten();

        let (old_text, old_binary) = read_text(&store, &entry.path, before.as_ref())?;
        let (new_text, new_binary) = read_text(&store, &entry.path, after.as_ref())?;
        let kind = match (before.is_some(), after.is_some()) {
            (false, true) => ChangeKind::Added,
            (true, false) => ChangeKind::Removed,
            _ => ChangeKind::Modified,
        };
        changes.push(FileChange {
            path: entry.path.as_internal_file_string().to_string(),
            kind,
            old_text,
            new_text,
            is_binary: old_binary || new_binary,
        });
    }
    Ok(changes)
}

/// Read a file value as UTF-8 text. Returns `(None, true)` for binary content,
/// `(None, false)` for non-file/absent values.
fn read_text(
    store: &Arc<Store>,
    path: &RepoPath,
    value: Option<&TreeValue>,
) -> Result<(Option<String>, bool)> {
    let Some(TreeValue::File { id, .. }) = value else {
        return Ok((None, false));
    };
    let mut reader = pollster::block_on(store.read_file(path, id)).context("reading blob")?;
    let mut buf = Vec::new();
    pollster::block_on(reader.read_to_end(&mut buf)).context("reading blob bytes")?;
    match String::from_utf8(buf) {
        Ok(text) => Ok((Some(text), false)),
        Err(_) => Ok((None, true)),
    }
}
