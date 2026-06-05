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
//! * Backspace/Delete on a `-` line un-removes it (→ context); at the start of a
//!   context line it marks the line removed (` ` → `-`) — a clean toggle.
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
        // Deleting a selection: only safe wholly inside one `+` line's content.
        let (lo, hi) = sel.ordered();
        return if deletion_is_safe_lines(lines, lo, hi) {
            EditPlan::Allow
        } else {
            EditPlan::Block
        };
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
        // Un-remove: `-` → context, caret preserved.
        DiffLineKind::Removed => toggle_prefix(l, ' ', caret.col),
        // At the line start, mark the line removed: ` ` → `-`. Elsewhere the
        // content is immutable (type to change it), so block.
        DiffLineKind::Context => {
            if caret.col <= 1 {
                toggle_prefix(l, '-', caret.col)
            } else {
                EditPlan::Block
            }
        }
        DiffLineKind::Added => delete_in_added(lines, l, caret.col, dir),
        _ => EditPlan::Block,
    }
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
    fn backspace_on_removed_line_restores_context() {
        let patch = sample();
        let li = line_index(&patch, |l| l == "-b");
        let plan = plan_edit(&patch, caret(li, 1), EditGesture::Backspace);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains(" b\n"), "got:\n{out}");
        // Applying restores 'b' in the file.
        let applied = apply_patch("a\nb\nc\n", &out).unwrap();
        assert!(applied.contains("a\nb\nB\nc\n") || applied == "a\nb\nB\nc\n");
    }

    #[test]
    fn delete_on_removed_line_restores_context() {
        let patch = sample();
        let li = line_index(&patch, |l| l == "-b");
        let plan = plan_edit(&patch, caret(li, 1), EditGesture::Delete);
        let out = apply(&patch, &edit(plan));
        assert!(out.contains(" b\n"), "got:\n{out}");
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
    fn context_removed_toggle_round_trips() {
        let patch = sample();
        let li = line_index(&patch, |l| l == " c");
        let removed = apply(&patch, &edit(plan_edit(&patch, caret(li, 1), EditGesture::Backspace)));
        // Now the line is "-c"; backspace again restores context.
        let back = apply(&removed, &edit(plan_edit(&removed, caret(li, 1), EditGesture::Backspace)));
        assert!(back.contains(" c\n"), "got:\n{back}");
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
    fn pasting_multiline_onto_added_prefixes_continuations() {
        let patch = unified_diff("a\n", "a\nb\n", "f");
        let li = line_index(&patch, |l| l == "+b");
        // Paste "x\ny" at end of "+b".
        let plan = plan_edit(&patch, caret(li, 2), EditGesture::Insert("x\ny".into()));
        let out = apply(&patch, &edit(plan));
        assert!(out.contains("+bx\n+y\n"), "got:\n{out}");
        assert_eq!(apply_patch("a\n", &out).unwrap(), "a\nbx\ny\n");
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
