//! Extract the per-file changes a commit introduces (vs. its parent), with text
//! content for the history/hunk view.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use similar::{ChangeTag, DiffOp, TextDiff};
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
/// meant to be edited and fed back to [`apply_patch`].
pub fn unified_diff(old: &str, new: &str, path: &str) -> String {
    let old = ensure_trailing_newline(old);
    let new = ensure_trailing_newline(new);
    let diff = TextDiff::from_lines(old.as_ref(), new.as_ref());
    let mut formatter = diff.unified_diff();
    formatter
        .context_radius(DEFAULT_CONTEXT)
        .missing_newline_hint(false)
        .header(&format!("a/{path}"), &format!("b/{path}"));
    formatter.to_string()
}

/// Default number of context lines shown around each change, matching the usual
/// `diff -u` radius.
pub const DEFAULT_CONTEXT: usize = 3;

/// How many context lines one "expand" step reveals on each side of a hunk.
pub const CONTEXT_STEP: usize = 3;

/// Extra context lines (beyond [`DEFAULT_CONTEXT`]) requested above (`before`)
/// and below (`after`) each *change group*, indexed by the group's order within
/// the file. A change group is a maximal run of added/removed lines; the gaps of
/// unchanged lines between them are what hunk context is drawn from. Missing
/// entries mean no extra context, so [`ContextExpansion::default`] reproduces a
/// standard `diff -u`.
#[derive(Debug, Clone, Default)]
pub struct ContextExpansion {
    pub before: Vec<usize>,
    pub after: Vec<usize>,
}

impl ContextExpansion {
    fn before_of(&self, group: usize) -> usize {
        self.before.get(group).copied().unwrap_or(0)
    }
    fn after_of(&self, group: usize) -> usize {
        self.after.get(group).copied().unwrap_or(0)
    }

    /// Widen the context above and below `group` by [`CONTEXT_STEP`] lines,
    /// growing the backing vectors as needed.
    pub fn expand(&mut self, first_group: usize, last_group: usize) {
        grow(&mut self.before, first_group);
        grow(&mut self.after, last_group);
        self.before[first_group] += CONTEXT_STEP;
        self.after[last_group] += CONTEXT_STEP;
    }
}

fn grow(v: &mut Vec<usize>, idx: usize) {
    if v.len() <= idx {
        v.resize(idx + 1, 0);
    }
}

/// A hunk in a [`RenderedDiff`], mapped back to the change groups it covers so
/// the UI can request more context for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkInfo {
    /// Zero-based line of this hunk's `@@` header within [`RenderedDiff::text`].
    pub header_line: usize,
    /// Inclusive range of change-group indices this hunk covers. Expanding the
    /// hunk widens context above `first_group` and below `last_group`.
    pub first_group: usize,
    pub last_group: usize,
    /// Whether hidden unchanged lines remain just above / below the hunk, i.e.
    /// whether expanding in that direction would reveal anything.
    pub can_expand_up: bool,
    pub can_expand_down: bool,
}

/// A unified diff rendered with per-hunk context, plus the hunk metadata needed
/// to drive context expansion from the UI.
#[derive(Debug, Clone, Default)]
pub struct RenderedDiff {
    pub text: String,
    pub hunks: Vec<HunkInfo>,
    /// Total number of change groups in the file (bounds valid group indices).
    pub group_count: usize,
}

/// Render a unified diff of `old` → `new` like [`unified_diff`], but with the
/// context radius of each hunk controlled by `exp`. With the default (empty)
/// expansion this matches `unified_diff`'s output. The returned [`RenderedDiff`]
/// maps each `@@` hunk back to its change groups so the caller can ask for more
/// surrounding context per hunk; the text always reverse-applies via
/// [`apply_patch`].
pub fn render_diff(old: &str, new: &str, path: &str, exp: &ContextExpansion) -> RenderedDiff {
    let old_n = ensure_trailing_newline(old);
    let new_n = ensure_trailing_newline(new);
    let old_lines: Vec<&str> = old_n.lines().collect();
    let new_lines: Vec<&str> = new_n.lines().collect();
    let diff = TextDiff::from_lines(old_n.as_ref(), new_n.as_ref());

    // Flatten the line ops into a single sequence of segments, tracking each
    // line's old/new index for the `@@` header.
    #[derive(Clone, Copy)]
    enum Tag {
        Ctx,
        Del,
        Ins,
    }
    struct Seg<'a> {
        tag: Tag,
        old: usize,
        new: usize,
        text: &'a str,
    }
    let mut segs: Vec<Seg> = Vec::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for k in 0..len {
                    segs.push(Seg {
                        tag: Tag::Ctx,
                        old: old_index + k,
                        new: new_index + k,
                        text: old_lines[old_index + k],
                    });
                }
            }
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => {
                for k in 0..old_len {
                    segs.push(Seg {
                        tag: Tag::Del,
                        old: old_index + k,
                        new: new_index,
                        text: old_lines[old_index + k],
                    });
                }
            }
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                for k in 0..new_len {
                    segs.push(Seg {
                        tag: Tag::Ins,
                        old: old_index,
                        new: new_index + k,
                        text: new_lines[new_index + k],
                    });
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                for k in 0..old_len {
                    segs.push(Seg {
                        tag: Tag::Del,
                        old: old_index + k,
                        new: new_index,
                        text: old_lines[old_index + k],
                    });
                }
                for k in 0..new_len {
                    segs.push(Seg {
                        tag: Tag::Ins,
                        old: old_index,
                        new: new_index + k,
                        text: new_lines[new_index + k],
                    });
                }
            }
        }
    }

    // Maximal runs of changed segments — the "change groups".
    let is_change = |s: &Seg| !matches!(s.tag, Tag::Ctx);
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < segs.len() {
        if is_change(&segs[i]) {
            let start = i;
            while i < segs.len() && is_change(&segs[i]) {
                i += 1;
            }
            groups.push((start, i));
        } else {
            i += 1;
        }
    }
    let group_count = groups.len();
    if group_count == 0 {
        return RenderedDiff::default();
    }

    let cb = |g: usize| DEFAULT_CONTEXT + exp.before_of(g);
    let ca = |g: usize| DEFAULT_CONTEXT + exp.after_of(g);

    // Join consecutive groups into hunks while the gap between them is fully
    // covered by the two groups' context (then there is no point in a split).
    let mut hunk_groups: Vec<(usize, usize)> = Vec::new();
    let mut a = 0;
    while a < group_count {
        let mut b = a;
        while b + 1 < group_count {
            let gap = groups[b + 1].0 - groups[b].1;
            if ca(b) + cb(b + 1) >= gap {
                b += 1;
            } else {
                break;
            }
        }
        hunk_groups.push((a, b));
        a = b + 1;
    }

    // Each hunk is the contiguous segment slice [top_start, bottom_end): the
    // groups it spans, their (fully shown) interior gaps, plus clamped context.
    let slices: Vec<(usize, usize, usize, usize)> = hunk_groups
        .iter()
        .map(|&(a, b)| {
            let top_avail = if a == 0 { groups[0].0 } else { groups[a].0 - groups[a - 1].1 };
            let bot_avail = if b + 1 == group_count {
                segs.len() - groups[b].1
            } else {
                groups[b + 1].0 - groups[b].1
            };
            let top_start = groups[a].0 - cb(a).min(top_avail);
            let bottom_end = groups[b].1 + ca(b).min(bot_avail);
            (top_start, bottom_end, a, b)
        })
        .collect();

    let mut lines: Vec<String> = vec![format!("--- a/{path}"), format!("+++ b/{path}")];
    let mut hunks = Vec::with_capacity(slices.len());
    for (idx, &(ts, be, a, b)) in slices.iter().enumerate() {
        let first = &segs[ts];
        let (mut old_count, mut new_count) = (0usize, 0usize);
        for s in &segs[ts..be] {
            match s.tag {
                Tag::Ctx => {
                    old_count += 1;
                    new_count += 1;
                }
                Tag::Del => old_count += 1,
                Tag::Ins => new_count += 1,
            }
        }
        let header_line = lines.len();
        lines.push(format!(
            "@@ -{},{} +{},{} @@",
            first.old + 1,
            old_count,
            first.new + 1,
            new_count
        ));
        for s in &segs[ts..be] {
            let prefix = match s.tag {
                Tag::Ctx => ' ',
                Tag::Del => '-',
                Tag::Ins => '+',
            };
            lines.push(format!("{prefix}{}", s.text));
        }
        // Separate hunks always have hidden lines between them (otherwise they
        // would have merged), so only the outermost edges can hit a file bound.
        hunks.push(HunkInfo {
            header_line,
            first_group: a,
            last_group: b,
            can_expand_up: if idx == 0 { ts > 0 } else { true },
            can_expand_down: if idx + 1 == slices.len() {
                be < segs.len()
            } else {
                true
            },
        });
    }

    RenderedDiff {
        text: format!("{}\n", lines.join("\n")),
        hunks,
        group_count,
    }
}

/// The role of a single line within a unified diff, for display/highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    /// A ` ` context line, unchanged.
    Context,
    /// A `+` line, added by the new side.
    Added,
    /// A `-` line, removed from the old side.
    Removed,
    /// An `@@ … @@` hunk header.
    Hunk,
    /// A `--- a/…` / `+++ b/…` file header.
    Header,
    /// Any other structural line (`\ No newline…`, `diff`/`index`).
    Meta,
}

/// A classified line of a unified diff, with intra-line word changes.
#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    /// Byte ranges *within the code portion* (after the leading prefix char)
    /// that actually differ from the paired opposite line. Empty unless this is
    /// one of an aligned removed/added line pair. Useful for intra-line
    /// emphasis (the specific characters/tokens that changed).
    pub intra: Vec<(usize, usize)>,
}

/// Classify each `\n`-separated line of a unified diff and, for aligned
/// removed/added line pairs, compute the intra-line (character-level) ranges
/// that differ. The result is aligned 1:1 with `diff.split('\n')`, so a text view can
/// tag its lines by index.
pub fn parse_diff_lines(diff: &str) -> Vec<DiffLine> {
    let lines: Vec<&str> = diff.split('\n').collect();
    let mut out: Vec<DiffLine> = lines
        .iter()
        .map(|l| DiffLine {
            kind: classify_line(l),
            intra: Vec::new(),
        })
        .collect();

    // Pair each maximal run of removed lines with the following run of added
    // lines and word-diff the aligned members (i-th removed ↔ i-th added).
    let mut i = 0;
    while i < out.len() {
        if out[i].kind != DiffLineKind::Removed {
            i += 1;
            continue;
        }
        let rem_start = i;
        while i < out.len() && out[i].kind == DiffLineKind::Removed {
            i += 1;
        }
        let add_start = i;
        while i < out.len() && out[i].kind == DiffLineKind::Added {
            i += 1;
        }
        let pairs = (add_start - rem_start).min(i - add_start);
        for k in 0..pairs {
            let old_code = &lines[rem_start + k][1..];
            let new_code = &lines[add_start + k][1..];
            let (old_ranges, new_ranges) = intra_change_ranges(old_code, new_code);
            out[rem_start + k].intra = old_ranges;
            out[add_start + k].intra = new_ranges;
        }
    }
    out
}

/// Classify a single unified-diff line by its leading marker. Heuristic: assumes
/// the well-formed output of [`unified_diff`] (one file-header pair at the top).
pub(crate) fn classify_line(line: &str) -> DiffLineKind {
    if line.starts_with("@@") {
        DiffLineKind::Hunk
    } else if line.starts_with("--- ") || line.starts_with("+++ ") {
        DiffLineKind::Header
    } else if line.starts_with('\\') || line.starts_with("diff ") || line.starts_with("index ") {
        DiffLineKind::Meta
    } else if line.starts_with('+') {
        DiffLineKind::Added
    } else if line.starts_with('-') {
        DiffLineKind::Removed
    } else {
        DiffLineKind::Context
    }
}

/// Maximum fraction of a line that intra-line emphasis may cover before it is
/// dropped entirely: beyond this the change is a near-rewrite and per-token
/// highlighting is just noise — the line background already conveys it.
const MAX_INTRA_COVERAGE: f32 = 0.66;

/// A list of `[start, end)` byte ranges within a line of code.
type Ranges = Vec<(usize, usize)>;

/// Compute the changed byte ranges on each side (deletions in `old`, insertions
/// in `new`) for intra-line emphasis. A character-level diff *locates* the
/// minimal edits, then each range is **snapped out to whole-word boundaries** so
/// a sub-word change highlights the entire token instead of speckling individual
/// characters — the way `git --word-diff`, delta and GitHub present it. If the
/// result would blanket most of the line, emphasis is dropped so only the line
/// background remains.
fn intra_change_ranges(old: &str, new: &str) -> (Ranges, Ranges) {
    let diff = TextDiff::from_chars(old, new);
    let (mut old_raw, mut new_raw) = (Vec::new(), Vec::new());
    let (mut old_off, mut new_off) = (0usize, 0usize);
    for change in diff.iter_all_changes() {
        let len = change.value().len();
        match change.tag() {
            ChangeTag::Equal => {
                old_off += len;
                new_off += len;
            }
            ChangeTag::Delete => {
                old_raw.push((old_off, old_off + len));
                old_off += len;
            }
            ChangeTag::Insert => {
                new_raw.push((new_off, new_off + len));
                new_off += len;
            }
        }
    }
    let old_ranges = merge_ranges(snap_to_words(old_raw, old));
    let new_ranges = merge_ranges(snap_to_words(new_raw, new));

    // Drop emphasis on both sides if it would blanket most of either line.
    if too_noisy(&old_ranges, old) || too_noisy(&new_ranges, new) {
        return (Vec::new(), Vec::new());
    }
    (old_ranges, new_ranges)
}

/// Grow each range to cover any word (`\w`-run) it overlaps, so a sub-word edit
/// highlights the whole token rather than scattered characters.
fn snap_to_words(ranges: Ranges, text: &str) -> Ranges {
    let words = word_spans(text);
    ranges
        .into_iter()
        .map(|(a, b)| {
            let (mut na, mut nb) = (a, b);
            for &(ws, we) in &words {
                if ws < b && we > a {
                    na = na.min(ws);
                    nb = nb.max(we);
                }
            }
            (na, nb)
        })
        .collect()
}

/// Byte ranges of maximal word-character (alphanumeric or `_`) runs in `text`.
fn word_spans(text: &str) -> Ranges {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        let is_word = c.is_alphanumeric() || c == '_';
        match (is_word, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                spans.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        spans.push((s, text.len()));
    }
    spans
}

/// Coalesce overlapping or touching ranges into disjoint, ordered spans.
fn merge_ranges(mut ranges: Ranges) -> Ranges {
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// True if the emphasized ranges cover more than [`MAX_INTRA_COVERAGE`] of the
/// (non-empty) line.
fn too_noisy(ranges: &[(usize, usize)], text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let covered: usize = ranges.iter().map(|&(s, e)| e - s).sum();
    covered as f32 > text.len() as f32 * MAX_INTRA_COVERAGE
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
    use super::{
        apply_patch, parse_diff_lines, render_diff, unified_diff, ContextExpansion, DiffLineKind,
    };

    /// Build a 20-line file and a copy with two far-apart single-line edits.
    fn two_change_file() -> (String, String) {
        let old: String = (1..=20).map(|n| format!("l{n}\n")).collect();
        let new: String = (1..=20)
            .map(|n| match n {
                3 => "L3\n".to_string(),
                15 => "L15\n".to_string(),
                _ => format!("l{n}\n"),
            })
            .collect();
        (old, new)
    }

    #[test]
    fn render_diff_default_matches_unified_diff() {
        let (old, new) = two_change_file();
        let rendered = render_diff(&old, &new, "f", &ContextExpansion::default());
        assert_eq!(rendered.text, unified_diff(&old, &new, "f"));
    }

    #[test]
    fn render_diff_splits_then_merges_on_expansion() {
        let (old, new) = two_change_file();
        let mut exp = ContextExpansion::default();
        let rendered = render_diff(&old, &new, "f", &exp);
        assert_eq!(rendered.group_count, 2);
        assert_eq!(rendered.hunks.len(), 2, "two distant edits start as two hunks");
        // The first edit (line 3) sits two lines from the top, so there is
        // nothing more to reveal above it; below, the second hunk follows.
        assert!(!rendered.hunks[0].can_expand_up);
        assert!(rendered.hunks[0].can_expand_down);

        // Widen the gap-facing side of each hunk until the gap is covered: the
        // two hunks then render as one.
        exp.expand(0, 0); // after group 0 +3 -> 6
        exp.expand(1, 1); // before group 1 +3 -> 6 ; 6+6 >= 11 gap
        let merged = render_diff(&old, &new, "f", &exp);
        assert_eq!(merged.hunks.len(), 1, "expanded context merges the hunks");
        assert_eq!(apply_patch(&old, &merged.text).unwrap(), new);
    }

    #[test]
    fn render_diff_expansion_reveals_more_context_and_still_applies() {
        let (old, new) = two_change_file();
        let base = render_diff(&old, &new, "f", &ContextExpansion::default());
        let base_lines = base.text.lines().count();
        let mut exp = ContextExpansion::default();
        exp.expand(0, 0);
        let wider = render_diff(&old, &new, "f", &exp);
        assert!(
            wider.text.lines().count() > base_lines,
            "expanding reveals additional context lines"
        );
        assert_eq!(apply_patch(&old, &wider.text).unwrap(), new);
    }

    #[test]
    fn render_diff_empty_when_unchanged() {
        let rendered = render_diff("a\nb\n", "a\nb\n", "f", &ContextExpansion::default());
        assert_eq!(rendered.group_count, 0);
        assert!(rendered.text.is_empty());
        assert!(rendered.hunks.is_empty());
    }

    /// Concatenate the substrings a line's intra ranges select from its code.
    fn marked(code: &str, ranges: &[(usize, usize)]) -> String {
        ranges.iter().map(|&(s, e)| &code[s..e]).collect()
    }

    #[test]
    fn classifies_diff_line_kinds() {
        let patch = unified_diff("a\nb\n", "a\nB\n", "f");
        let lines = parse_diff_lines(&patch);
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Header));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Hunk));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Context));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Removed));
        assert!(lines.iter().any(|l| l.kind == DiffLineKind::Added));
    }

    #[test]
    fn word_diff_marks_only_the_changed_token() {
        let old = "let r = f(a, b);";
        let new = "let r = f(a, c);";
        let lines = parse_diff_lines(&unified_diff(&format!("{old}\n"), &format!("{new}\n"), "f"));
        let removed = lines.iter().find(|l| l.kind == DiffLineKind::Removed).unwrap();
        let added = lines.iter().find(|l| l.kind == DiffLineKind::Added).unwrap();
        assert_eq!(marked(old, &removed.intra), "b");
        assert_eq!(marked(new, &added.intra), "c");
    }

    #[test]
    fn word_diff_pairs_aligned_lines_within_a_hunk() {
        let old = "a = alpha;\nb = beta;\n";
        let new = "a = alphaX;\nb = betaY;\n";
        let lines = parse_diff_lines(&unified_diff(old, new, "f"));
        let removed: Vec<_> = lines.iter().filter(|l| l.kind == DiffLineKind::Removed).collect();
        let added: Vec<_> = lines.iter().filter(|l| l.kind == DiffLineKind::Added).collect();
        assert_eq!(removed.len(), 2);
        assert_eq!(added.len(), 2);
        // Each added line marks the whole changed token, not just the new char.
        assert_eq!(marked("a = alphaX;", &added[0].intra), "alphaX");
        assert_eq!(marked("b = betaY;", &added[1].intra), "betaY");
    }

    #[test]
    fn intra_diff_snaps_subword_change_to_whole_token() {
        // A single changed character inside a token highlights the whole token
        // as one contiguous span — no per-character speckle.
        let lines = parse_diff_lines(&unified_diff("value = foobar;\n", "value = fooBar;\n", "f"));
        let added = lines.iter().find(|l| l.kind == DiffLineKind::Added).unwrap();
        assert_eq!(added.intra.len(), 1);
        assert_eq!(marked("value = fooBar;", &added.intra), "fooBar");
    }

    #[test]
    fn intra_diff_dropped_when_line_mostly_rewritten() {
        // A near-total rewrite would blanket the line, so emphasis is dropped and
        // only the line background remains.
        let lines = parse_diff_lines(&unified_diff("abcdefgh\n", "12345678\n", "f"));
        let added = lines.iter().find(|l| l.kind == DiffLineKind::Added).unwrap();
        let removed = lines.iter().find(|l| l.kind == DiffLineKind::Removed).unwrap();
        assert!(added.intra.is_empty());
        assert!(removed.intra.is_empty());
    }

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
