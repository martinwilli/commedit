//! Structured editing of a unified-diff buffer.
//!
//! The diff pane lets the user edit the patch text directly, but a patch only
//! [`apply`s](crate::diff::apply_patch) if its context (` `) and removed (`-`)
//! lines still match the original file. This module turns raw edit *gestures*
//! (typing, Enter, Backspace, Delete) into structurally-valid mutations so the
//! result always applies:
//!
//! * Only `+` (added) line content is freely editable.
//! * Typing into a context line splits it into a `-<orig>` / `+<edited>` pair.
//! * `-` lines are immutable per-character (piecemeal edits are rejected).
//!   Selecting whole `-` line(s) and deleting restores them to context —
//!   undoing the removal.
//! * Backspace/Delete on a context line edits it like typing: dropping a
//!   character splits it into a `-<orig>` / `+<edited>` pair. Backspace at the
//!   content start / Delete at the content end have no character to remove there,
//!   so they mark the whole line removed (` ` → `-`) — a clean toggle.
//! * Deleting a selection over context lines removes the whole ones (→ `-`) and
//!   edits any half-selected line at either end, the surviving text rejoining as
//!   a `+` line — like a plain-editor delete lifted into the diff.
//! * Enter on a context line keeps it and inserts an empty `+` line below.
//! * Header / `@@` / meta lines are read-only.
//!
//! The logic is pure and GTK-free: [`plan_edit`] maps `(text, selection,
//! gesture)` to an [`EditPlan`], and the UI layer applies the resulting
//! [`PatchEdit`] to its text buffer. Columns are *character* offsets within a
//! line, where column 0 is the leading prefix char — matching GTK's
//! `iter_at_line_offset`.

use crate::diff::{classify_line, DiffLineKind};

/// A decoded user edit gesture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditGesture {
    /// Insert literal text at the caret (a typed char, or a paste payload which
    /// may contain newlines).
    Insert(String),
    /// Press Enter.
    Newline,
    /// Press Backspace (delete backward).
    Backspace,
    /// Press Delete (delete forward).
    Delete,
}

/// A caret position: `line` indexes `text.split('\n')`; `col` is a character
/// offset within that line where col 0 is the prefix char and col 1 is the start
/// of the line's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub line: usize,
    pub col: usize,
}

impl Cursor {
    fn at(line: usize, col: usize) -> Self {
        Cursor { line, col }
    }
    fn key(&self) -> (usize, usize) {
        (self.line, self.col)
    }
}

/// A text selection. A collapsed selection (`anchor == end`) is a plain caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Cursor,
    pub end: Cursor,
}

impl Selection {
    /// A collapsed selection (plain caret) at `c`.
    pub fn caret(c: Cursor) -> Self {
        Selection { anchor: c, end: c }
    }
    fn is_empty(&self) -> bool {
        self.anchor == self.end
    }
    /// `(low, high)` in document order.
    fn ordered(&self) -> (Cursor, Cursor) {
        if self.anchor.key() <= self.end.key() {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }
}

/// A single coalesced buffer mutation: replace the half-open span
/// `[start, end)` with `replacement`, then place the caret at `cursor`
/// (in post-edit coordinates).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchEdit {
    pub start: Cursor,
    pub end: Cursor,
    pub replacement: String,
    pub cursor: Cursor,
}

/// What to do with a gesture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditPlan {
    /// Let the default editor action happen unchanged (e.g. typing in a `+`
    /// line, or deleting a char inside `+` content).
    Allow,
    /// Apply this exact mutation instead of the default.
    Edit(PatchEdit),
    /// Drop the gesture entirely — it would corrupt the patch.
    Block,
}

/// Decide how to handle `gesture` performed with `sel` on the unified diff
/// `text`. See the module docs for the rules.
pub fn plan_edit(text: &str, sel: Selection, gesture: EditGesture) -> EditPlan {
    let lines: Vec<&str> = text.split('\n').collect();
    match gesture {
        EditGesture::Insert(ins) => plan_insert(&lines, sel, &ins),
        EditGesture::Newline => plan_newline(&lines, sel),
        EditGesture::Backspace => plan_delete(&lines, sel, Dir::Back),
        EditGesture::Delete => plan_delete(&lines, sel, Dir::Forward),
    }
}

/// Whether a raw deletion of the ordered span `[start, end)` is safe to allow
/// unchanged — i.e. it lies wholly within one `+` line's content. Used by the UI
/// as a firewall to block deletions (cut, drag, selection-replace) that did not
/// come through [`plan_edit`] and could otherwise corrupt the patch.
pub fn deletion_is_safe(text: &str, start: Cursor, end: Cursor) -> bool {
    if start.key() > end.key() {
        return deletion_is_safe(text, end, start);
    }
    let lines: Vec<&str> = text.split('\n').collect();
    if start.line != end.line {
        return false;
    }
    let Some(line) = lines.get(start.line) else {
        return false;
    };
    if line.is_empty() || classify_line(line) != DiffLineKind::Added {
        return false;
    }
    // Never touch the prefix column.
    start.col >= 1 && end.col >= 1
}

/// Collapse no-op `-X`/`+X` pairs back into a single context line — the inverse
/// of the context split [`insert_into_context`] performs. When the user edits a
/// `+` line until it again equals the `-` line it replaced (i.e. undoes their
/// change), that line is no longer a real diff and should fold back to plain
/// context. Within each change block (a maximal run of `-` lines immediately
/// followed by `+` lines) the leading and trailing lines that match between the
/// removed and added side are converted to context; the genuinely-changed lines
/// in the middle stay as `-`/`+`.
///
/// Returns the collapsed text and the cursor remapped onto the equivalent line
/// (same column), so resuming an edit re-splits the line at the same spot — or
/// `None` if nothing collapses. The result still reverse-applies as a patch:
/// folding one `-`/`+` pair into a context line leaves the hunk's old/new line
/// counts unchanged (a context line counts on both sides).
pub fn collapse_diff(text: &str, cursor: Cursor) -> Option<(String, Cursor)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    // For each input line, the output line index it ends up on (collapsed pairs
    // map both the `-` and `+` line to the resulting context line).
    let mut map: Vec<usize> = vec![0; lines.len()];
    let mut changed = false;

    let mut i = 0;
    while i < lines.len() {
        if classify_line(lines[i]) == DiffLineKind::Removed {
            let rem_start = i;
            while i < lines.len() && classify_line(lines[i]) == DiffLineKind::Removed {
                i += 1;
            }
            let add_start = i;
            while i < lines.len() && classify_line(lines[i]) == DiffLineKind::Added {
                i += 1;
            }
            collapse_block(&lines, rem_start, add_start, i, &mut out, &mut map, &mut changed);
        } else {
            map[i] = out.len();
            out.push(lines[i].to_string());
            i += 1;
        }
    }

    if !changed {
        return None;
    }
    let line = map
        .get(cursor.line)
        .copied()
        .unwrap_or_else(|| out.len().saturating_sub(1));
    Some((out.join("\n"), Cursor { line, col: cursor.col }))
}

/// Fold the matching ends of one `-`-run / `+`-run change block into context.
/// `rem_start..add_start` are the removed lines, `add_start..add_end` the added.
fn collapse_block(
    lines: &[&str],
    rem_start: usize,
    add_start: usize,
    add_end: usize,
    out: &mut Vec<String>,
    map: &mut [usize],
    changed: &mut bool,
) {
    let removed: Vec<&str> = (rem_start..add_start).map(|j| content(lines[j])).collect();
    let added: Vec<&str> = (add_start..add_end).map(|j| content(lines[j])).collect();
    let (lr, la) = (removed.len(), added.len());

    // Leading lines equal on both sides, then trailing lines equal on what's left.
    let mut pre = 0;
    while pre < lr && pre < la && removed[pre] == added[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < lr - pre && suf < la - pre && removed[lr - 1 - suf] == added[la - 1 - suf] {
        suf += 1;
    }

    if pre == 0 && suf == 0 {
        // Nothing matches: emit the block verbatim.
        for j in rem_start..add_end {
            map[j] = out.len();
            out.push(lines[j].to_string());
        }
        return;
    }
    *changed = true;

    // Leading matches → context, then the changed middle (removed then added),
    // then trailing matches → context. This keeps each side's line order intact.
    for k in 0..pre {
        map[rem_start + k] = out.len();
        map[add_start + k] = out.len();
        out.push(format!(" {}", removed[k]));
    }
    for k in pre..lr - suf {
        map[rem_start + k] = out.len();
        out.push(format!("-{}", removed[k]));
    }
    for k in pre..la - suf {
        map[add_start + k] = out.len();
        out.push(format!("+{}", added[k]));
    }
    for k in 0..suf {
        map[rem_start + (lr - suf) + k] = out.len();
        map[add_start + (la - suf) + k] = out.len();
        out.push(format!(" {}", removed[lr - suf + k]));
    }
}

enum Dir {
    Back,
    Forward,
}

fn plan_insert(lines: &[&str], sel: Selection, ins: &str) -> EditPlan {
    let (lo, hi) = sel.ordered();
    // A selection spanning multiple lines can't be a structured insert.
    if lo.line != hi.line {
        return EditPlan::Block;
    }
    let l = lo.line;
    let Some(&line) = lines.get(l) else {
        return EditPlan::Block;
    };
    if line.is_empty() {
        return EditPlan::Block; // trailing virtual EOF line
    }
    match classify_line(line) {
        DiffLineKind::Added => insert_into_added(lines, l, lo.col, hi.col, ins),
        DiffLineKind::Context => EditPlan::Edit(insert_into_context(line, l, lo.col, hi.col, ins)),
        _ => EditPlan::Block,
    }
}

/// Typing into a `+` line: usually let the editor insert normally; only take
/// over to keep the caret inside the content (col 0) or to `+`-prefix the
/// continuation lines of a multi-line payload (so a pasted `\n` never produces a
/// bare/context line).
fn insert_into_added(lines: &[&str], l: usize, col_lo: usize, col_hi: usize, ins: &str) -> EditPlan {
    let line = lines[l];
    let single_line = !ins.contains('\n');
    let no_selection = col_lo == col_hi;
    if single_line && no_selection && col_lo >= 1 {
        return EditPlan::Allow;
    }
    let s = content_index(line, col_lo);
    let e = content_index(line, col_hi);
    let replacement = ins.replace('\n', "\n+");
    let segs: Vec<&str> = ins.split('\n').collect();
    let cursor = if segs.len() == 1 {
        Cursor::at(l, s + 1 + char_len(ins))
    } else {
        Cursor::at(l + segs.len() - 1, 1 + char_len(segs[segs.len() - 1]))
    };
    EditPlan::Edit(PatchEdit {
        start: Cursor::at(l, s + 1),
        end: Cursor::at(l, e + 1),
        replacement,
        cursor,
    })
}

/// Typing into a context line: split it into a removed copy of the original and
/// an added copy carrying the edit, so the original line still matches the base.
fn insert_into_context(line: &str, l: usize, col_lo: usize, col_hi: usize, ins: &str) -> PatchEdit {
    let body = content(line);
    let s = content_index(line, col_lo);
    let e = content_index(line, col_hi);
    let edited = splice(body, s, e, ins);
    let plus: String = edited
        .split('\n')
        .map(|seg| format!("+{seg}"))
        .collect::<Vec<_>>()
        .join("\n");
    let replacement = format!("-{body}\n{plus}");

    let segs: Vec<&str> = ins.split('\n').collect();
    let cursor = if segs.len() == 1 {
        Cursor::at(l + 1, 1 + s + char_len(ins))
    } else {
        Cursor::at(l + segs.len(), 1 + char_len(segs[segs.len() - 1]))
    };
    PatchEdit {
        start: Cursor::at(l, 0),
        end: Cursor::at(l, char_len(line)),
        replacement,
        cursor,
    }
}

fn plan_newline(lines: &[&str], sel: Selection) -> EditPlan {
    let (lo, hi) = sel.ordered();
    if lo.line != hi.line {
        return EditPlan::Block;
    }
    let l = lo.line;
    let Some(&line) = lines.get(l) else {
        return EditPlan::Block;
    };
    if line.is_empty() {
        return EditPlan::Block;
    }
    match classify_line(line) {
        // Keep the context line, insert an empty `+` line below it.
        DiffLineKind::Context => EditPlan::Edit(PatchEdit {
            start: Cursor::at(l, char_len(line)),
            end: Cursor::at(l, char_len(line)),
            replacement: "\n+".to_string(),
            cursor: Cursor::at(l + 1, 1),
        }),
        // Split the added line into two `+` lines at the caret/selection.
        DiffLineKind::Added => {
            let s = content_index(line, lo.col);
            let e = content_index(line, hi.col);
            EditPlan::Edit(PatchEdit {
                start: Cursor::at(l, s + 1),
                end: Cursor::at(l, e + 1),
                replacement: "\n+".to_string(),
                cursor: Cursor::at(l + 1, 1),
            })
        }
        _ => EditPlan::Block,
    }
}

fn plan_delete(lines: &[&str], sel: Selection, dir: Dir) -> EditPlan {
    if !sel.is_empty() {
        let (lo, hi) = sel.ordered();
        // Wholly inside one `+` line's content: the default delete is safe.
        if deletion_is_safe_lines(lines, lo, hi) {
            return EditPlan::Allow;
        }
        // A selection over context line(s): remove the whole ones and edit any
        // half-selected line at either end (the surviving text rejoins as `+`).
        if let Some(edit) = delete_context_span(lines, lo, hi) {
            return EditPlan::Edit(edit);
        }
        // A selection confined to `+` lines: remove whole lines, or join across
        // them for a partial multi-line delete inside the added block.
        if let Some(edit) = delete_added_span(lines, lo, hi) {
            return EditPlan::Edit(edit);
        }
        // A selection of whole `-` line(s) restores them to context.
        if let Some(edit) = restore_removed_span(lines, lo, hi) {
            return EditPlan::Edit(edit);
        }
        return EditPlan::Block;
    }
    let caret = sel.end;
    let l = caret.line;
    let Some(&line) = lines.get(l) else {
        return EditPlan::Block;
    };
    if line.is_empty() {
        return EditPlan::Block;
    }
    match classify_line(line) {
        // A `-` line is immutable per-character: a stray key un-removing it is
        // confusing. Restore one by selecting the whole line (see
        // `restore_removed_span`); reject piecemeal edits.
        DiffLineKind::Removed => EditPlan::Block,
        DiffLineKind::Context => delete_in_context(line, l, caret.col, dir),
        DiffLineKind::Added => delete_in_added(lines, l, caret.col, dir),
        _ => EditPlan::Block,
    }
}

/// Delete a character from a context line, splitting it into a `-orig`/`+edited`
/// pair the same way typing does so the original still matches the base.
/// Backspace at the content start / Delete at the content end have no character
/// to remove on this line, so they fall back to marking the whole line removed.
fn delete_in_context(line: &str, l: usize, col: usize, dir: Dir) -> EditPlan {
    let n = char_len(content(line));
    let ci = content_index(line, col);
    let (s, e) = match dir {
        Dir::Back if ci >= 1 => (ci - 1, ci),
        Dir::Forward if ci < n => (ci, ci + 1),
        _ => return toggle_prefix(l, '-', col),
    };
    EditPlan::Edit(delete_from_context(line, l, s, e))
}

/// Split a context line into a `-orig`/`+edited` pair with content chars `[s, e)`
/// removed, leaving the caret on the `+` line at the deletion point.
fn delete_from_context(line: &str, l: usize, s: usize, e: usize) -> PatchEdit {
    let body = content(line);
    let edited = splice(body, s, e, "");
    PatchEdit {
        start: Cursor::at(l, 0),
        end: Cursor::at(l, char_len(line)),
        replacement: format!("-{body}\n+{edited}"),
        cursor: Cursor::at(l + 1, 1 + s),
    }
}

/// Delete a selection confined to a run of `+` lines. `+` lines are purely
/// additive, so any deletion that stays within them keeps the patch valid. Two
/// shapes are accepted:
///
/// * **Mid-line start** (`lo.col >= 1`): keep the first line's `+` prefix and
///   head, join it with the tail of the last line, and drop the lines between —
///   an ordinary cross-line delete confined to the block, yielding one `+` line.
/// * **Boundary start** (`lo.col == 0`): remove whole `+` lines, but only if the
///   selection also *ends* on a boundary (the next line's start, or the last
///   line's content end). Otherwise the join would strip the last line's `+`
///   prefix and leave bare content, so it is refused.
///
/// Returns `None` when any consumed line isn't a (non-empty) added line, or the
/// shape would corrupt the patch — leaving the caller to block it.
fn delete_added_span(lines: &[&str], lo: Cursor, hi: Cursor) -> Option<PatchEdit> {
    // The last line is consumed only if the selection enters its content; an end
    // at column 0 sits on the boundary above it.
    let last = if hi.col == 0 {
        hi.line.checked_sub(1)?
    } else {
        hi.line
    };
    if last < lo.line || last >= lines.len() {
        return None;
    }
    if lines[lo.line..=last]
        .iter()
        .any(|l| l.is_empty() || classify_line(l) != DiffLineKind::Added)
    {
        return None;
    }
    if lo.col >= 1 {
        // Keep the first line's prefix and join with the last line's tail; a raw
        // delete of the span does exactly that and stays a single `+` line.
        return Some(PatchEdit {
            start: lo,
            end: hi,
            replacement: String::new(),
            cursor: lo,
        });
    }
    // Boundary start: only whole-line removal is safe. The end must also be a
    // boundary, else dropping the last line's prefix would leave bare content.
    let ends_on_boundary = hi.col == 0 || hi.col >= char_len(lines[hi.line]);
    if !ends_on_boundary {
        return None;
    }
    let end = if hi.col == 0 { hi.line } else { hi.line + 1 };
    Some(PatchEdit {
        start: Cursor::at(lo.line, 0),
        end: Cursor::at(end, 0),
        replacement: String::new(),
        cursor: Cursor::at(lo.line, 0),
    })
}

/// Delete a selection that spans context line(s). Every spanned context line is
/// marked removed (`-`); unless the selection sat exactly on line boundaries,
/// the surviving head of the first line and tail of the last line rejoin into a
/// single edited `+` line (mirroring a plain-editor delete that merges the
/// partial ends). Returns `None` unless every spanned line is a non-empty
/// context line, leaving the caller to handle other kinds.
fn delete_context_span(lines: &[&str], lo: Cursor, hi: Cursor) -> Option<PatchEdit> {
    // A selection sitting exactly on line boundaries removes whole lines; one
    // entering a line's content edits it, so the surviving text rejoins as `+`.
    let whole_lines = lo.col == 0 && hi.col == 0;
    let last = if whole_lines { hi.line.checked_sub(1)? } else { hi.line };
    if last < lo.line || last >= lines.len() {
        return None;
    }
    if lines[lo.line..=last]
        .iter()
        .any(|l| l.is_empty() || classify_line(l) != DiffLineKind::Context)
    {
        return None;
    }
    // Mark every spanned context line removed.
    let mut replacement = String::new();
    for &line in &lines[lo.line..=last] {
        replacement.push('-');
        replacement.push_str(content(line));
        replacement.push('\n');
    }
    // Unless whole lines were taken, the kept head/tail rejoin as one `+` line
    // (empty when only a line's content was cleared), with the caret at the seam.
    let cursor = if whole_lines {
        Cursor::at(lo.line, 0)
    } else {
        let first = lines[lo.line];
        let head = content_range(first, 0, content_index(first, lo.col));
        let tail_line = lines[hi.line];
        let tail = content_range(
            tail_line,
            content_index(tail_line, hi.col),
            char_len(content(tail_line)),
        );
        let head_len = char_len(&head);
        replacement.push('+');
        replacement.push_str(&head);
        replacement.push_str(&tail);
        replacement.push('\n');
        Cursor::at(hi.line + 1, 1 + head_len)
    };
    Some(PatchEdit {
        start: Cursor::at(lo.line, 0),
        end: Cursor::at(last + 1, 0),
        replacement,
        cursor,
    })
}

/// Restore the whole `-` line(s) a selection covers back to context — undoing
/// the removal so the lines stay in the file. The selection must cover the lines
/// whole (it may include the newline just before or after them), and every
/// covered line must be a non-empty removed line — so it stays confined to one
/// `-` block. Returns `None` for a partial or mixed selection, leaving the
/// caller to reject it. Flipping `-` to ` ` keeps the patch valid: the line
/// already matches the base, it just stops being dropped.
fn restore_removed_span(lines: &[&str], lo: Cursor, hi: Cursor) -> Option<PatchEdit> {
    // Resolve the selection ends to whole-line bounds. A start past a line's
    // content (or a `\n`) begins at the next line; an end at column 0 sits on the
    // boundary above hi.line.
    let first = if lo.col == 0 {
        lo.line
    } else if lo.col >= char_len(lines.get(lo.line).copied()?) {
        lo.line + 1
    } else {
        return None;
    };
    let last = if hi.col == 0 {
        hi.line.checked_sub(1)?
    } else if hi.col >= char_len(lines.get(hi.line).copied()?) {
        hi.line
    } else {
        return None;
    };
    if first > last || last >= lines.len() {
        return None;
    }
    if lines[first..=last]
        .iter()
        .any(|l| l.is_empty() || classify_line(l) != DiffLineKind::Removed)
    {
        return None;
    }
    let mut replacement = String::new();
    for &line in &lines[first..=last] {
        replacement.push(' ');
        replacement.push_str(content(line));
        replacement.push('\n');
    }
    Some(PatchEdit {
        start: Cursor::at(first, 0),
        end: Cursor::at(last + 1, 0),
        replacement,
        cursor: Cursor::at(first, 0),
    })
}

fn toggle_prefix(l: usize, new_prefix: char, col: usize) -> EditPlan {
    EditPlan::Edit(PatchEdit {
        start: Cursor::at(l, 0),
        end: Cursor::at(l, 1),
        replacement: new_prefix.to_string(),
        cursor: Cursor::at(l, col),
    })
}

fn delete_in_added(lines: &[&str], l: usize, col: usize, dir: Dir) -> EditPlan {
    let line = lines[l];
    let n = char_len(content(line));
    let ci = content_index(line, col);
    match dir {
        // Backspace: a char precedes the caret inside content → ordinary delete.
        Dir::Back if ci >= 1 => EditPlan::Allow,
        Dir::Back => backspace_at_added_start(lines, l),
        // Forward delete: a char follows the caret inside content → ordinary.
        Dir::Forward if ci < n => EditPlan::Allow,
        Dir::Forward => forward_delete_at_added_end(lines, l),
    }
}

/// Backspace at the start of a `+` line: remove the line if empty, or join it
/// into the preceding `+` line. Never merge into a context/`-`/structural line.
fn backspace_at_added_start(lines: &[&str], l: usize) -> EditPlan {
    if l == 0 {
        return EditPlan::Block;
    }
    let line = lines[l];
    let prev = lines[l - 1];
    let empty = content(line).is_empty();
    let prev_added = classify_line(prev) == DiffLineKind::Added;
    if !empty && !prev_added {
        return EditPlan::Block;
    }
    // Delete the "\n" before this line plus this line's "+" prefix.
    let prev_end = char_len(prev);
    EditPlan::Edit(PatchEdit {
        start: Cursor::at(l - 1, prev_end),
        end: Cursor::at(l, 1),
        replacement: String::new(),
        cursor: Cursor::at(l - 1, prev_end),
    })
}

/// Forward-delete at the end of a `+` line: join the following `+` line up.
/// Never pull a context/`-`/structural line into a `+` line.
fn forward_delete_at_added_end(lines: &[&str], l: usize) -> EditPlan {
    let Some(&next) = lines.get(l + 1) else {
        return EditPlan::Block;
    };
    if next.is_empty() || classify_line(next) != DiffLineKind::Added {
        return EditPlan::Block;
    }
    let end_col = char_len(lines[l]);
    EditPlan::Edit(PatchEdit {
        start: Cursor::at(l, end_col),
        end: Cursor::at(l + 1, 1),
        replacement: String::new(),
        cursor: Cursor::at(l, end_col),
    })
}

fn deletion_is_safe_lines(lines: &[&str], lo: Cursor, hi: Cursor) -> bool {
    if lo.line != hi.line {
        return false;
    }
    let Some(&line) = lines.get(lo.line) else {
        return false;
    };
    !line.is_empty()
        && classify_line(line) == DiffLineKind::Added
        && lo.col >= 1
        && hi.col >= 1
}

// --- small string helpers (character-based) ---

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// The content of a prefixed diff line: everything after the leading char.
fn content(line: &str) -> &str {
    match line.chars().next() {
        Some(c) => &line[c.len_utf8()..],
        None => line,
    }
}

/// Map a buffer column to a character index into the line's content. Column 0 or
/// 1 both map to the content start; clamped to the content length.
fn content_index(line: &str, col: usize) -> usize {
    col.saturating_sub(1).min(char_len(content(line)))
}

/// Byte offset of character index `ch` in `s` (or `s.len()` if past the end).
fn byte_of_char(s: &str, ch: usize) -> usize {
    s.char_indices().nth(ch).map(|(b, _)| b).unwrap_or(s.len())
}

/// The substring of a diff line's content covering character indices `[a, b)`.
fn content_range(line: &str, a: usize, b: usize) -> String {
    let body = content(line);
    body[byte_of_char(body, a)..byte_of_char(body, b)].to_string()
}

/// Replace the character range `[from, to)` of `s` with `insert`.
fn splice(s: &str, from: usize, to: usize, insert: &str) -> String {
    let a = byte_of_char(s, from);
    let b = byte_of_char(s, to);
    let mut out = String::with_capacity(s.len() + insert.len());
    out.push_str(&s[..a]);
    out.push_str(insert);
    out.push_str(&s[b..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{apply_patch, unified_diff};

    fn caret(line: usize, col: usize) -> Selection {
        Selection::caret(Cursor::at(line, col))
    }

    fn sel(a: (usize, usize), b: (usize, usize)) -> Selection {
        Selection {
            anchor: Cursor::at(a.0, a.1),
            end: Cursor::at(b.0, b.1),
        }
    }

    /// Char offset of a cursor in `text`.
    fn char_offset(text: &str, c: Cursor) -> usize {
        let mut off = 0;
        for (i, line) in text.split('\n').enumerate() {
            if i == c.line {
                return off + c.col;
            }
            off += line.chars().count() + 1;
        }
        off
    }

    /// Apply an edit to `text`, returning the resulting text.
    fn apply(text: &str, e: &PatchEdit) -> String {
        let chars: Vec<char> = text.chars().collect();
        let s = char_offset(text, e.start);
        let en = char_offset(text, e.end);
        let mut out: String = chars[..s].iter().collect();
        out.push_str(&e.replacement);
        out.extend(&chars[en..]);
        out
    }

    fn edit(plan: EditPlan) -> PatchEdit {
        match plan {
            EditPlan::Edit(e) => e,
            other => panic!("expected Edit, got {other:?}"),
        }
    }

    /// A four-line modify diff: header, header, hunk, then ` a` / `-b` / `+B` / ` c`.
    fn sample() -> String {
        unified_diff("a\nb\nc\n", "a\nB\nc\n", "f")
    }

    fn line_index(patch: &str, predicate: impl Fn(&str) -> bool) -> usize {
        patch
            .split('\n')
            .position(|l| predicate(l))
            .expect("line present")
    }

    #[test]
    fn typing_into_context_splits_into_removed_and_added() {
        let patch = sample();
        let li = line_index(&patch, |l| l == " a");
        // Type 'X' after 'a' (col 2 = end of content).
        let plan = plan_edit(&patch, caret(li, 2), EditGesture::Insert("X".into()));
        let e = edit(plan);
        let out = apply(&patch, &e);
        assert!(out.contains("-a\n+aX\n"), "got:\n{out}");
        // Caret lands on the new '+' line just after the inserted char.
        assert_eq!(e.cursor, Cursor::at(li + 1, 3));
        // Still a valid patch.
        assert!(apply_patch("a\nb\nc\n", &out).is_ok());
    }

    #[test]
    fn typing_into_context_at_col_zero_inserts_at_content_start() {
        let patch = sample();
        let li = line_index(&patch, |l| l == " a");
        let plan = plan_edit(&patch, caret(li, 0), EditGesture::Insert("X".into()));
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("-a\n+Xa\n"), "got:\n{out}");
    }

    #[test]
    fn caret_delete_on_removed_line_is_rejected() {
        // A stray Backspace/Delete on a `-` line no longer un-removes it; that is
        // done by selecting the whole line (see the restore tests below).
        let patch = sample();
        let li = line_index(&patch, |l| l == "-b");
        assert_eq!(plan_edit(&patch, caret(li, 1), EditGesture::Backspace), EditPlan::Block);
        assert_eq!(plan_edit(&patch, caret(li, 1), EditGesture::Delete), EditPlan::Block);
    }

    #[test]
    fn selection_restores_a_removed_line_to_context() {
        let patch = sample(); // " a" / "-b" / "+B" / " c"
        let lb = line_index(&patch, |l| l == "-b");
        // Select the whole "-b" line, including its trailing newline.
        let plan = plan_edit(&patch, sel((lb, 0), (lb + 1, 0)), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains(" b\n") && !out.contains("-b"), "restored to context:\n{out}");
        // 'b' is no longer dropped from the file.
        assert!(apply_patch("a\nb\nc\n", &out).unwrap().contains('b'));
    }

    #[test]
    fn selection_restores_a_removed_line_without_trailing_newline() {
        let patch = sample();
        let lb = line_index(&patch, |l| l == "-b");
        // Select just the "-b" content (no newline) — a line-select gesture.
        let plan = plan_edit(&patch, sel((lb, 0), (lb, 2)), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains(" b\n") && !out.contains("-b"), "got:\n{out}");
    }

    #[test]
    fn selection_restores_multiple_removed_lines() {
        let patch = unified_diff("a\nb\nc\n", "a\n", "f"); // " a" / "-b" / "-c"
        let lb = line_index(&patch, |l| l == "-b");
        let plan = plan_edit(&patch, sel((lb, 0), (lb + 2, 0)), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains(" b\n") && out.contains(" c\n"), "both restored:\n{out}");
        assert_eq!(apply_patch("a\nb\nc\n", &out).unwrap(), "a\nb\nc\n");
    }

    #[test]
    fn partial_removed_line_selection_is_rejected() {
        let patch = unified_diff("abc\nx\n", "x\n", "f"); // "-abc"
        let li = line_index(&patch, |l| l == "-abc");
        let plan = plan_edit(&patch, sel((li, 1), (li, 3)), EditGesture::Delete);
        assert_eq!(plan, EditPlan::Block);
    }

    #[test]
    fn selection_spanning_a_removed_block_and_context_is_rejected() {
        let patch = unified_diff("a\nb\nc\n", "a\n", "f"); // " a" / "-b" / "-c"
        let la = line_index(&patch, |l| l == " a");
        // From the context " a" across the "-b"/"-c" block.
        let plan = plan_edit(&patch, sel((la, 0), (la + 2, 0)), EditGesture::Delete);
        assert_eq!(plan, EditPlan::Block);
    }

    #[test]
    fn typing_on_removed_line_is_blocked() {
        let patch = sample();
        let li = line_index(&patch, |l| l == "-b");
        let plan = plan_edit(&patch, caret(li, 2), EditGesture::Insert("X".into()));
        assert_eq!(plan, EditPlan::Block);
    }

    #[test]
    fn backspace_at_context_start_marks_removed() {
        let patch = sample();
        let li = line_index(&patch, |l| l == " c");
        let plan = plan_edit(&patch, caret(li, 1), EditGesture::Backspace);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("-c\n"), "got:\n{out}");
        // Applies, and drops 'c' from the file.
        let applied = apply_patch("a\nb\nc\n", &out).unwrap();
        assert!(!applied.contains("\nc\n") && !applied.ends_with("c\n") || applied == "a\nB\n");
    }

    #[test]
    fn context_removed_then_restored_round_trips() {
        let patch = sample();
        let li = line_index(&patch, |l| l == " c");
        // Mark the context line removed (backspace at its start).
        let removed = apply(&patch, &edit(plan_edit(&patch, caret(li, 1), EditGesture::Backspace)));
        assert!(removed.contains("-c\n"), "got:\n{removed}");
        // Restore it by selecting the whole "-c" line and deleting.
        let lc = line_index(&removed, |l| l == "-c");
        let back = apply(
            &removed,
            &edit(plan_edit(&removed, sel((lc, 0), (lc + 1, 0)), EditGesture::Delete)),
        );
        assert!(back.contains(" c\n"), "got:\n{back}");
    }

    #[test]
    fn backspace_in_context_splits_into_pair() {
        let patch = unified_diff("abc\nx\n", "abc\nY\n", "f");
        let li = line_index(&patch, |l| l == " abc");
        // Caret after 'b' (col 3); backspace drops 'b', like editing the line.
        let plan = plan_edit(&patch, caret(li, 3), EditGesture::Backspace);
        let e = edit(plan);
        let out = apply(&patch, &e);
        assert!(out.contains("-abc\n+ac\n"), "got:\n{out}");
        // Caret lands on the new '+' line at the deletion point.
        assert_eq!(e.cursor, Cursor::at(li + 1, 2));
        assert!(apply_patch("abc\nx\n", &out).is_ok());
    }

    #[test]
    fn forward_delete_in_context_splits_into_pair() {
        let patch = unified_diff("abc\nx\n", "abc\nY\n", "f");
        let li = line_index(&patch, |l| l == " abc");
        // Caret before 'b' (col 2); forward-delete drops 'b'.
        let plan = plan_edit(&patch, caret(li, 2), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("-abc\n+ac\n"), "got:\n{out}");
        assert!(apply_patch("abc\nx\n", &out).is_ok());
    }

    #[test]
    fn forward_delete_at_context_end_marks_removed() {
        // No character follows the caret, so the whole line is marked removed.
        let patch = sample();
        let li = line_index(&patch, |l| l == " c");
        let out = apply(&patch, &edit(plan_edit(&patch, caret(li, 2), EditGesture::Delete)));
        assert!(out.contains("-c\n"), "got:\n{out}");
    }

    #[test]
    fn selection_delete_in_context_splits_into_pair() {
        let patch = unified_diff("abc\nx\n", "abc\nY\n", "f");
        let li = line_index(&patch, |l| l == " abc");
        // Select "bc" (content chars 1..3 → cols 2..4) and delete it.
        let plan = plan_edit(&patch, sel((li, 2), (li, 4)), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("-abc\n+a\n"), "got:\n{out}");
        assert!(apply_patch("abc\nx\n", &out).is_ok());
    }

    /// A file with three context lines around one change, to exercise multi-line
    /// context selections: ` one` / ` two` / ` three` precede the `-x`/`+Y` edit.
    fn context_block() -> String {
        unified_diff("one\ntwo\nthree\nx\n", "one\ntwo\nthree\nY\n", "f")
    }

    #[test]
    fn selection_deletes_whole_context_lines() {
        let patch = context_block();
        let l1 = line_index(&patch, |l| l == " one");
        // Select " one".." three" whole (start of " one" to start of "-x").
        let plan = plan_edit(&patch, sel((l1, 0), (l1 + 3, 0)), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("-one\n-two\n-three\n"), "all removed:\n{out}");
        // The three lines are dropped from the new file.
        assert_eq!(apply_patch("one\ntwo\nthree\nx\n", &out).unwrap(), "Y\n");
    }

    #[test]
    fn selection_edits_half_selected_context_ends() {
        let patch = context_block();
        let l1 = line_index(&patch, |l| l == " one");
        // From mid " one" (after 'o') to mid " three" (before 'e'): the kept head
        // "o" and tail "ee" rejoin into one `+` line, all three lines removed.
        let plan = plan_edit(&patch, sel((l1, 2), (l1 + 2, 4)), EditGesture::Delete);
        let e = edit(plan);
        let out = apply(&patch, &e);
        assert!(out.contains("-one\n-two\n-three\n+oee\n"), "joined edit:\n{out}");
        // Caret at the seam, just after the kept head "o".
        assert_eq!(e.cursor, Cursor::at(line_index(&out, |l| l == "+oee"), 2));
        assert_eq!(apply_patch("one\ntwo\nthree\nx\n", &out).unwrap(), "oee\nY\n");
    }

    #[test]
    fn selection_clears_a_single_context_line_to_empty() {
        let patch = context_block();
        let l1 = line_index(&patch, |l| l == " one");
        // Select all of " one"'s content (not its newline): the line is emptied.
        let plan = plan_edit(&patch, sel((l1, 0), (l1, 4)), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("-one\n+\n"), "emptied, not removed:\n{out}");
        assert_eq!(apply_patch("one\ntwo\nthree\nx\n", &out).unwrap(), "\ntwo\nthree\nY\n");
    }

    #[test]
    fn selection_mixing_context_and_change_lines_is_blocked() {
        let patch = context_block();
        let l3 = line_index(&patch, |l| l == " three");
        // From " three" across the "-x" change line.
        let plan = plan_edit(&patch, sel((l3, 0), (l3 + 2, 0)), EditGesture::Delete);
        assert_eq!(plan, EditPlan::Block);
    }

    #[test]
    fn enter_on_context_inserts_empty_added_below() {
        let patch = sample();
        let li = line_index(&patch, |l| l == " a");
        // Caret mid-line should not matter.
        let plan = plan_edit(&patch, caret(li, 1), EditGesture::Newline);
        let e = edit(plan);
        let out = apply(&patch, &e);
        assert!(out.contains(" a\n+\n"), "got:\n{out}");
        assert_eq!(e.cursor, Cursor::at(li + 1, 1));
        assert!(apply_patch("a\nb\nc\n", &out).is_ok());
    }

    #[test]
    fn enter_in_added_line_splits_it() {
        let patch = sample();
        let li = line_index(&patch, |l| l == "+B");
        // Caret after 'B' (col 2).
        let plan = plan_edit(&patch, caret(li, 2), EditGesture::Newline);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("+B\n+\n"), "got:\n{out}");
        // Caret between '+' chars (col 2 mid-content) → split inside.
        let li2 = line_index(&patch, |l| l == "+B");
        let mid = plan_edit(&patch, caret(li2, 2), EditGesture::Newline);
        let _ = edit(mid);
    }

    #[test]
    fn typing_in_added_line_is_allowed() {
        let patch = sample();
        let li = line_index(&patch, |l| l == "+B");
        let plan = plan_edit(&patch, caret(li, 2), EditGesture::Insert("Z".into()));
        assert_eq!(plan, EditPlan::Allow);
    }

    #[test]
    fn backspace_removes_empty_added_line() {
        // Build a patch with an empty '+' line after a '+' line.
        let patch = unified_diff("a\n", "a\nb\n", "f").replace("+b\n", "+b\n+\n");
        let li = line_index(&patch, |l| l == "+"); // the empty added line
        let plan = plan_edit(&patch, caret(li, 1), EditGesture::Backspace);
        let out = apply(&patch, &edit(plan));
        assert!(!out.contains("+\n+"), "empty line should be gone:\n{out}");
        assert!(apply_patch("a\n", &out).is_ok());
    }

    #[test]
    fn backspace_at_added_start_with_context_above_is_blocked() {
        let patch = sample();
        // The '+B' line follows '-b' (removed), not added → no join.
        let li = line_index(&patch, |l| l == "+B");
        let plan = plan_edit(&patch, caret(li, 1), EditGesture::Backspace);
        assert_eq!(plan, EditPlan::Block);
    }

    #[test]
    fn backspace_joins_two_added_lines() {
        let patch = unified_diff("a\n", "a\nb\nc\n", "f");
        let li = line_index(&patch, |l| l == "+c");
        let plan = plan_edit(&patch, caret(li, 1), EditGesture::Backspace);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("+bc\n"), "got:\n{out}");
        assert_eq!(apply_patch("a\n", &out).unwrap(), "a\nbc\n");
    }

    #[test]
    fn forward_delete_joins_next_added_line() {
        let patch = unified_diff("a\n", "a\nb\nc\n", "f");
        let li = line_index(&patch, |l| l == "+b");
        // Caret at end of '+b' content (col 2).
        let plan = plan_edit(&patch, caret(li, 2), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("+bc\n"), "got:\n{out}");
    }

    #[test]
    fn structural_lines_are_read_only() {
        let patch = sample();
        let hunk = line_index(&patch, |l| l.starts_with("@@"));
        let header = line_index(&patch, |l| l.starts_with("---"));
        for li in [hunk, header] {
            assert_eq!(
                plan_edit(&patch, caret(li, 2), EditGesture::Insert("x".into())),
                EditPlan::Block
            );
            assert_eq!(plan_edit(&patch, caret(li, 1), EditGesture::Backspace), EditPlan::Block);
            assert_eq!(plan_edit(&patch, caret(li, 1), EditGesture::Newline), EditPlan::Block);
        }
    }

    #[test]
    fn first_line_backspace_at_start_is_blocked() {
        let patch = sample();
        assert_eq!(plan_edit(&patch, caret(0, 0), EditGesture::Backspace), EditPlan::Block);
    }

    #[test]
    fn selection_within_added_replaces_content() {
        // A caret-only insert in '+' content is Allow'd (default handles it);
        // with a selection the function replaces the selected span itself (GTK
        // collapses selections before insert-text, so this path is belt-and-
        // suspenders, but it must still yield a valid patch).
        let patch = sample();
        let li = line_index(&patch, |l| l == "+B");
        assert_eq!(
            plan_edit(&patch, caret(li, 2), EditGesture::Insert("Z".into())),
            EditPlan::Allow
        );
        let plan = plan_edit(&patch, sel((li, 1), (li, 2)), EditGesture::Insert("Z".into()));
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("+Z\n"), "got:\n{out}");
        assert_eq!(apply_patch("a\nb\nc\n", &out).unwrap(), "a\nZ\nc\n");
    }

    #[test]
    fn selection_within_context_insert_splits_with_replacement() {
        let patch = unified_diff("abc\nx\n", "abc\nY\n", "f");
        let li = line_index(&patch, |l| l == " abc");
        // Select "b" (chars 1..2 of content → cols 2..3) and type "Z".
        let plan = plan_edit(&patch, sel((li, 2), (li, 3)), EditGesture::Insert("Z".into()));
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("-abc\n+aZc\n"), "got:\n{out}");
        assert!(apply_patch("abc\nx\n", &out).is_ok());
    }

    #[test]
    fn multiline_selection_delete_is_blocked() {
        let patch = sample();
        let a = line_index(&patch, |l| l == " a");
        let plan = plan_edit(&patch, sel((a, 1), (a + 2, 1)), EditGesture::Delete);
        assert_eq!(plan, EditPlan::Block);
    }

    #[test]
    fn selection_deletes_a_whole_added_line() {
        let patch = unified_diff("a\n", "a\nb\nc\n", "f");
        let lb = line_index(&patch, |l| l == "+b");
        // Select the whole "+b" line, including its trailing newline.
        let plan = plan_edit(&patch, sel((lb, 0), (lb + 1, 0)), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(!out.contains("+b"), "the line is gone:\n{out}");
        assert_eq!(apply_patch("a\n", &out).unwrap(), "a\nc\n");
    }

    #[test]
    fn selection_deletes_a_whole_added_line_to_content_end() {
        let patch = unified_diff("a\n", "a\nb\nc\n", "f");
        let lb = line_index(&patch, |l| l == "+b");
        // Select "+b" from its start to the end of its content (no newline) — as a
        // line-select gesture yields — and delete it.
        let plan = plan_edit(&patch, sel((lb, 0), (lb, 2)), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(!out.contains("+b"), "the line is gone:\n{out}");
        assert_eq!(apply_patch("a\n", &out).unwrap(), "a\nc\n");
    }

    #[test]
    fn selection_deletes_multiple_added_lines() {
        let patch = unified_diff("a\n", "a\nb\nc\nd\n", "f");
        let lb = line_index(&patch, |l| l == "+b");
        // Select whole "+b" and "+c" (start of +b through start of +d).
        let plan = plan_edit(&patch, sel((lb, 0), (lb + 2, 0)), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(!out.contains("+b") && !out.contains("+c"), "both gone:\n{out}");
        assert!(out.contains("+d\n"), "the untouched line stays:\n{out}");
        assert_eq!(apply_patch("a\n", &out).unwrap(), "a\nd\n");
    }

    #[test]
    fn selection_spanning_a_context_line_is_blocked() {
        // Whole-line deletion only applies when every spanned line is added.
        let patch = unified_diff("a\n", "a\nb\n", "f");
        let la = line_index(&patch, |l| l == " a");
        let plan = plan_edit(&patch, sel((la, 0), (la + 2, 0)), EditGesture::Delete);
        assert_eq!(plan, EditPlan::Block);
    }

    #[test]
    fn partial_added_line_from_start_is_not_whole_deleted() {
        // From a line boundary but stopping mid-content: must not eat the rest.
        let patch = unified_diff("a\n", "a\nbcde\n", "f");
        let lb = line_index(&patch, |l| l == "+bcde");
        let plan = plan_edit(&patch, sel((lb, 0), (lb, 2)), EditGesture::Delete);
        assert_eq!(plan, EditPlan::Block);
    }

    #[test]
    fn partial_selection_joins_added_lines() {
        // Select from mid "+bcd" to mid "+xyz": the kept head and tail join into a
        // single `+` line, dropping what's between.
        let patch = unified_diff("a\n", "a\nbcd\nxyz\n", "f");
        let lb = line_index(&patch, |l| l == "+bcd");
        // "+bcd": caret after 'b' is col 2; "+xyz": caret before 'y' is col 2.
        let plan = plan_edit(&patch, sel((lb, 2), (lb + 1, 2)), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("+byz\n"), "the lines join:\n{out}");
        assert!(!out.contains("+bcd") && !out.contains("+xyz"), "originals gone:\n{out}");
        assert_eq!(apply_patch("a\n", &out).unwrap(), "a\nbyz\n");
    }

    #[test]
    fn partial_selection_joins_across_whole_added_lines() {
        // The join also swallows whole `+` lines caught in the middle.
        let patch = unified_diff("a\n", "a\nbcd\nmid\nxyz\n", "f");
        let lb = line_index(&patch, |l| l == "+bcd");
        let plan = plan_edit(&patch, sel((lb, 2), (lb + 2, 2)), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("+byz\n"), "got:\n{out}");
        assert_eq!(apply_patch("a\n", &out).unwrap(), "a\nbyz\n");
    }

    #[test]
    fn partial_selection_join_stops_at_a_context_line() {
        // A join may not cross a context line: that would touch unchanged content.
        let patch = unified_diff("a\nm\n", "A\nm\nB\n", "f");
        let la = line_index(&patch, |l| l == "+A");
        let lb = line_index(&patch, |l| l == "+B");
        assert!(lb > la, "context line sits between the two additions");
        let plan = plan_edit(&patch, sel((la, 1), (lb, 1)), EditGesture::Delete);
        assert_eq!(plan, EditPlan::Block);
    }

    #[test]
    fn pasting_multiline_onto_added_prefixes_continuations() {
        let patch = unified_diff("a\n", "a\nb\n", "f");
        let li = line_index(&patch, |l| l == "+b");
        // Paste "x\ny" at end of "+b".
        let plan = plan_edit(&patch, caret(li, 2), EditGesture::Insert("x\ny".into()));
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("+bx\n+y\n"), "got:\n{out}");
        assert_eq!(apply_patch("a\n", &out).unwrap(), "a\nbx\ny\n");
    }

    fn collapse(text: &str, c: Cursor) -> (String, Cursor) {
        collapse_diff(text, c).expect("a collapse")
    }

    #[test]
    fn collapse_folds_an_undone_pair_back_to_context() {
        // " a / -b / +B / c"; the user edits +B back to +b, undoing the change.
        let patch = sample().replace("+B", "+b");
        let li = line_index(&patch, |l| l == "+b");
        // Caret on the re-typed '+b' line (col 2, after 'b').
        let (out, cur) = collapse(&patch, Cursor::at(li, 2));
        assert!(out.contains(" b\n"), "the pair folds to context:\n{out}");
        assert!(!out.contains("-b") && !out.contains("+b"), "no +/- left:\n{out}");
        // Caret lands on the merged context line at the same column, so a further
        // edit re-splits it there.
        assert_eq!(cur, Cursor::at(line_index(&out, |l| l == " b"), 2));
        // Still applies, now as a no-op against the base.
        assert_eq!(apply_patch("a\nb\nc\n", &out).unwrap(), "a\nb\nc\n");
    }

    #[test]
    fn collapse_returns_none_when_nothing_matches() {
        // A genuine, still-present change must not be collapsed.
        let patch = sample();
        assert_eq!(collapse_diff(&patch, Cursor::at(0, 0)), None);
    }

    #[test]
    fn collapse_keeps_the_changed_middle_of_a_block() {
        // old "a b c" -> new "a X c": a multi-line block where only the middle
        // differs. Re-typing the surrounding lines back to equal must fold just
        // the matching ends, leaving the middle as a -/+ pair.
        let old = "a\nb\nc\n";
        let new = "A\nX\nC\n";
        let patch = unified_diff(old, new, "f");
        // Undo the first and last edits (A->a, C->c), keeping b->X.
        let patch = patch.replace("+A", "+a").replace("+C", "+c");
        let (out, _) = collapse(&patch, Cursor::at(0, 0));
        assert!(out.contains(" a\n"), "leading match folds:\n{out}");
        assert!(out.contains(" c\n"), "trailing match folds:\n{out}");
        assert!(out.contains("-b\n") && out.contains("+X\n"), "middle stays:\n{out}");
        assert_eq!(apply_patch(old, &out).unwrap(), "a\nX\nc\n");
    }

    #[test]
    fn collapse_folds_a_whole_block_when_fully_undone() {
        let old = "a\nb\n";
        let new = "A\nB\n";
        let patch = unified_diff(old, new, "f")
            .replace("+A", "+a")
            .replace("+B", "+b");
        let (out, _) = collapse(&patch, Cursor::at(0, 0));
        // No change lines remain (the `---`/`+++` headers are not Added/Removed).
        for line in out.lines() {
            let kind = classify_line(line);
            assert!(
                kind != DiffLineKind::Added && kind != DiffLineKind::Removed,
                "leftover change line: {line:?}"
            );
        }
        assert_eq!(apply_patch(old, &out).unwrap(), old);
    }

    #[test]
    fn collapse_round_trips_with_a_context_split() {
        // Folding then re-typing on the merged line reproduces a valid -/+ pair.
        let patch = sample().replace("+B", "+b");
        let li = line_index(&patch, |l| l == "+b");
        let (folded, cur) = collapse(&patch, Cursor::at(li, 2));
        // Type 'Z' at the caret on the merged context line.
        let plan = plan_edit(&folded, Selection::caret(cur), EditGesture::Insert("Z".into()));
        let resplit = apply(&folded, &edit(plan));
        assert!(resplit.contains("-b\n+bZ\n"), "re-split at the caret:\n{resplit}");
        assert!(apply_patch("a\nb\nc\n", &resplit).is_ok());
    }

    #[test]
    fn fuzz_handled_gestures_keep_the_patch_applicable() {
        // Apply a deterministic sequence of varied gestures and assert the patch
        // stays applicable at every step. Kept index-driven (not random) so a
        // failure is reproducible.
        let old = "one\ntwo\nthree\nfour\nfive\n";
        let new = "one\nTWO\nthree\nfour\nFIVE\n";
        let mut patch = unified_diff(old, new, "f");
        assert!(apply_patch(old, &patch).is_ok());

        let typed = ['q', 'w', 'e', 'r', 't', 'y'];
        for step in 0..40usize {
            let lines: Vec<String> = patch.split('\n').map(|s| s.to_string()).collect();
            // Pick a target content line deterministically.
            let li = (step * 7 + 3) % lines.len();
            let line = &lines[li];
            if line.is_empty() {
                continue;
            }
            let kind = classify_line(line);
            let col = 1 + (step % 3);
            let gesture = match step % 4 {
                0 => EditGesture::Insert(typed[step % typed.len()].to_string()),
                1 => EditGesture::Newline,
                2 => EditGesture::Backspace,
                _ => EditGesture::Delete,
            };
            // Only exercise gestures on editable kinds; skip structural lines.
            if matches!(
                kind,
                DiffLineKind::Header | DiffLineKind::Hunk | DiffLineKind::Meta
            ) {
                continue;
            }
            match plan_edit(&patch, caret(li, col), gesture) {
                EditPlan::Edit(e) => {
                    patch = apply(&patch, &e);
                    assert!(
                        apply_patch(old, &patch).is_ok(),
                        "patch became inapplicable at step {step}:\n{patch}"
                    );
                }
                EditPlan::Allow | EditPlan::Block => {}
            }
        }
    }
}
