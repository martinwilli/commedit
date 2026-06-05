//! Extract the per-file changes a commit introduces (vs. its parent), with text
//! content for the history/hunk view.

use std::sync::Arc;

use anyhow::{Context, Result};
use futures::StreamExt;
use similar::{ChangeTag, TextDiff};
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

/// Render an editable full-context line diff of `old` → `new`. Every line is
/// prefixed with ` ` (context), `-` (removed) or `+` (added). Inputs are
/// normalized to end with a newline so each diff line is newline-terminated;
/// full context (no elision) makes the diff reconstructible from the buffer
/// alone — see [`reconstruct_from_diff`].
pub fn unified_diff(old: &str, new: &str) -> String {
    let old = ensure_trailing_newline(old);
    let new = ensure_trailing_newline(new);
    let diff = TextDiff::from_lines(old.as_ref(), new.as_ref());
    let mut out = String::new();
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Equal => ' ',
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
        };
        out.push(sign);
        out.push_str(change.value());
    }
    out
}

/// Ensure non-empty text ends with a newline (leaves empty text untouched).
fn ensure_trailing_newline(text: &str) -> std::borrow::Cow<'_, str> {
    if text.is_empty() || text.ends_with('\n') {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(format!("{text}\n"))
    }
}

/// Reconstruct file content from a (possibly edited) full-context diff produced
/// by [`unified_diff`]: keep context (` `) and added (`+`) lines, drop removed
/// (`-`) lines and any other markers. This mirrors how `git add -e` interprets an
/// edited diff, and round-trips `unified_diff` exactly when unedited.
pub fn reconstruct_from_diff(edited: &str) -> String {
    let mut out = String::new();
    for line in edited.split_inclusive('\n') {
        match line.chars().next() {
            Some(' ') | Some('+') => out.push_str(&line[1..]),
            _ => {}
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::{reconstruct_from_diff, unified_diff};

    /// An unedited diff reconstructs the new content (newline-normalized).
    fn assert_roundtrip(old: &str, new: &str) {
        let diff = unified_diff(old, new);
        let expected = if new.is_empty() || new.ends_with('\n') {
            new.to_string()
        } else {
            format!("{new}\n")
        };
        assert_eq!(reconstruct_from_diff(&diff), expected, "diff was:\n{diff}");
    }

    #[test]
    fn roundtrips() {
        assert_roundtrip("a\nb\nc\n", "a\nB\nc\n");
        assert_roundtrip("", "added\nlines\n");
        assert_roundtrip("removed\nlines\n", "");
        assert_roundtrip("a\nb\nc\n", "a\nb\nc\n");
        // No trailing newline (normalized to one on reconstruct).
        assert_roundtrip("x\ny", "x\nY");
        // Content lines that themselves start with diff sigils.
        assert_roundtrip(" +keep\n-keep\n", " +keep\n-keep\n+extra\n");
    }

    #[test]
    fn editing_an_added_line_changes_output() {
        let diff = unified_diff("a\n", "a\nb\n");
        // The added line "+b" -> edit to "+B".
        let edited = diff.replace("+b\n", "+B\n");
        assert_eq!(reconstruct_from_diff(&edited), "a\nB\n");
    }

    #[test]
    fn flipping_a_removed_line_to_kept_restores_it() {
        let diff = unified_diff("keep\ndrop\n", "keep\n");
        // Turn the "-drop" line into a context line by replacing its sigil.
        let edited = diff.replace("-drop\n", " drop\n");
        assert_eq!(reconstruct_from_diff(&edited), "keep\ndrop\n");
    }
}
