//! Small, GTK-only helpers for reading and editing the diff/conflict text
//! buffers: whole-buffer and per-line text, selection/iter conversions to the
//! engine's structured-edit [`Cursor`]/[`Selection`], the guarded patch-edit
//! application, the minimal-span splice, and the file-change dropdown label.

use std::cell::Cell;
use std::rc::Rc;

use commedit_engine::diff::{ChangeKind, FileChange};
use commedit_engine::patch_edit::{Cursor, PatchEdit, Selection};
use gtk::prelude::*;

pub(crate) fn buffer_text(buffer: &sourceview5::Buffer) -> String {
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

/// The text of buffer `line` (without its trailing newline).
pub(crate) fn buffer_line_text(buffer: &sourceview5::Buffer, line: usize) -> String {
    let Some(start) = buffer.iter_at_line(line as i32) else {
        return String::new();
    };
    let mut end = start;
    if !end.ends_line() {
        end.forward_to_line_end();
    }
    buffer.text(&start, &end, false).to_string()
}

/// Buffer iter at a structured-edit [`Cursor`] (line + character column).
pub(crate) fn iter_at(buffer: &sourceview5::Buffer, c: &Cursor) -> gtk::TextIter {
    buffer
        .iter_at_line_offset(c.line as i32, c.col as i32)
        .unwrap_or_else(|| buffer.end_iter())
}

/// The current selection (or a collapsed caret) as a structured-edit
/// [`Selection`].
pub(crate) fn buffer_selection(buffer: &sourceview5::Buffer) -> Selection {
    if let Some((s, e)) = buffer.selection_bounds() {
        Selection {
            anchor: Cursor {
                line: s.line() as usize,
                col: s.line_offset() as usize,
            },
            end: Cursor {
                line: e.line() as usize,
                col: e.line_offset() as usize,
            },
        }
    } else {
        let it = buffer.iter_at_offset(buffer.cursor_position());
        Selection::caret(Cursor {
            line: it.line() as usize,
            col: it.line_offset() as usize,
        })
    }
}

/// Apply a planned [`PatchEdit`] as a single undo step. The `editing` guard marks
/// the mutation as our own so the firewall signal handlers let it through.
///
/// A structured edit can change a line's diff *kind* — splitting a context line
/// into a `-orig`/`+edited` pair, or toggling a prefix — so it must re-highlight.
/// Because the `editing` guard suppresses the buffer's debounced `changed`
/// re-highlight (to avoid double work on a full render), do it here synchronously
/// once the guard is cleared, so the new `+`/`-` line is colored immediately.
pub(crate) fn apply_patch_edit(
    buffer: &sourceview5::Buffer,
    editing: &Rc<Cell<bool>>,
    edit: &PatchEdit,
    highlight: &dyn Fn(),
) {
    editing.set(true);
    buffer.begin_user_action();
    let mut start = iter_at(buffer, &edit.start);
    let mut end = iter_at(buffer, &edit.end);
    buffer.delete(&mut start, &mut end);
    let mut at = iter_at(buffer, &edit.start);
    buffer.insert(&mut at, &edit.replacement);
    buffer.end_user_action();
    editing.set(false);
    let cursor = iter_at(buffer, &edit.cursor);
    buffer.place_cursor(&cursor);
    highlight();
}

pub(crate) fn change_label(change: &FileChange) -> String {
    let sigil = match change.kind {
        ChangeKind::Added => "+",
        ChangeKind::Modified => "~",
        ChangeKind::Removed => "-",
    };
    format!("{sigil} {}", change.path)
}

/// Replace the buffer's contents with `new_text` by editing only the span that
/// actually differs — the common leading/trailing runs are left untouched — so
/// GTK keeps the scroll position instead of resetting it to the top as
/// `set_text` would (and then fighting GTK's deferred validation; see the diff
/// expand handler). Mirrors the localized splice in `collapse`. The caller must
/// hold the `editing` guard so the firewall treats it as our own edit.
pub(crate) fn splice_buffer_text(buffer: &sourceview5::Buffer, new_text: &str) {
    let old = buffer_text(buffer);
    if old == new_text {
        return;
    }
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new_text.chars().collect();
    let mut head = 0;
    while head < old_chars.len() && head < new_chars.len() && old_chars[head] == new_chars[head] {
        head += 1;
    }
    let mut tail = 0;
    while tail < old_chars.len() - head
        && tail < new_chars.len() - head
        && old_chars[old_chars.len() - 1 - tail] == new_chars[new_chars.len() - 1 - tail]
    {
        tail += 1;
    }
    let middle: String = new_chars[head..new_chars.len() - tail].iter().collect();
    buffer.begin_user_action();
    let mut start = buffer.iter_at_offset(head as i32);
    let mut end = buffer.iter_at_offset((old_chars.len() - tail) as i32);
    buffer.delete(&mut start, &mut end);
    let mut at = buffer.iter_at_offset(head as i32);
    buffer.insert(&mut at, &middle);
    buffer.end_user_action();
}
