//! Conflict-resolution support. Two layers: the pure conflict-text helpers — the
//! combined conflict view's header/layout lines, the inline "use ours/theirs/both"
//! quick-resolve cues, block-finding and resolution, and the buffer scanners that
//! map a buffer line to a file section / elision gap — and the conflict-mode UI
//! wiring: the callback builders (`build_*`, called by `build_ui` in dependency
//! order) and `wire`, which installs the abort/navigation events.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;

use commedit_engine::conflict::{ConflictedCommit, SaveOutcome};
use commedit_engine::diff::{classify_conflict_lines, ConflictLineKind};
use commedit_engine::history::history;
use gtk::prelude::*;

use crate::buffer_util::buffer_text;
use crate::highlight::pill;
use crate::rows::populate_list;
use crate::state::{
    Callbacks, ConflictCtx, Data, PaneMode, Side, Widgets, CONFLICT_CUE_LABEL,
    CONFLICT_STRUCTURAL_NOTICE, CUE_BOTH, CUE_CAP_L, CUE_OURS, CUE_THEIRS, SAVE_HINT_CONFLICT,
    SAVE_HINT_DIFF,
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

// --- Conflict-mode UI wiring -------------------------------------------------
//
// The conflict-mode callbacks are built in `build_ui` in strict dependency order
// (`refresh_conflict` -> `exit_conflict_mode` -> `enter_conflict_mode` ->
// `resolve_current`) and bundled into `Callbacks`; `wire` then installs the
// conflict-mode events. Each builder re-binds the handles it needs out of the
// borrowed bundles, so the moved closures are otherwise verbatim.

/// Build the `refresh_conflict` callback: rebuild the history list from jj's
/// pending (not-yet-exported) head while a conflicted rewrite is being resolved,
/// badging the still-conflicted commits and updating the banner's progress text.
/// Selecting a row cascades through `row-selected` -> `load_conflict_files`.
pub(crate) fn build_refresh_conflict(w: &Widgets, d: &Data) -> Rc<dyn Fn()> {
    let repo = d.repo.clone();
    let commits = d.commits.clone();
    let list = w.list.clone();
    let pane_mode = d.pane_mode.clone();
    let conflict_label = w.conflict_label.clone();
    let selected_change = d.selected_change.clone();
    let wc_list = w.wc_list.clone();
    Rc::new(move || {
        let loaded = {
            let r = repo.borrow();
            match r.jj_head_commit_id() {
                Some(head) => history(&r.repo, &head).unwrap_or_default(),
                None => Vec::new(),
            }
        };
        *commits.borrow_mut() = loaded;
        let (badges, n_files, n_commits) = {
            let mode = pane_mode.borrow();
            if let PaneMode::Conflict(ctx) = &*mode {
                let badges = ctx.conflicted_changes();
                let files: usize = ctx.commits.iter().map(|c| c.files.len()).sum();
                (badges.clone(), files, badges.len())
            } else {
                (HashSet::new(), 0, 0)
            }
        };
        // The working-copy chain resolves inline among the conflicted
        // commits, so hide the standalone rows and prepend each conflicted
        // entry to the chain. Insert oldest-first (the chain is newest-first)
        // so the newest entry lands at the top, above the branch tip.
        wc_list.set_visible(false);
        for entry in repo.borrow().working_copy_chain().into_iter().rev() {
            if badges.contains(&entry.info.change_id_hex()) {
                commits.borrow_mut().insert(0, entry.info);
            }
        }
        populate_list(&list, &commits.borrow(), &badges);
        conflict_label.set_text(&format!(
            "Conflicts from the rewrite must be resolved before it applies to git — \
             {n_files} file(s) across {n_commits} commit(s) remaining."
        ));
        // Re-select the previously selected commit if it's still in the chain,
        // else the first conflicted one. Unselect first so the signal always
        // fires (the file list may have changed even for the same row).
        let target = selected_change
            .borrow()
            .clone()
            .filter(|ch| badges.contains(ch))
            .or_else(|| {
                commits
                    .borrow()
                    .iter()
                    .map(|c| c.change_id_hex())
                    .find(|ch| badges.contains(ch))
            });
        let idx = target.and_then(|ch| {
            commits
                .borrow()
                .iter()
                .position(|c| c.change_id_hex() == ch)
        });
        list.unselect_all();
        if let Some(idx) = idx {
            if let Some(row) = list.row_at_index(idx as i32) {
                list.select_row(Some(&row));
            }
        }
    })
}

/// Build the `exit_conflict_mode` callback: back to the normal diff pane, banner
/// hidden.
pub(crate) fn build_exit_conflict_mode(w: &Widgets, d: &Data) -> Rc<dyn Fn()> {
    let pane_mode = d.pane_mode.clone();
    let conflict_banner = w.conflict_banner.clone();
    let prev_conflict_button = w.prev_conflict_button.clone();
    let next_conflict_button = w.next_conflict_button.clone();
    let save_button = w.save_button.clone();
    Rc::new(move || {
        *pane_mode.borrow_mut() = PaneMode::Diff;
        conflict_banner.set_visible(false);
        prev_conflict_button.set_visible(false);
        next_conflict_button.set_visible(false);
        save_button.set_tooltip_text(Some(SAVE_HINT_DIFF));
    })
}

/// Build the `enter_conflict_mode` callback: show the banner, select the oldest
/// conflicted commit, and render the pending chain. The quick-resolve affordances
/// are the inline marker-line cues (see `with_resolve_cues`).
pub(crate) fn build_enter_conflict_mode(
    w: &Widgets,
    d: &Data,
    refresh_conflict: Rc<dyn Fn()>,
) -> Rc<dyn Fn(Vec<ConflictedCommit>)> {
    let pane_mode = d.pane_mode.clone();
    let conflict_banner = w.conflict_banner.clone();
    let selected_change = d.selected_change.clone();
    let prev_conflict_button = w.prev_conflict_button.clone();
    let next_conflict_button = w.next_conflict_button.clone();
    let save_button = w.save_button.clone();
    Rc::new(move |commits: Vec<ConflictedCommit>| {
        let first = commits
            .iter()
            .find(|c| !c.files.is_empty())
            .map(|c| c.change_id_hex());
        *pane_mode.borrow_mut() = PaneMode::Conflict(ConflictCtx { commits });
        conflict_banner.set_visible(true);
        prev_conflict_button.set_visible(true);
        next_conflict_button.set_visible(true);
        save_button.set_tooltip_text(Some(SAVE_HINT_CONFLICT));
        if let Some(ch) = first {
            *selected_change.borrow_mut() = Some(ch);
        }
        refresh_conflict();
    })
}

/// Build the `resolve_current` callback: resolve the conflicted file currently in
/// the buffer. The engine re-checks the whole chain: when the last conflict clears
/// it exports the rewrite and we return to the normal view, otherwise the
/// remaining conflicts are re-shown.
pub(crate) fn build_resolve_current(
    d: &Data,
    refresh: Rc<dyn Fn()>,
    refresh_conflict: Rc<dyn Fn()>,
    exit_conflict_mode: Rc<dyn Fn()>,
    show_status: Rc<dyn Fn(&str)>,
    sync_conflict_from_buffer: Rc<dyn Fn()>,
) -> Rc<dyn Fn()> {
    let repo = d.repo.clone();
    let pane_mode = d.pane_mode.clone();
    let selected_change = d.selected_change.clone();
    let conflict_view = d.conflict_view.clone();
    Rc::new(move || {
        if !pane_mode.borrow().is_conflict() {
            return;
        }
        let Some(change_hex) = selected_change.borrow().clone() else {
            show_status("Select a conflicted commit to resolve");
            return;
        };
        // Capture buffer edits into each file's full text, then reconstruct the
        // (path, full_text, marker_len) list for this commit's resolvable files.
        sync_conflict_from_buffer();
        let files: Option<Vec<(String, String, usize)>> = {
            let view = conflict_view.borrow();
            let mut out = Vec::new();
            let mut unresolved = false;
            for fv in view.iter().filter(|fv| fv.resolvable) {
                if classify_conflict_lines(&fv.full_text).iter().any(|k| k.is_marker()) {
                    unresolved = true;
                    break;
                }
                out.push((fv.path.clone(), fv.full_text.clone(), fv.marker_len));
            }
            if unresolved {
                None
            } else {
                Some(out)
            }
        };
        let Some(files) = files else {
            show_status("Resolve all conflict markers before saving");
            return;
        };
        if files.is_empty() {
            show_status("No text-resolvable conflicts here — use “Abort rewrite” to discard");
            return;
        }
        let outcome = repo.borrow_mut().resolve_conflicts(&change_hex, &files);
        match outcome {
            Ok(SaveOutcome::Clean) => {
                exit_conflict_mode();
                refresh();
                show_status("Conflicts resolved — rewrite applied.");
            }
            Ok(SaveOutcome::Conflicts { commits }) => {
                if let PaneMode::Conflict(ctx) = &mut *pane_mode.borrow_mut() {
                    ctx.commits = commits;
                }
                refresh_conflict();
                show_status("Resolved — more conflicts remain.");
            }
            Err(err) => show_status(&format!("Resolve failed: {err}")),
        }
    })
}

/// Install the conflict-mode events: the Abort button, the previous/next-conflict
/// navigation buttons, and the cursor-driven nav-button enabler.
pub(crate) fn wire(w: &Widgets, d: &Data, cb: &Callbacks) {
    let repo = d.repo.clone();
    let pane_mode = d.pane_mode.clone();
    let list = w.list.clone();
    let abort_button = w.abort_button.clone();
    let file_buffer = w.file_buffer.clone();
    let file_view = w.file_view.clone();
    let prev_conflict_button = w.prev_conflict_button.clone();
    let next_conflict_button = w.next_conflict_button.clone();
    let refresh = cb.refresh.clone();
    let show_status = cb.show_status.clone();
    let exit_conflict_mode = cb.exit_conflict_mode.clone();

    abort_button.connect_clicked({
        let repo = repo.clone();
        let exit_conflict_mode = exit_conflict_mode.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let list = list.clone();
        move |_| {
            if let Err(err) = repo.borrow_mut().abort() {
                show_status(&format!("Abort failed: {err}"));
                return;
            }
            exit_conflict_mode();
            // The aborted commit is still selected, so `refresh` re-selecting it
            // (rows are reused, not rebuilt) wouldn't re-fire `row-selected` —
            // leaving the diff pane showing the abandoned conflict markers until
            // the user clicks away and back. Drop the selection first so the
            // reselect fires and reloads the now-conflict-free diff.
            list.unselect_all();
            refresh();
            show_status("Rewrite aborted — history unchanged.");
        }
    });

    // ◀ / ▶ jump the view to the previous/next conflict block, anchored on the
    // caret line (which scroll_to_line parks on each block's opening marker, so
    // repeated presses step through the file).
    let goto_conflict: Rc<dyn Fn(bool)> = {
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let pane_mode = pane_mode.clone();
        Rc::new(move |forward: bool| {
            if !pane_mode.borrow().is_conflict() {
                return;
            }
            let blocks = conflict_block_lines(&file_buffer);
            let caret = file_buffer.iter_at_mark(&file_buffer.get_insert()).line() as usize;
            let target = if forward {
                blocks.iter().find(|&&l| l > caret).copied()
            } else {
                blocks.iter().rev().find(|&&l| l < caret).copied()
            };
            if let Some(line) = target {
                scroll_to_line(&file_view, &file_buffer, line);
            }
        })
    };
    prev_conflict_button.connect_clicked({
        let goto_conflict = goto_conflict.clone();
        move |_| goto_conflict(false)
    });
    next_conflict_button.connect_clicked({
        let goto_conflict = goto_conflict.clone();
        move |_| goto_conflict(true)
    });

    // Keep the nav buttons enabled only while there's a conflict to jump to,
    // relative to the caret. Driven off the cursor-position property, which the
    // buffer fires on every caret move — clicks, typing, set_text (which resets
    // the caret), and scroll_to_line's place_cursor — so this stays in sync as
    // the file is navigated and conflicts get resolved away.
    file_buffer.connect_cursor_position_notify({
        let pane_mode = pane_mode.clone();
        let prev_conflict_button = prev_conflict_button.clone();
        let next_conflict_button = next_conflict_button.clone();
        move |buffer| {
            if !pane_mode.borrow().is_conflict() {
                return;
            }
            let blocks = conflict_block_lines(buffer);
            let caret = buffer.iter_at_mark(&buffer.get_insert()).line() as usize;
            prev_conflict_button.set_sensitive(blocks.iter().any(|&l| l < caret));
            next_conflict_button.set_sensitive(blocks.iter().any(|&l| l > caret));
        }
    });
}
