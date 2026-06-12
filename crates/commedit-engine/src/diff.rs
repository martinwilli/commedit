//! Extract the per-file changes a commit introduces (vs. its parent), with text
//! content for the history/hunk view.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures::io::AsyncReadExt;
use futures::StreamExt;
use jj_lib::backend::{CommitId, TreeValue};
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::repo_path::RepoPath;
use jj_lib::store::Store;
use similar::{ChangeTag, DiffOp, TextDiff};

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
    /// True if the `old` (parent) side is a *conflicted* tree value: for a merge
    /// commit the parents disagree at this path and don't auto-resolve, so the
    /// auto-merged `parent_tree()` has no single resolved base to reverse an
    /// edited patch against. Shown read-only, the same as [`Self::is_binary`].
    pub conflicted_base: bool,
}

/// List the file changes a commit introduces relative to its parent tree.
pub fn commit_changes(repo: &ReadonlyRepo, commit_id: &CommitId) -> Result<Vec<FileChange>> {
    let store = repo.store().clone();
    let commit = store.get_commit(commit_id).context("loading commit")?;
    let new_tree = commit.tree();
    let parent_tree = pollster::block_on(commit.parent_tree(repo)).context("parent tree")?;
    tree_changes(&store, &parent_tree, &new_tree)
}

/// List the file changes between two arbitrary trees (`old` → `new`), as the
/// content delta with text on both sides. The core of [`commit_changes`], also
/// used to diff non-parent-related trees (e.g. the session "Review" comparing
/// the current tree against the one the session started with).
pub fn tree_changes(
    store: &Arc<Store>,
    old_tree: &MergedTree,
    new_tree: &MergedTree,
) -> Result<Vec<FileChange>> {
    let entries = pollster::block_on(
        old_tree
            .diff_stream(new_tree, &EverythingMatcher)
            .collect::<Vec<_>>(),
    );

    let mut changes = Vec::new();
    for entry in entries {
        let diff = entry.values.context("computing file diff")?;
        // Capture before the `into_resolved()` move below collapses a conflicted
        // base to `None`: a conflicted parent tree (merge parents disagree) has no
        // single old side, so the file can't be edited as a reversible patch.
        let before_conflicted = !diff.before.is_resolved();
        let before = diff.before.into_resolved().ok().flatten();
        let after = diff.after.into_resolved().ok().flatten();

        let (old_text, old_binary) = read_text(store, &entry.path, before.as_ref())?;
        let (new_text, new_binary) = read_text(store, &entry.path, after.as_ref())?;
        // A conflicted base resolves to `None`, but the file does exist on the
        // (disagreeing) parents — so treat it as present for classification, so a
        // path the merge keeps reads as Modified rather than spuriously Added.
        let kind = match (before.is_some() || before_conflicted, after.is_some()) {
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
            conflicted_base: before_conflicted,
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

    /// Widen the context revealed *into an elided gap* by [`CONTEXT_STEP`]: more
    /// trailing context below the block `above` the gap, more leading context
    /// above the block `below` it. Either end may be absent (the gap is at the
    /// file's start/end). Drives the conflict-snippet "expand" cue, whose gaps sit
    /// between conflict blocks rather than on a hunk header.
    pub fn expand_gap(&mut self, above: Option<usize>, below: Option<usize>) {
        if let Some(g) = above {
            grow(&mut self.after, g);
            self.after[g] += CONTEXT_STEP;
        }
        if let Some(g) = below {
            grow(&mut self.before, g);
            self.before[g] += CONTEXT_STEP;
        }
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

/// One line of a flattened line-level diff: whether it is context / removed /
/// added, its old/new line index (for `@@` headers), and its text.
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

/// Flatten a line-level diff's ops into one sequence of segments, tracking each
/// line's old/new index and its text. Shared by [`render_diff`] and
/// [`revert_groups`] so both see identical segmentation and change grouping.
fn diff_segments<'a>(old_lines: &[&'a str], new_lines: &[&'a str], ops: &[DiffOp]) -> Vec<Seg<'a>> {
    let mut segs: Vec<Seg> = Vec::new();
    for op in ops {
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
    segs
}

/// Maximal runs of changed (non-context) segments — the "change groups". Each is
/// a half-open `[start, end)` range into `segs`, ordered and non-overlapping.
fn change_groups(segs: &[Seg]) -> Vec<(usize, usize)> {
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
    groups
}

/// Reconstruct a file from `old` → `new`'s diff, deciding per change group which
/// side to emit: `keep_new(g)` true emits the group's new side (the `+` lines,
/// dropping the `-`), false emits its old side (the `-` lines, dropping the `+`);
/// context is emitted verbatim. Group indexing matches [`render_diff`] /
/// [`HunkInfo`]. The result is newline-normalized like [`apply_patch`]'s output,
/// so the rendered diff of `old` → result still reverse-applies. Shared by
/// [`revert_groups`] and [`select_groups`].
fn reconstruct_groups(old: &str, new: &str, keep_new: impl Fn(usize) -> bool) -> String {
    let old_n = ensure_trailing_newline(old);
    let new_n = ensure_trailing_newline(new);
    let old_lines: Vec<&str> = old_n.lines().collect();
    let new_lines: Vec<&str> = new_n.lines().collect();
    let diff = TextDiff::from_lines(old_n.as_ref(), new_n.as_ref());

    let segs = diff_segments(&old_lines, &new_lines, diff.ops());
    let groups = change_groups(&segs);

    // Walk the segments, tracking which change group each changed segment belongs
    // to, and emit the side `keep_new` selects for that group.
    let mut out: Vec<&str> = Vec::with_capacity(segs.len());
    let mut g = 0usize;
    let mut i = 0usize;
    while i < segs.len() {
        if matches!(segs[i].tag, Tag::Ctx) {
            out.push(segs[i].text);
            i += 1;
            continue;
        }
        while g < groups.len() && i >= groups[g].1 {
            g += 1;
        }
        let keep = g < groups.len() && keep_new(g);
        match (segs[i].tag, keep) {
            (Tag::Ins, true) => out.push(segs[i].text),
            (Tag::Del, false) => out.push(segs[i].text),
            _ => {}
        }
        i += 1;
    }

    if out.is_empty() {
        String::new()
    } else {
        format!("{}\n", out.join("\n"))
    }
}

/// Reconstruct `new` with the change groups in the inclusive range
/// `[first_group, last_group]` reverted to `old`, leaving the other groups'
/// changes intact. Group indexing matches [`render_diff`] / [`HunkInfo`], so a
/// UI hunk's `first_group`/`last_group` selects exactly that hunk's content to
/// drop. The result is newline-normalized like [`apply_patch`]'s output, so the
/// rendered diff of `old` → result still reverse-applies. Out-of-range indices
/// (or a file with no changes) return `new` unchanged (normalized).
pub fn revert_groups(old: &str, new: &str, first_group: usize, last_group: usize) -> String {
    // A group is *kept* (new side) unless it falls in the reverted range.
    reconstruct_groups(old, new, |g| g < first_group || g > last_group)
}

/// Reconstruct a file that keeps only the change groups whose indices are in
/// `kept` and reverts every other group to `old` — the dual of [`revert_groups`],
/// generalized from one contiguous range to an arbitrary set. The partial
/// working-copy commit uses it to materialize the content of a selected subset of
/// a file's hunks (each hunk's `first_group..=last_group` is added to `kept`). An
/// empty `kept` reverts everything (yields `old`, normalized).
pub fn select_groups(old: &str, new: &str, kept: &BTreeSet<usize>) -> String {
    reconstruct_groups(old, new, |g| kept.contains(&g))
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

    // Flatten the line ops into segments and find the change groups (both shared
    // with `revert_groups` so they agree on grouping).
    let segs = diff_segments(&old_lines, &new_lines, diff.ops());
    let groups = change_groups(&segs);
    let group_count = groups.len();
    if group_count == 0 {
        return RenderedDiff::default();
    }

    let windows = window_groups(segs.len(), &groups, exp);

    let mut lines: Vec<String> = vec![format!("--- a/{path}"), format!("+++ b/{path}")];
    let mut hunks = Vec::with_capacity(windows.len());
    for w in &windows {
        let first = &segs[w.top];
        let (mut old_count, mut new_count) = (0usize, 0usize);
        for s in &segs[w.top..w.bottom] {
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
        for s in &segs[w.top..w.bottom] {
            let prefix = match s.tag {
                Tag::Ctx => ' ',
                Tag::Del => '-',
                Tag::Ins => '+',
            };
            lines.push(format!("{prefix}{}", s.text));
        }
        hunks.push(HunkInfo {
            header_line,
            first_group: w.first_group,
            last_group: w.last_group,
            can_expand_up: w.can_expand_up,
            can_expand_down: w.can_expand_down,
        });
    }

    RenderedDiff {
        text: format!("{}\n", lines.join("\n")),
        hunks,
        group_count,
    }
}

/// A windowed hunk produced by [`window_groups`]: the half-open span
/// `[top, bottom)` of the underlying sequence it shows, the inclusive
/// change-group index range it covers, and whether hidden items remain just
/// above/below it (i.e. whether expanding in that direction reveals anything).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Window {
    pub top: usize,
    pub bottom: usize,
    pub first_group: usize,
    pub last_group: usize,
    pub can_expand_up: bool,
    pub can_expand_down: bool,
}

/// Window `total` items around `groups` (each a half-open `[start, end)` index
/// range into the same sequence, ordered and non-overlapping) with per-group
/// context controlled by `exp`. Consecutive groups merge into one window while
/// the gap between them is fully covered by their combined context (then a split
/// is pointless). Shared by the unified-diff renderer (items = diff segments) and
/// the conflict-snippet renderer (items = file lines, groups = conflict blocks).
pub(crate) fn window_groups(
    total: usize,
    groups: &[(usize, usize)],
    exp: &ContextExpansion,
) -> Vec<Window> {
    let group_count = groups.len();
    if group_count == 0 {
        return Vec::new();
    }
    let cb = |g: usize| DEFAULT_CONTEXT + exp.before_of(g);
    let ca = |g: usize| DEFAULT_CONTEXT + exp.after_of(g);

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

    let n = hunk_groups.len();
    hunk_groups
        .iter()
        .enumerate()
        .map(|(idx, &(a, b))| {
            let top_avail = if a == 0 {
                groups[0].0
            } else {
                groups[a].0 - groups[a - 1].1
            };
            let bot_avail = if b + 1 == group_count {
                total - groups[b].1
            } else {
                groups[b + 1].0 - groups[b].1
            };
            let top = groups[a].0 - cb(a).min(top_avail);
            let bottom = groups[b].1 + ca(b).min(bot_avail);
            // Separate windows always have hidden items between them (otherwise
            // they would have merged), so only the outermost edges can hit a bound.
            Window {
                top,
                bottom,
                first_group: a,
                last_group: b,
                can_expand_up: if idx == 0 { top > 0 } else { true },
                can_expand_down: if idx + 1 == n { bottom < total } else { true },
            }
        })
        .collect()
}

/// One file's placement within a combined commit diff (see [`render_commit_diff`]).
#[derive(Debug, Clone)]
pub struct CombinedFile {
    pub path: String,
    /// Line of this file's `diff --git` separator within the combined text.
    pub start_line: usize,
    /// False for removed/binary files, shown as a read-only notice.
    pub editable: bool,
    /// This file's hunks, with `header_line` mapped to the *combined* text;
    /// `first_group`/`last_group` stay file-relative (for context expansion).
    pub hunks: Vec<HunkInfo>,
}

/// All of a commit's file changes rendered into one editable unified-diff buffer,
/// each file separated by a `diff --git` line, plus per-file placement so the UI
/// can jump to / expand / save individual files within the one view.
#[derive(Debug, Clone, Default)]
pub struct CombinedDiff {
    pub text: String,
    pub files: Vec<CombinedFile>,
}

/// Render every change in `changes` (in order) into one combined unified-diff
/// buffer. Each file is introduced by a `diff --git a/PATH b/PATH` separator —
/// unambiguous because a diff content line always carries a leading prefix char,
/// so it never starts with bare `diff `. Editable files (text content present)
/// use [`render_diff`] with their per-path [`ContextExpansion`] from `expansions`;
/// removed/binary files (and ones with no textual change) get a read-only
/// `\`-prefixed notice. The result reverse-applies per file via
/// [`split_combined_patch`] + [`apply_patch`].
pub fn render_commit_diff(
    changes: &[FileChange],
    expansions: &HashMap<String, ContextExpansion>,
) -> CombinedDiff {
    let default_exp = ContextExpansion::default();
    let mut text = String::new();
    let mut files = Vec::new();
    // Line index in `text` of the next line to be appended.
    let mut line = 0usize;

    for change in changes {
        let start_line = line;
        text.push_str(&format!("diff --git a/{p} b/{p}\n", p = change.path));
        line += 1;

        let mut editable =
            change.new_text.is_some() && !change.is_binary && !change.conflicted_base;
        let mut hunks = Vec::new();
        let body = if editable {
            let new = change.new_text.as_deref().unwrap_or("");
            let old = change.old_text.as_deref().unwrap_or("");
            let exp = expansions.get(&change.path).unwrap_or(&default_exp);
            let rendered = render_diff(old, new, &change.path, exp);
            if rendered.text.is_empty() {
                // No textual change (e.g. mode-only) — show a notice instead.
                editable = false;
                format!(
                    "--- a/{p}\n+++ b/{p}\n\\ (no textual changes)\n",
                    p = change.path
                )
            } else {
                // Offset each hunk's `@@` header into the combined text. The
                // file's `--- a/` header sits at `line` (just past the separator),
                // which is render_diff's line 0.
                for h in &rendered.hunks {
                    hunks.push(HunkInfo {
                        header_line: line + h.header_line,
                        ..h.clone()
                    });
                }
                rendered.text
            }
        } else {
            let notice = if change.is_binary {
                "\\ Binary file (not editable)"
            } else if change.conflicted_base {
                "\\ Conflicted merge base (not editable)"
            } else {
                "\\ File removed by this commit"
            };
            format!("--- a/{p}\n+++ b/{p}\n{notice}\n", p = change.path)
        };
        line += body.matches('\n').count();
        text.push_str(&body);
        files.push(CombinedFile {
            path: change.path.clone(),
            start_line,
            editable,
            hunks,
        });
    }

    CombinedDiff { text, files }
}

/// Split a combined diff (as produced by [`render_commit_diff`], possibly edited)
/// into per-file `(path, patch)` chunks. A `diff --git ` line opens each section;
/// the path is read from its `--- a/PATH` header. Robust under edits because the
/// firewall keeps the `diff --git`/`--- `/`+++ ` lines read-only, and content
/// lines always carry a prefix char so they can't masquerade as those headers.
pub fn split_combined_patch(text: &str) -> Vec<(String, String)> {
    let mut sections: Vec<Vec<&str>> = Vec::new();
    for line in text.split('\n') {
        if line.starts_with("diff --git ") {
            sections.push(Vec::new());
        }
        if let Some(cur) = sections.last_mut() {
            cur.push(line);
        }
    }
    sections
        .into_iter()
        .filter_map(|lines| {
            let path = lines
                .iter()
                .find_map(|l| l.strip_prefix("--- a/"))
                .map(str::to_string)?;
            Some((path, lines.join("\n")))
        })
        .collect()
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

/// The role of a single line of a *conflicted* file materialized with Git-style
/// conflict markers, for highlighting the conflict-resolution pane. Separate from
/// [`DiffLineKind`] (which the unified-diff patch firewall depends on): a
/// conflicted file is whole-file content, not a patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictLineKind {
    /// Content outside any conflict region.
    Plain,
    /// The `<<<<<<<` line opening a conflict ("our" side).
    MarkerOurs,
    /// A line of the "our" side of a conflict.
    Ours,
    /// The `|||||||` line opening the base (only with diff3-style markers; we
    /// strip these for display, but classify defensively in case one survives).
    MarkerBase,
    /// A line of the common-ancestor base of a conflict.
    Base,
    /// The `=======` line separating the two sides.
    MarkerSep,
    /// A line of the "their" side of a conflict.
    Theirs,
    /// The `>>>>>>>` line closing a conflict.
    MarkerTheirs,
}

impl ConflictLineKind {
    /// Whether this is one of the four `<<<`/`|||`/`===`/`>>>` marker lines.
    pub fn is_marker(self) -> bool {
        matches!(
            self,
            ConflictLineKind::MarkerOurs
                | ConflictLineKind::MarkerBase
                | ConflictLineKind::MarkerSep
                | ConflictLineKind::MarkerTheirs
        )
    }
}

/// Whether `line` begins with at least seven repetitions of `ch` — a conflict
/// marker line (`<<<<<<<`, `|||||||`, `=======`, `>>>>>>>`). Seven is Git's and
/// jj's default marker length; longer markers (chosen when content collides) also
/// start with seven, so this stays correct.
fn is_conflict_marker(line: &str, ch: char) -> bool {
    line.chars().take_while(|&c| c == ch).count() >= 7
}

/// Classify each `\n`-separated line of a conflicted file by its position
/// relative to the conflict markers, aligned 1:1 with `text.split('\n')`, so the
/// pane can tag each line. A small state machine: `<<<<<<<` opens the "our" side,
/// `|||||||` the base, `=======` switches to "their" side, `>>>>>>>` closes back
/// to plain. Closing/separator markers are only recognized while inside a
/// conflict, so ordinary content (e.g. a `=======` rule) is left as `Plain`.
pub fn classify_conflict_lines(text: &str) -> Vec<ConflictLineKind> {
    use ConflictLineKind::*;
    let mut state = Plain;
    text.split('\n')
        .map(|line| {
            if state == Plain {
                if is_conflict_marker(line, '<') {
                    state = Ours;
                    return MarkerOurs;
                }
                return Plain;
            }
            if is_conflict_marker(line, '>') {
                state = Plain;
                return MarkerTheirs;
            }
            if is_conflict_marker(line, '=') {
                state = Theirs;
                return MarkerSep;
            }
            if is_conflict_marker(line, '|') {
                state = Base;
                return MarkerBase;
            }
            state
        })
        .collect()
}

/// A piece of a conflicted file in snippet view, for reconstructing the whole
/// file from the (edited) shown segments interleaved with the verbatim hidden
/// runs (see [`reconstruct_conflict_file`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictPiece {
    /// A run of `lines` consecutive lines shown (and editable) in the view.
    Shown { lines: usize },
    /// A run of lines hidden behind an elision cue, kept verbatim.
    Elided { lines: Vec<String> },
}

/// An elided gap in a [`ConflictSnippets`] view: the cue line standing in for the
/// hidden run, and the conflict blocks adjacent to it whose context expands when
/// the cue is clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictGap {
    /// Line of this gap's elision cue within [`ConflictSnippets::text`].
    pub cue_line: usize,
    /// Block just above the gap whose trailing context widens, if any.
    pub above: Option<usize>,
    /// Block just below the gap whose leading context widens, if any.
    pub below: Option<usize>,
}

/// A conflicted file rendered as snippets: only the conflict blocks plus
/// surrounding context, with the long unconflicted runs between them elided
/// behind an expandable cue line — the conflict-pane analogue of [`render_diff`].
#[derive(Debug, Clone, Default)]
pub struct ConflictSnippets {
    /// The snippet text: shown content lines, with a single elision-`cue` line
    /// standing in for each hidden run. Carries no file header.
    pub text: String,
    /// The elided gaps, one per cue line, for click-to-expand.
    pub gaps: Vec<ConflictGap>,
    /// Ordered pieces to reconstruct the full file (see [`reconstruct_conflict_file`]).
    pub pieces: Vec<ConflictPiece>,
    /// Number of conflict blocks (bounds valid block indices for expansion).
    pub block_count: usize,
}

/// Render a conflicted file (whole-file text materialized with Git 2-way markers)
/// as snippets: each `<<<<<<< … >>>>>>>` block is the "change group", shown in
/// full with surrounding context; the unconflicted runs between/around blocks are
/// elided behind a `cue` line (exactly as [`render_diff`] windows a diff). A block
/// is *never* elided, so inline marker-line resolution still sees every marker.
/// Reuses [`window_groups`] for identical windowing/merging behavior. `cue` is the
/// caller's elision-cue line text (echoed back to [`reconstruct_conflict_file`]).
pub fn render_conflict_snippets(
    full_text: &str,
    exp: &ContextExpansion,
    cue: &str,
) -> ConflictSnippets {
    let kinds = classify_conflict_lines(full_text);
    let lines: Vec<&str> = full_text.split('\n').collect();
    let total = lines.len();

    // Conflict blocks as half-open `[start, end)` line ranges (opener through its
    // closing marker, inclusive). An unterminated block runs to end of file.
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < kinds.len() {
        if kinds[i] == ConflictLineKind::MarkerOurs {
            let mut j = i + 1;
            while j < kinds.len() && kinds[j] != ConflictLineKind::MarkerTheirs {
                j += 1;
            }
            let end = (j + 1).min(kinds.len());
            blocks.push((i, end));
            i = end;
        } else {
            i += 1;
        }
    }

    let block_count = blocks.len();
    if block_count == 0 {
        // No conflict left (e.g. every block was resolved inline): there is
        // nothing to focus on, so show the whole file as content. Shown — not
        // elided — so it round-trips through `reconstruct_conflict_file` (an
        // Elided run with no cue line to anchor it could not be recovered).
        return ConflictSnippets {
            text: full_text.to_string(),
            gaps: Vec::new(),
            pieces: vec![ConflictPiece::Shown { lines: total }],
            block_count: 0,
        };
    }

    let windows = window_groups(total, &blocks, exp);
    let mut text_lines: Vec<String> = Vec::new();
    let mut pieces: Vec<ConflictPiece> = Vec::new();
    let mut gaps: Vec<ConflictGap> = Vec::new();
    let mut cursor = 0usize;
    for (wi, w) in windows.iter().enumerate() {
        // Hidden run between the previous window (or the file start) and this one.
        if w.top > cursor {
            pieces.push(ConflictPiece::Elided {
                lines: lines[cursor..w.top].iter().map(|s| s.to_string()).collect(),
            });
            let cue_line = text_lines.len();
            text_lines.push(cue.to_string());
            gaps.push(ConflictGap {
                cue_line,
                above: (wi > 0).then(|| windows[wi - 1].last_group),
                below: Some(w.first_group),
            });
        }
        // The shown window (its interior gaps, if any, are fully shown).
        for line in &lines[w.top..w.bottom] {
            text_lines.push(line.to_string());
        }
        pieces.push(ConflictPiece::Shown {
            lines: w.bottom - w.top,
        });
        cursor = w.bottom;
    }
    // Trailing hidden run after the last window.
    if cursor < total {
        pieces.push(ConflictPiece::Elided {
            lines: lines[cursor..total].iter().map(|s| s.to_string()).collect(),
        });
        let cue_line = text_lines.len();
        text_lines.push(cue.to_string());
        gaps.push(ConflictGap {
            cue_line,
            above: Some(windows[windows.len() - 1].last_group),
            below: None,
        });
    }

    ConflictSnippets {
        text: text_lines.join("\n"),
        gaps,
        pieces,
        block_count,
    }
}

/// Reconstruct a conflicted file's full text from `shown` (the lines of its
/// snippet section in the buffer, possibly edited) and the `pieces` recorded at
/// render time. Each line equal to `cue` is replaced by the next [`ConflictPiece::Elided`]
/// run's verbatim lines (in document order); every other line is shown content
/// kept as-is. The inverse of [`render_conflict_snippets`]: rendering then
/// reconstructing (without edits) yields the original text.
pub fn reconstruct_conflict_file(shown: &[&str], pieces: &[ConflictPiece], cue: &str) -> String {
    let mut elided = pieces.iter().filter_map(|p| match p {
        ConflictPiece::Elided { lines } => Some(lines),
        ConflictPiece::Shown { .. } => None,
    });
    let mut out: Vec<String> = Vec::new();
    for &line in shown {
        if line == cue {
            if let Some(run) = elided.next() {
                out.extend(run.iter().cloned());
            }
            // A cue with no matching run (shouldn't happen with the edit guard)
            // is simply dropped.
        } else {
            out.push(line.to_string());
        }
    }
    out.join("\n")
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
    let digits: String = after_minus
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
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
pub(crate) fn read_text(
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
        apply_patch, parse_diff_lines, reconstruct_conflict_file, render_commit_diff,
        render_conflict_snippets, render_diff, revert_groups, select_groups, split_combined_patch,
        unified_diff, ChangeKind, ContextExpansion, DiffLineKind, FileChange,
    };
    use std::collections::{BTreeSet, HashMap};

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
        assert_eq!(
            rendered.hunks.len(),
            2,
            "two distant edits start as two hunks"
        );
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

    #[test]
    fn revert_one_of_two_groups_keeps_the_other() {
        let (old, new) = two_change_file();
        // Two change groups: line 3 (l3->L3) and line 15 (l15->L15).
        let reverted = revert_groups(&old, &new, 0, 0);
        assert!(reverted.contains("\nl3\n"), "group 0 reverted to old");
        assert!(reverted.contains("\nL15\n"), "group 1 kept the new content");
        // Only one hunk remains, and it still reverse-applies from old.
        let rendered = render_diff(&old, &reverted, "f", &ContextExpansion::default());
        assert_eq!(rendered.hunks.len(), 1);
        assert_eq!(apply_patch(&old, &rendered.text).unwrap(), reverted);
    }

    #[test]
    fn revert_all_groups_equals_old() {
        let (old, new) = two_change_file();
        assert_eq!(revert_groups(&old, &new, 0, 1), old);
    }

    #[test]
    fn revert_noop_when_unchanged() {
        assert_eq!(revert_groups("a\nb\n", "a\nb\n", 0, 0), "a\nb\n");
    }

    #[test]
    fn revert_pure_insertion_drops_added_lines() {
        let old = "a\nc\n";
        let new = "a\nb\nc\n";
        assert_eq!(revert_groups(old, new, 0, 0), old);
    }

    #[test]
    fn revert_pure_deletion_restores_lines() {
        let old = "a\nb\nc\n";
        let new = "a\nc\n";
        assert_eq!(revert_groups(old, new, 0, 0), old);
    }

    #[test]
    fn revert_then_render_still_applies() {
        let (old, new) = two_change_file();
        let reverted = revert_groups(&old, &new, 0, 0);
        // The property collect_file_edits relies on: the buffer the UI builds from
        // the reverted baseline reverse-applies back to that same content.
        let rendered = render_diff(&old, &reverted, "f", &ContextExpansion::default());
        assert_eq!(apply_patch(&old, &rendered.text).unwrap(), reverted);
    }

    #[test]
    fn revert_normalizes_missing_trailing_newline() {
        // Input new lacks a trailing newline; the result is normalized to carry
        // one, matching apply_patch's output so the save-side newline handling is
        // consistent.
        let old = "a\nb";
        let new = "a\nB";
        assert_eq!(revert_groups(old, new, 0, 0), "a\nb\n");
    }

    #[test]
    fn revert_out_of_range_index_is_noop() {
        let (old, new) = two_change_file();
        assert_eq!(revert_groups(&old, &new, 9, 9), new);
    }

    #[test]
    fn select_one_of_two_groups_drops_the_other() {
        let (old, new) = two_change_file();
        // Keep only group 1 (line 15): it takes the new content, group 0 reverts.
        let kept: BTreeSet<usize> = [1].into_iter().collect();
        let selected = select_groups(&old, &new, &kept);
        assert!(selected.contains("\nl3\n"), "group 0 reverted to old");
        assert!(selected.contains("\nL15\n"), "group 1 kept the new content");
        // It still reverse-applies from old, just like revert_groups' output.
        let rendered = render_diff(&old, &selected, "f", &ContextExpansion::default());
        assert_eq!(rendered.hunks.len(), 1);
        assert_eq!(apply_patch(&old, &rendered.text).unwrap(), selected);
    }

    #[test]
    fn select_empty_set_reverts_everything() {
        let (old, new) = two_change_file();
        assert_eq!(select_groups(&old, &new, &BTreeSet::new()), old);
    }

    #[test]
    fn select_all_groups_equals_new() {
        let (old, new) = two_change_file();
        let kept: BTreeSet<usize> = [0, 1].into_iter().collect();
        assert_eq!(select_groups(&old, &new, &kept), new);
    }

    #[test]
    fn select_is_the_dual_of_revert() {
        // Selecting group g must equal reverting every group but g.
        let (old, new) = two_change_file();
        let kept: BTreeSet<usize> = [0].into_iter().collect();
        assert_eq!(
            select_groups(&old, &new, &kept),
            revert_groups(&old, &new, 1, 1)
        );
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
        let removed = lines
            .iter()
            .find(|l| l.kind == DiffLineKind::Removed)
            .unwrap();
        let added = lines
            .iter()
            .find(|l| l.kind == DiffLineKind::Added)
            .unwrap();
        assert_eq!(marked(old, &removed.intra), "b");
        assert_eq!(marked(new, &added.intra), "c");
    }

    #[test]
    fn word_diff_pairs_aligned_lines_within_a_hunk() {
        let old = "a = alpha;\nb = beta;\n";
        let new = "a = alphaX;\nb = betaY;\n";
        let lines = parse_diff_lines(&unified_diff(old, new, "f"));
        let removed: Vec<_> = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Removed)
            .collect();
        let added: Vec<_> = lines
            .iter()
            .filter(|l| l.kind == DiffLineKind::Added)
            .collect();
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
        let added = lines
            .iter()
            .find(|l| l.kind == DiffLineKind::Added)
            .unwrap();
        assert_eq!(added.intra.len(), 1);
        assert_eq!(marked("value = fooBar;", &added.intra), "fooBar");
    }

    #[test]
    fn intra_diff_dropped_when_line_mostly_rewritten() {
        // A near-total rewrite would blanket the line, so emphasis is dropped and
        // only the line background remains.
        let lines = parse_diff_lines(&unified_diff("abcdefgh\n", "12345678\n", "f"));
        let added = lines
            .iter()
            .find(|l| l.kind == DiffLineKind::Added)
            .unwrap();
        let removed = lines
            .iter()
            .find(|l| l.kind == DiffLineKind::Removed)
            .unwrap();
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

    fn modified(path: &str, old: &str, new: &str) -> FileChange {
        FileChange {
            path: path.to_string(),
            kind: ChangeKind::Modified,
            old_text: Some(old.to_string()),
            new_text: Some(new.to_string()),
            is_binary: false,
            conflicted_base: false,
        }
    }

    #[test]
    fn combined_diff_has_one_section_per_file_with_global_hunk_lines() {
        let changes = vec![
            modified("a.txt", "a1\na2\n", "a1\nA2\n"),
            modified("b.txt", "b1\nb2\n", "B1\nb2\n"),
        ];
        let combined = render_commit_diff(&changes, &HashMap::new());
        let lines: Vec<&str> = combined.text.split('\n').collect();

        // One `diff --git` separator per file, recorded as `start_line`.
        let seps: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("diff --git "))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(seps.len(), 2);
        assert_eq!(combined.files.len(), 2);
        assert_eq!(combined.files[0].start_line, seps[0]);
        assert_eq!(combined.files[1].start_line, seps[1]);

        // Every recorded hunk header line is, in the combined text, an `@@` line.
        for f in &combined.files {
            assert!(f.editable);
            for h in &f.hunks {
                assert!(lines[h.header_line].starts_with("@@"), "hunk not at @@");
            }
        }
    }

    #[test]
    fn combined_diff_splits_then_applies_per_file() {
        let changes = vec![
            modified("a.txt", "a1\na2\n", "a1\nA2\n"),
            modified("b.txt", "b1\nb2\n", "B1\nb2\n"),
        ];
        let combined = render_commit_diff(&changes, &HashMap::new());
        let chunks = split_combined_patch(&combined.text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].0, "a.txt");
        assert_eq!(chunks[1].0, "b.txt");
        assert_eq!(apply_patch("a1\na2\n", &chunks[0].1).unwrap(), "a1\nA2\n");
        assert_eq!(apply_patch("b1\nb2\n", &chunks[1].1).unwrap(), "B1\nb2\n");
    }

    #[test]
    fn reverting_a_hunk_then_rendering_saves_back_the_partially_reverted_content() {
        // Mirror the UI's revert flow end to end: set the render baseline's
        // new_text to the hunk-reverted content, then reproduce `collect_file_edits`
        // (render + split + apply). The content Save would write equals the
        // partially-reverted new — line 3 restored, line 15 kept — and differs from
        // the original, so it is detected as an edit.
        let (old, new) = two_change_file();
        let reverted = revert_groups(&old, &new, 0, 0);
        let changes = vec![modified("f", &old, &reverted)];
        let combined = render_commit_diff(&changes, &HashMap::new());
        let chunks = split_combined_patch(&combined.text);
        let saved = apply_patch(&old, &chunks[0].1).unwrap();
        assert_eq!(saved, reverted);
        assert_ne!(saved, new);
        assert!(saved.contains("\nl3\n") && saved.contains("\nL15\n"));
    }

    #[test]
    fn reverting_a_whole_file_renders_a_notice_that_saves_back_to_old() {
        // Full-file revert: the baseline's new_text is set to old. render_commit_diff
        // shows the `(no textual changes)` notice *with* `--- a/PATH` headers, so
        // split finds the path and apply returns old — which differs from the
        // commit's real new, so Save drops the file's changes.
        let old = "a1\na2\n";
        let new = "A1\nA2\n";
        let changes = vec![modified("a.txt", old, old)];
        let combined = render_commit_diff(&changes, &HashMap::new());
        assert!(combined.text.contains("(no textual changes)"));
        let chunks = split_combined_patch(&combined.text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].0, "a.txt");
        let saved = apply_patch(old, &chunks[0].1).unwrap();
        assert_eq!(saved, old);
        assert_ne!(saved, new);
    }

    #[test]
    fn editing_one_file_in_combined_diff_changes_only_that_file() {
        let changes = vec![
            modified("a.txt", "a1\na2\n", "a1\nA2\n"),
            modified("b.txt", "b1\nb2\n", "B1\nb2\n"),
        ];
        let combined = render_commit_diff(&changes, &HashMap::new());
        // Edit the second file's added line `+B1` -> `+B1X`.
        let edited = combined.text.replace("+B1\n", "+B1X\n");
        let chunks = split_combined_patch(&edited);
        assert_eq!(apply_patch("a1\na2\n", &chunks[0].1).unwrap(), "a1\nA2\n");
        assert_eq!(apply_patch("b1\nb2\n", &chunks[1].1).unwrap(), "B1X\nb2\n");
    }

    #[test]
    fn removed_file_renders_a_read_only_notice() {
        let changes = vec![FileChange {
            path: "gone.txt".to_string(),
            kind: ChangeKind::Removed,
            old_text: Some("x\n".to_string()),
            new_text: None,
            is_binary: false,
            conflicted_base: false,
        }];
        let combined = render_commit_diff(&changes, &HashMap::new());
        assert!(!combined.files[0].editable);
        assert!(combined.files[0].hunks.is_empty());
        assert!(combined.text.contains("File removed by this commit"));
        // Its split chunk has no @@, so apply is a no-op against the old content.
        let chunks = split_combined_patch(&combined.text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(apply_patch("x\n", &chunks[0].1).unwrap(), "x\n");
    }

    #[test]
    fn conflicted_merge_base_renders_a_read_only_notice() {
        // A merge whose parents disagree at a path leaves the auto-merged base
        // conflicted: there is no single old side to reverse a patch against, so
        // the file is shown read-only (like a binary file), never as an editable
        // diff that could not round-trip.
        let changes = vec![FileChange {
            path: "m.txt".to_string(),
            kind: ChangeKind::Modified,
            old_text: None,
            new_text: Some("resolved\n".to_string()),
            is_binary: false,
            conflicted_base: true,
        }];
        let combined = render_commit_diff(&changes, &HashMap::new());
        assert!(!combined.files[0].editable);
        assert!(combined.files[0].hunks.is_empty());
        assert!(combined
            .text
            .contains("Conflicted merge base (not editable)"));
    }

    /// A 14-line file with a conflict block buried in the middle.
    fn conflict_file() -> String {
        let mut s = String::new();
        for n in 1..=5 {
            s.push_str(&format!("ctx{n}\n"));
        }
        s.push_str("<<<<<<< ours\n");
        s.push_str("mine\n");
        s.push_str("=======\n");
        s.push_str("yours\n");
        s.push_str(">>>>>>> theirs\n");
        for n in 6..=10 {
            s.push_str(&format!("ctx{n}\n"));
        }
        s
    }

    #[test]
    fn conflict_snippets_elide_far_context_and_keep_the_block() {
        let full = conflict_file();
        let snip = render_conflict_snippets(&full, &ContextExpansion::default(), "<CUE>");
        assert_eq!(snip.block_count, 1);
        // The whole conflict block is shown.
        assert!(snip.text.contains("<<<<<<< ours"));
        assert!(snip.text.contains("mine"));
        assert!(snip.text.contains("yours"));
        assert!(snip.text.contains(">>>>>>> theirs"));
        // Distant context (ctx1/ctx10) is elided behind a cue on each side.
        assert!(!snip.text.contains("ctx1\n") || snip.text.matches("<CUE>").count() >= 1);
        assert_eq!(snip.text.matches("<CUE>").count(), snip.gaps.len());
        assert!(snip.gaps.iter().any(|g| g.below == Some(0)));
        assert!(snip.gaps.iter().any(|g| g.above == Some(0)));
    }

    #[test]
    fn conflict_snippets_reconstruct_to_the_original() {
        let full = conflict_file();
        let snip = render_conflict_snippets(&full, &ContextExpansion::default(), "<CUE>");
        let shown: Vec<&str> = snip.text.split('\n').collect();
        let rebuilt = reconstruct_conflict_file(&shown, &snip.pieces, "<CUE>");
        assert_eq!(rebuilt, full);
    }

    #[test]
    fn conflict_snippets_with_no_conflict_show_and_reconstruct_whole_file() {
        // A file with every conflict already resolved (no markers) shows in full
        // and round-trips — no content is hidden behind an unrecoverable elision.
        let full = "a\nb\nc\nd\n";
        let snip = render_conflict_snippets(full, &ContextExpansion::default(), "<CUE>");
        assert_eq!(snip.block_count, 0);
        assert!(!snip.text.contains("<CUE>"));
        assert!(snip.text.contains("a") && snip.text.contains("d"));
        let shown: Vec<&str> = snip.text.split('\n').collect();
        assert_eq!(
            reconstruct_conflict_file(&shown, &snip.pieces, "<CUE>"),
            full
        );
    }

    #[test]
    fn conflict_snippet_gap_expands_then_still_reconstructs() {
        let full = conflict_file();
        let mut exp = ContextExpansion::default();
        let base = render_conflict_snippets(&full, &exp, "<CUE>");
        // Widen the gap above the block (the leading elided run).
        let leading = base
            .gaps
            .iter()
            .find(|g| g.below == Some(0))
            .expect("leading gap");
        exp.expand_gap(leading.above, leading.below);
        let wider = render_conflict_snippets(&full, &exp, "<CUE>");
        assert!(
            wider.text.lines().count() > base.text.lines().count(),
            "expanding the gap reveals more context"
        );
        // It still reconstructs to the original.
        let shown: Vec<&str> = wider.text.split('\n').collect();
        assert_eq!(
            reconstruct_conflict_file(&shown, &wider.pieces, "<CUE>"),
            full
        );
    }

    #[test]
    fn conflict_snippets_reconstruct_after_editing_a_shown_line() {
        let full = conflict_file();
        let snip = render_conflict_snippets(&full, &ContextExpansion::default(), "<CUE>");
        // Resolve the block by hand: drop the markers, keep "mine".
        let resolved_text = snip
            .text
            .replace("<<<<<<< ours\n", "")
            .replace("=======\nyours\n>>>>>>> theirs\n", "");
        let shown: Vec<&str> = resolved_text.split('\n').collect();
        let rebuilt = reconstruct_conflict_file(&shown, &snip.pieces, "<CUE>");
        // The elided context is restored verbatim; the resolved block has no markers.
        assert!(rebuilt.contains("ctx1\n"));
        assert!(rebuilt.contains("ctx10\n"));
        assert!(rebuilt.contains("mine\n"));
        assert!(!rebuilt.contains("<<<<<<<"));
        assert!(!rebuilt.contains("yours"));
    }
}
