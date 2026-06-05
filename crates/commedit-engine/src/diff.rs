//! Extract the per-file changes a commit introduces (vs. its parent), with text
//! content for the history/hunk view.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use similar::TextDiff;
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

/// Render a standard unified diff of `old` → `new` with `@@` hunk headers and
/// the usual context (3 lines). Inputs are newline-normalized. The result is
/// meant to be edited and fed back to [`apply_patch`]!
pub fn unified_diff(old: &str, new: &str, path: &str) -> String {
    let old = ensure_trailing_newline(old);
    let new = ensure_trailing_newline(new);
    let diff = TextDiff::from_lines(old.as_ref(), new.as_ref());
    let mut formatter = diff.unified_diff();
    formatter
        .context_radius(3)
        .missing_newline_hint(false)
        .header(&format!("a/{path}"), &format!("b/{path}"));
    formatter.to_string()
}

/// Apply a (possibly edited) unified diff to `old`, returning the new content.
///
/// Each hunk is anchored by its old-side start line (the `@@ -start,.. @@`
/// number), not by the line counts, so editing/adding/removing `+` lines works
/// without keeping the counts accurate. Context and removed (`-`) lines are
/// verified against `old`; a mismatch (e.g. an edited context line) fails with a
/// descriptive error rather than silently corrupting the file. The result is
/// newline-normalized.
pub fn apply_patch(old: &str, patch: &str) -> Result<String> {
    let old = ensure_trailing_newline(old);
    let old_lines: Vec<&str> = old.lines().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut cursor = 0usize;
    let mut in_hunk = false;

    for raw in patch.lines() {
        if raw.starts_with("@@") {
            let start = parse_hunk_old_start(raw)?.saturating_sub(1);
            if start < cursor {
                bail!("out-of-order hunk at old line {}", start + 1);
            }
            if start > old_lines.len() {
                bail!("hunk starts past end of file (old line {})", start + 1);
            }
            out.extend_from_slice(&old_lines[cursor..start]);
            cursor = start;
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue; // file header / preamble before the first hunk
        }
        match raw.chars().next() {
            Some(' ') => {
                let content = &raw[1..];
                if old_lines.get(cursor).copied() != Some(content) {
                    bail!(
                        "context mismatch at old line {}: patch has {content:?}, file has {:?}",
                        cursor + 1,
                        old_lines.get(cursor)
                    );
                }
                out.push(content);
                cursor += 1;
            }
            Some('-') => {
                let content = &raw[1..];
                if old_lines.get(cursor).copied() != Some(content) {
                    bail!(
                        "removed-line mismatch at old line {}: patch has {content:?}, file has {:?}",
                        cursor + 1,
                        old_lines.get(cursor)
                    );
                }
                cursor += 1;
            }
            Some('+') => out.push(&raw[1..]),
            Some('\\') => {} // "\ No newline at end of file" marker
            None => {}       // blank line in the patch text
            Some(_) => bail!("unrecognized patch line: {raw:?}"),
        }
    }
    out.extend_from_slice(&old_lines[cursor..]);

    if out.is_empty() {
        Ok(String::new())
    } else {
        Ok(format!("{}\n", out.join("\n")))
    }
}

/// Parse the old-side start line from a `@@ -start,count +start,count @@` header.
fn parse_hunk_old_start(header: &str) -> Result<usize> {
    let after_minus = header
        .split('-')
        .nth(1)
        .context("malformed hunk header (no '-')")?;
    let digits: String = after_minus.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<usize>().context("malformed hunk start")
}

/// Ensure non-empty text ends with a newline (leaves empty text untouched).
fn ensure_trailing_newline(text: &str) -> std::borrow::Cow<'_, str> {
    if text.is_empty() || text.ends_with('\n') {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(format!("{text}\n"))
    }
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
    use super::{apply_patch, unified_diff};

    fn norm(s: &str) -> String {
        if s.is_empty() || s.ends_with('\n') {
            s.to_string()
        } else {
            format!("{s}\n")
        }
    }

    /// An unedited patch applies back to exactly the new content (normalized).
    fn assert_roundtrip(old: &str, new: &str) {
        let patch = unified_diff(old, new, "f");
        let applied = apply_patch(old, &patch).expect("apply");
        assert_eq!(applied, norm(new), "patch was:\n{patch}");
    }

    #[test]
    fn roundtrips() {
        assert_roundtrip("a\nb\nc\n", "a\nB\nc\n");
        assert_roundtrip("", "added\nlines\n");
        assert_roundtrip("removed\nlines\n", "");
        assert_roundtrip("a\nb\nc\n", "a\nb\nc\n");
        assert_roundtrip("x\ny", "x\nY"); // no trailing newline
        // Multiple hunks (changes separated by more than the context radius).
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = "1\nTWO\n3\n4\n5\n6\n7\n8\nNINE\n10\n";
        assert_roundtrip(old, new);
    }

    #[test]
    fn header_present_with_hunk_markers() {
        let patch = unified_diff("a\nb\n", "a\nB\n", "src/x.txt");
        assert!(patch.contains("--- a/src/x.txt"));
        assert!(patch.contains("+++ b/src/x.txt"));
        assert!(patch.contains("@@"));
    }

    #[test]
    fn editing_an_added_line_applies() {
        let patch = unified_diff("a\n", "a\nb\n", "f").replace("+b", "+B");
        assert_eq!(apply_patch("a\n", &patch).unwrap(), "a\nB\n");
    }

    #[test]
    fn inserting_an_extra_added_line_applies_without_fixing_counts() {
        // Add a brand new "+x" line; the stale @@ counts must not matter.
        let patch = unified_diff("a\n", "a\nb\n", "f").replace("+b\n", "+b\n+x\n");
        assert_eq!(apply_patch("a\n", &patch).unwrap(), "a\nb\nx\n");
    }

    #[test]
    fn flipping_a_removed_line_to_context_restores_it() {
        let patch = unified_diff("keep\ndrop\n", "keep\n", "f").replace("-drop", " drop");
        assert_eq!(apply_patch("keep\ndrop\n", &patch).unwrap(), "keep\ndrop\n");
    }

    #[test]
    fn context_mismatch_is_an_error() {
        // Corrupt a context line so it no longer matches the base.
        let patch = unified_diff("a\nb\nc\n", "a\nB\nc\n", "f").replace(" a\n", " WRONG\n");
        assert!(apply_patch("a\nb\nc\n", &patch).is_err());
    }
}
