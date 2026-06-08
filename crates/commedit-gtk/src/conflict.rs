//! Conflict-resolution support. For now this holds the pure conflict-text
//! helpers — the combined conflict view's header/layout lines, the inline
//! "use ours/theirs/both" quick-resolve cues, block-finding and resolution, and
//! the buffer scanners that map a buffer line to a file section / elision gap.
//! The conflict-mode UI wiring is added here in a later step.

use std::cell::Cell;
use std::rc::Rc;

use commedit_engine::diff::{classify_conflict_lines, ConflictLineKind};
use gtk::prelude::*;

use crate::buffer_util::buffer_text;
use crate::highlight::pill;
use crate::state::{
    Side, CONFLICT_CUE_LABEL, CONFLICT_STRUCTURAL_NOTICE, CUE_BOTH, CUE_CAP_L, CUE_OURS, CUE_THEIRS,
};

/// The header line introducing one file's section in the combined conflict view,
/// e.g. `─── src/main.rs ───`.
pub(crate) fn conflict_header_line(path: &str) -> String {
    format!("\u{2500}\u{2500}\u{2500} {path} \u{2500}\u{2500}\u{2500}")
}

/// The path of a conflict-view file header line, or `None` if `line` isn't one.
pub(crate) fn conflict_header_path(line: &str) -> Option<&str> {
    line.strip_prefix("\u{2500}\u{2500}\u{2500} ")
        .and_then(|r| r.strip_suffix(" \u{2500}\u{2500}\u{2500}"))
}

/// Whether `line` is a structural line of the combined conflict view's layout
/// (a file header, the elision cue, or the structural-conflict notice) that the
/// user must not edit, lest the snippet→full-file reconstruction lose its anchors.
pub(crate) fn is_conflict_protected_line(line: &str) -> bool {
    conflict_header_path(line).is_some()
        || line == pill(CONFLICT_CUE_LABEL)
        || line == CONFLICT_STRUCTURAL_NOTICE
}

/// Strip the inline "➜ use ours/theirs/both" cue that [`with_resolve_cues`]
/// appends to a conflict-marker line, restoring the bare marker. Applied only to
/// marker lines (so real content containing `◀` is untouched) when reconstructing
/// the full file, otherwise re-rendering would append a second cue each time.
pub(crate) fn strip_marker_cue(line: &str) -> String {
    let is_marker = ['<', '=', '>']
        .iter()
        .any(|&c| line.chars().take_while(|&x| x == c).count() >= 7);
    if is_marker {
        if let Some(pos) = line.find(CUE_CAP_L) {
            return line[..pos].trim_end().to_string();
        }
    }
    line.to_string()
}

/// The inline cue text and the side it resolves to for a marker line, or `None`
/// for a non-marker (content) line.
fn resolve_cue(kind: ConflictLineKind) -> Option<(&'static str, Side)> {
    match kind {
        ConflictLineKind::MarkerOurs => Some((CUE_OURS, Side::Ours)),
        ConflictLineKind::MarkerSep => Some((CUE_BOTH, Side::Both)),
        ConflictLineKind::MarkerTheirs => Some((CUE_THEIRS, Side::Theirs)),
        _ => None,
    }
}

/// The quick-resolve [`Side`] for a buffer position `(line, col)` if it lands on
/// a conflict block's inline "➜ use …" cue, else `None`. The single hit test
/// shared by the click gesture (which acts on it) and the hover cursor (which
/// shows a hand over it) so the two always agree. `col` is a character offset.
pub(crate) fn conflict_cue_side_at(text: &str, line: usize, col: usize) -> Option<Side> {
    let (_, side) = classify_conflict_lines(text)
        .get(line)
        .copied()
        .and_then(resolve_cue)?;
    let line_text = text.split('\n').nth(line).unwrap_or("");
    let byte = line_text.find(CUE_CAP_L)?;
    (col >= line_text[..byte].chars().count()).then_some(side)
}

/// Return `text` with the inline quick-resolve cue appended to each of its
/// conflict-marker lines (the buffer must hold materialized conflict text).
/// Appending at a line's end leaves line numbering unchanged, so the cached
/// classification stays valid; building the full string up front lets the
/// caller land it with either `set_text` or an in-place splice.
pub(crate) fn with_resolve_cues(text: &str) -> String {
    let kinds = classify_conflict_lines(text);
    let mut out = String::new();
    for (li, line) in text.split('\n').enumerate() {
        if li > 0 {
            out.push('\n');
        }
        out.push_str(line);
        if let Some((cue, _)) = kinds.get(li).and_then(|k| resolve_cue(*k)) {
            out.push_str(cue);
        }
    }
    out
}

/// Buffer line indices of the conflict-block openers (`<<<<<<<`) in the
/// currently-materialized conflict file, in document order — the scroll anchors
/// the previous/next-conflict navigation jumps between.
pub(crate) fn conflict_block_lines(buffer: &sourceview5::Buffer) -> Vec<usize> {
    let text = buffer_text(buffer);
    classify_conflict_lines(&text)
        .into_iter()
        .enumerate()
        .filter(|(_, kind)| *kind == ConflictLineKind::MarkerOurs)
        .map(|(li, _)| li)
        .collect()
}

/// Scroll the view so that buffer `line` sits at the top of the viewport and
/// park the caret there. Used to anchor the conflict navigation on the opening
/// marker of a block.
pub(crate) fn scroll_to_line(view: &sourceview5::View, buffer: &sourceview5::Buffer, line: usize) {
    if let Some(iter) = buffer.iter_at_line(line as i32) {
        buffer.place_cursor(&iter);
        // use_align with yalign 0.0 pins the line to the top edge. scroll_to_mark
        // defers until layout is valid, so this is safe right after set_text.
        view.scroll_to_mark(&buffer.get_insert(), 0.0, true, 0.0, 0.0);
    }
}

/// Find the conflict block (the `<<<<<<< … >>>>>>>` region) containing
/// `caret_line` in `text`, and compute the replacement that keeps the chosen
/// side(s) and drops the markers. Returns `(start_line, end_line, replacement)`
/// where the block spans buffer lines `start_line..=end_line` and `replacement`
/// is the text to substitute for those lines (newline-terminated). `None` if the
/// caret is not inside a conflict block.
fn resolve_conflict_block(text: &str, caret_line: usize, side: Side) -> Option<(usize, usize, String)> {
    let kinds = classify_conflict_lines(text);
    let lines: Vec<&str> = text.split('\n').collect();
    if kinds.is_empty() {
        return None;
    }
    let line = caret_line.min(kinds.len() - 1);
    // The block's opening marker is the nearest `<<<<<<<` at or before the line,
    // its closing marker the next `>>>>>>>` after that. Anchoring on the opener
    // (rather than walking back and bailing on a closing marker) lets a click on
    // any line of the block resolve it — including the closing marker itself,
    // which carries the "use theirs" cue.
    let start = (0..=line).rev().find(|&i| kinds[i] == ConflictLineKind::MarkerOurs)?;
    let end = (start + 1..kinds.len()).find(|&i| kinds[i] == ConflictLineKind::MarkerTheirs)?;
    // Reject a line that sits past this block's close (i.e. between two blocks).
    if line > end {
        return None;
    }
    let mut ours = Vec::new();
    let mut theirs = Vec::new();
    for i in start + 1..end {
        match kinds[i] {
            ConflictLineKind::Ours => ours.push(lines[i]),
            ConflictLineKind::Theirs => theirs.push(lines[i]),
            _ => {}
        }
    }
    let chosen: Vec<&str> = match side {
        Side::Ours => ours,
        Side::Theirs => theirs,
        Side::Both => ours.into_iter().chain(theirs).collect(),
    };
    let mut replacement = String::new();
    for line in chosen {
        replacement.push_str(line);
        replacement.push('\n');
    }
    Some((start, end, replacement))
}

/// Resolve the conflict block containing buffer `line` by keeping `side` and
/// dropping its markers (and the inline cues attached to them), as one undo
/// step. Returns `false` (a no-op) if the line is not inside a conflict block.
/// The `editing` guard marks the edit as our own so the conflict pane's free-form
/// editing path lets it through; `highlight` recolors the now-shrunk buffer.
pub(crate) fn resolve_conflict_at(
    buffer: &sourceview5::Buffer,
    editing: &Rc<Cell<bool>>,
    line: usize,
    side: Side,
    highlight: &dyn Fn(),
) -> bool {
    let text = buffer_text(buffer);
    let Some((start, end, replacement)) = resolve_conflict_block(&text, line, side) else {
        return false;
    };
    editing.set(true);
    buffer.begin_user_action();
    let mut s = buffer
        .iter_at_line(start as i32)
        .unwrap_or_else(|| buffer.start_iter());
    let mut e = buffer
        .iter_at_line(end as i32 + 1)
        .unwrap_or_else(|| buffer.end_iter());
    buffer.delete(&mut s, &mut e);
    let mut at = buffer
        .iter_at_line(start as i32)
        .unwrap_or_else(|| buffer.end_iter());
    buffer.insert(&mut at, &replacement);
    buffer.end_user_action();
    editing.set(false);
    highlight();
    true
}

/// Index (in file order) of the conflict-view section containing buffer `line` —
/// the count of file headers at or before it, minus one. Scans the live buffer so
/// it survives edits. The conflict analogue of [`diff_file_index_at_line`].
pub(crate) fn conflict_file_index_at_line(buffer: &sourceview5::Buffer, line: usize) -> usize {
    let text = buffer_text(buffer);
    let mut idx = 0;
    let mut seen = 0usize;
    for (i, l) in text.split('\n').enumerate() {
        if i > line {
            break;
        }
        if conflict_header_path(l).is_some() {
            idx = seen;
            seen += 1;
        }
    }
    idx
}

/// Buffer line of the `idx`-th file header in the combined conflict view.
pub(crate) fn conflict_file_header_line(buffer: &sourceview5::Buffer, idx: usize) -> Option<usize> {
    let text = buffer_text(buffer);
    text.split('\n')
        .enumerate()
        .filter(|(_, l)| conflict_header_path(l).is_some())
        .nth(idx)
        .map(|(i, _)| i)
}

/// For a click on the elision cue at buffer `line`, the `(file index, gap index)`
/// it addresses: which file section it falls in, and which cue within that
/// section it is (0-based, document order, matching the recorded gaps). `None` if
/// `line` is not an elision cue.
pub(crate) fn conflict_cue_gap_at(buffer: &sourceview5::Buffer, line: usize) -> Option<(usize, usize)> {
    let cue = pill(CONFLICT_CUE_LABEL);
    let text = buffer_text(buffer);
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.get(line).copied() != Some(cue.as_str()) {
        return None;
    }
    let mut file_idx = 0usize;
    let mut seen_files = 0usize;
    let mut k = 0usize;
    for l in &lines[..line] {
        if conflict_header_path(l).is_some() {
            file_idx = seen_files;
            seen_files += 1;
            k = 0;
        } else if *l == cue {
            k += 1;
        }
    }
    Some((file_idx, k))
}
