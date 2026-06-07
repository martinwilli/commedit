//! commedit GTK4 UI (Milestone 2): browse history, edit a commit message, and
//! edit the content of files a commit changes. Saving transparently rewrites the
//! commit and rebases descendants via the engine.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use commedit_engine::conflict::{ConflictedCommit, SaveOutcome};
use commedit_engine::diff::{
    apply_patch, classify_conflict_lines, commit_changes, parse_diff_lines, reconstruct_conflict_file,
    render_commit_diff, render_conflict_snippets, split_combined_patch, ChangeKind, CombinedFile,
    ConflictLineKind, ConflictPiece, ContextExpansion, DiffLineKind, FileChange, HunkInfo,
};
use commedit_engine::history::{history, history_limited, CommitInfo};
use commedit_engine::patch_edit::{
    collapse_diff, deletion_is_safe, plan_edit, Cursor, EditGesture, EditPlan, PatchEdit, Selection,
};
use commedit_engine::repo::Repo;
use commedit_engine::rewrite::Identity;
use commedit_engine::squash::{parse_squash_mode, SquashMode};
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    gdk, Application, ApplicationWindow, Box as GtkBox, Button, Calendar, CallbackAction, DragSource,
    DropDown, DropTarget, Entry, EventControllerKey, EventControllerScroll,
    EventControllerScrollFlags, Grid, HeaderBar, Label, ListBox, ListBoxRow,
    MenuButton, Orientation, Paned, PolicyType, Popover, PropagationPhase, ScrolledWindow, Shortcut,
    ShortcutController, ShortcutTrigger, Stack, StringList, TextTag, ToggleButton,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

const APP_ID: &str = "net.willi.commedit";

/// How many history rows to load per page. The list starts with one page and
/// grows by another whenever the user scrolls near the bottom (see the
/// `history_scroll` edge handler), so opening a deep repo stays cheap.
const HISTORY_PAGE: usize = 64;

/// A reference-counted, re-entrant "render the current diff" callback. Boxed so
/// the embedded expand-context buttons can hold and invoke it after they widen a
/// hunk (the renderer rebuilds the buffer and the buttons themselves).
type Renderer = Rc<dyn Fn()>;

/// Which list a drag started in, so the shared drop handlers can tell a reorder
/// (history → history), a drop (history → trash) and a restore (trash → history)
/// apart. The carried value is just the source row index; this says where from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DragOrigin {
    History,
    Trash,
}

/// Which content the diff pane is showing. In `Diff` mode it's the usual
/// editable unified diff guarded by the patch firewall. In `Conflict` mode a
/// rewrite produced conflicts that git is held back from until they're resolved:
/// the pane shows a conflicted file materialized with 2-way markers, edited
/// free-form (the firewall is bypassed), and saving resolves rather than
/// rewrites.
enum PaneMode {
    Diff,
    Conflict(ConflictCtx),
}

impl PaneMode {
    fn is_conflict(&self) -> bool {
        matches!(self, PaneMode::Conflict(_))
    }
}

/// The live state of an in-progress conflict resolution: the conflicted commits,
/// refreshed from the engine after each resolution step, oldest first.
struct ConflictCtx {
    commits: Vec<ConflictedCommit>,
}

/// Per-file state of the combined conflict-snippet buffer currently shown (one
/// per conflicted file of the selected commit). The buffer shows only each file's
/// conflict snippets — its `<<< … >>>` blocks plus context, with the long
/// unconflicted runs elided behind a cue — so on save we reconstruct each whole
/// file from the (edited) shown segments interleaved with the verbatim elided
/// runs recorded in `pieces`.
struct ConflictFileView {
    path: String,
    /// False for structural (non-text) conflicts, shown as a read-only notice.
    resolvable: bool,
    /// Marker length jj used, echoed back on resolve so the edit re-parses.
    marker_len: usize,
    /// The file's current full conflict text (source of truth): re-windowed on
    /// render, refreshed from the buffer (capturing edits) on expand/save.
    full_text: String,
    /// Per-file snippet context expansion (the elision cues widen it).
    exp: ContextExpansion,
    /// Pieces recorded at the last render, for reconstructing the full file.
    pieces: Vec<ConflictPiece>,
    /// The elision gaps recorded at the last render, in document order, as
    /// `(above_block, below_block)` — which blocks' context a cue click widens.
    gaps: Vec<(Option<usize>, Option<usize>)>,
}

impl ConflictCtx {
    /// The change ids (hex) of commits that still have conflicts — used to badge
    /// the matching history rows.
    fn conflicted_changes(&self) -> HashSet<String> {
        self.commits
            .iter()
            .filter(|c| !c.files.is_empty())
            .map(|c| c.change_id_hex())
            .collect()
    }
}

/// Which side(s) of a conflict block a quick-resolve action keeps.
#[derive(Clone, Copy)]
enum Side {
    Ours,
    Theirs,
    Both,
}

/// Inline, clickable quick-resolve cues appended to a conflict block's marker
/// lines — the same idiom as the diff view's "expand context" cue. Clicking the
/// marker line keeps the indicated side(s) and drops the markers: "use ours"
/// after `<<<<<<<`, "use theirs" after `>>>>>>>`, "use both" after `=======`.
const CUE_OURS: &str = " ◀ ➜ use ours ▶";
const CUE_BOTH: &str = " ◀ ➜ use both ▶";
const CUE_THEIRS: &str = " ◀ ➜ use theirs ▶";
/// The end-caps that make a cue read as a banner/tag-shaped button. Painted as a
/// full-height triangle in the button colour against the line background, their
/// flat (vertical) side sits flush against the solid-fill body between them, so
/// they align in height and touch the block, giving pointed ends. The left cap
/// also marks where the clickable button begins.
const CUE_CAP_L: char = '◀';
const CUE_CAP_R: char = '▶';

/// Tooltips for the action-bar buttons. The Save button means different things
/// per pane mode — committing an edit in the diff view, resolving a file in the
/// conflict view — so its tooltip is swapped when entering/leaving conflict mode.
const SAVE_HINT_DIFF: &str =
    "Save your edits to this commit — message, identity, or file content — \
     rewriting it in place and rebasing its descendants onto the result.";
const SAVE_HINT_CONFLICT: &str =
    "Resolve the conflicted file shown above. When a rewrite conflicts across \
     several files you resolve them one at a time — save each in turn; the \
     rewrite is applied to git only once the last conflict is cleared.";
const ABORT_HINT: &str =
    "Discard the entire rewrite and roll the repository back to the state it had \
     before you saved, leaving git untouched.";
/// Hover hint for the diff view's Split button (enabled only with pending diff edits).
const SPLIT_HINT: &str =
    "Split this commit in two: rewrite it to your edited diff, and add a new commit \
     after it holding the changes you took out — so the two together reproduce the \
     original commit and its descendants stay unchanged.";

/// Wrap a cue label in the banner caps, e.g. `↕ expand context` -> `◀ ↕ expand context ▶`.
fn pill(label: &str) -> String {
    format!("{CUE_CAP_L} {label} {CUE_CAP_R}")
}

/// Label of the conflict pane's elision cue — the pill standing in for a hidden
/// run of unconflicted lines between snippets. Clicking it reveals more context.
const CONFLICT_CUE_LABEL: &str = "↕ expand hidden lines";

/// The standalone notice shown for a structural (non-text-resolvable) conflicted
/// file in the combined conflict view.
const CONFLICT_STRUCTURAL_NOTICE: &str =
    "⚠ structural conflict — can't be resolved as text here; use “Abort rewrite”";

/// The header line introducing one file's section in the combined conflict view,
/// e.g. `─── src/main.rs ───`.
fn conflict_header_line(path: &str) -> String {
    format!("\u{2500}\u{2500}\u{2500} {path} \u{2500}\u{2500}\u{2500}")
}

/// The path of a conflict-view file header line, or `None` if `line` isn't one.
fn conflict_header_path(line: &str) -> Option<&str> {
    line.strip_prefix("\u{2500}\u{2500}\u{2500} ")
        .and_then(|r| r.strip_suffix(" \u{2500}\u{2500}\u{2500}"))
}

/// Whether `line` is a structural line of the combined conflict view's layout
/// (a file header, the elision cue, or the structural-conflict notice) that the
/// user must not edit, lest the snippet→full-file reconstruction lose its anchors.
fn is_conflict_protected_line(line: &str) -> bool {
    conflict_header_path(line).is_some()
        || line == pill(CONFLICT_CUE_LABEL)
        || line == CONFLICT_STRUCTURAL_NOTICE
}

/// Strip the inline "➜ use ours/theirs/both" cue that [`append_resolve_cues`]
/// appends to a conflict-marker line, restoring the bare marker. Applied only to
/// marker lines (so real content containing `◀` is untouched) when reconstructing
/// the full file, otherwise re-rendering would append a second cue each time.
fn strip_marker_cue(line: &str) -> String {
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

/// Paint the inline banner button on `raw` (buffer line `line`): the two end-caps
/// get `cap_tag` (a coloured triangle on the line background), the run between
/// them gets `body_tag` (the solid button fill). No-op if the line has no caps.
fn paint_pill(buffer: &sourceview5::Buffer, line: i32, raw: &str, cap_tag: &str, body_tag: &str) {
    let (Some(lpos), Some(rpos)) = (raw.find(CUE_CAP_L), raw.rfind(CUE_CAP_R)) else {
        return;
    };
    let table = buffer.tag_table();
    let (Some(cap), Some(body)) = (table.lookup(cap_tag), table.lookup(body_tag)) else {
        return;
    };
    let lc = raw[..lpos].chars().count() as i32;
    let rc = raw[..rpos].chars().count() as i32;
    apply_cols(buffer, line, lc, lc + 1, &cap);
    if rc > lc + 1 {
        apply_cols(buffer, line, lc + 1, rc, &body);
    }
    apply_cols(buffer, line, rc, rc + 1, &cap);
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
fn conflict_cue_side_at(text: &str, line: usize, col: usize) -> Option<Side> {
    let (_, side) = classify_conflict_lines(text)
        .get(line)
        .copied()
        .and_then(resolve_cue)?;
    let line_text = text.split('\n').nth(line).unwrap_or("");
    let byte = line_text.find(CUE_CAP_L)?;
    (col >= line_text[..byte].chars().count()).then_some(side)
}

/// The hunk group range to widen for a click/hover at buffer `(line, col)`, if it
/// lands on an expandable `@@` header's inline pill cue. `line_text` is that
/// line's text. The single hit test shared by the expand click and the hover
/// cursor, restricting both to the pill rather than the whole header line.
fn expand_cue_at(
    hunks: &[HunkInfo],
    line_text: &str,
    line: usize,
    col: usize,
) -> Option<(usize, usize)> {
    let cap = line_text.find(CUE_CAP_L)?;
    if col < line_text[..cap].chars().count() {
        return None;
    }
    hunks
        .iter()
        .find(|h| h.header_line == line && (h.can_expand_up || h.can_expand_down))
        .map(|h| (h.first_group, h.last_group))
}

/// The index (in `changes`/dropdown order) of the file whose `diff --git`
/// separator is the last one at or before buffer `line` — i.e. the file the
/// combined-diff viewport is currently showing at its top. Scans the *live*
/// buffer rather than cached line numbers so it stays correct after edits shift
/// lines. Defaults to 0 when `line` precedes the first separator.
fn diff_file_index_at_line(buffer: &sourceview5::Buffer, line: usize) -> usize {
    let text = buffer_text(buffer);
    let mut idx = 0;
    let mut seen = 0usize;
    for (i, l) in text.split('\n').enumerate() {
        if i > line {
            break;
        }
        if l.starts_with("diff --git ") {
            idx = seen;
            seen += 1;
        }
    }
    idx
}

/// Index (in file order) of the conflict-view section containing buffer `line` —
/// the count of file headers at or before it, minus one. Scans the live buffer so
/// it survives edits. The conflict analogue of [`diff_file_index_at_line`].
fn conflict_file_index_at_line(buffer: &sourceview5::Buffer, line: usize) -> usize {
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
fn conflict_file_header_line(buffer: &sourceview5::Buffer, idx: usize) -> Option<usize> {
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
fn conflict_cue_gap_at(buffer: &sourceview5::Buffer, line: usize) -> Option<(usize, usize)> {
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

/// Buffer line of the `k`-th elision cue within file section `fi`, after a
/// re-render — used to re-pin the viewport on the cue the user just expanded.
fn conflict_section_cue_line(buffer: &sourceview5::Buffer, fi: usize, k: usize) -> Option<usize> {
    let cue = pill(CONFLICT_CUE_LABEL);
    let text = buffer_text(buffer);
    let mut cur_file: Option<usize> = None;
    let mut header_count = 0usize;
    let mut seen = 0usize;
    for (i, l) in text.split('\n').enumerate() {
        if conflict_header_path(l).is_some() {
            cur_file = Some(header_count);
            header_count += 1;
            seen = 0;
        } else if l == cue && cur_file == Some(fi) {
            if seen == k {
                return Some(i);
            }
            seen += 1;
        }
    }
    None
}

/// The text of buffer `line` (without its trailing newline).
fn buffer_line_text(buffer: &sourceview5::Buffer, line: usize) -> String {
    let Some(start) = buffer.iter_at_line(line as i32) else {
        return String::new();
    };
    let mut end = start;
    if !end.ends_line() {
        end.forward_to_line_end();
    }
    buffer.text(&start, &end, false).to_string()
}

/// Append the inline quick-resolve cue to each conflict-marker line of the
/// buffer (which must already hold the materialized conflict text). Inserting at
/// a line's end leaves line numbering unchanged, so the cached classification
/// stays valid across the loop. The caller must hold the `editing` guard so the
/// firewall lets these programmatic inserts through.
fn append_resolve_cues(buffer: &sourceview5::Buffer) {
    let text = buffer_text(buffer);
    for (li, kind) in classify_conflict_lines(&text).into_iter().enumerate() {
        let Some((cue, _)) = resolve_cue(kind) else {
            continue;
        };
        if let Some(mut iter) = buffer.iter_at_line(li as i32) {
            iter.forward_to_line_end();
            buffer.insert(&mut iter, cue);
        }
    }
}

/// Buffer line indices of the conflict-block openers (`<<<<<<<`) in the
/// currently-materialized conflict file, in document order — the scroll anchors
/// the previous/next-conflict navigation jumps between.
fn conflict_block_lines(buffer: &sourceview5::Buffer) -> Vec<usize> {
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
fn scroll_to_line(view: &sourceview5::View, buffer: &sourceview5::Buffer, line: usize) {
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
fn resolve_conflict_at(
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

/// Run a drop's staged action, scheduled from the drag source's `drag-end`.
///
/// The action rewrites history and rebuilds the list widgets, which unparents
/// the `GtkListBoxRow`s. If that happens while GTK still has drag-and-drop
/// crossing events queued for the just-finished gesture, GTK walks a row it
/// holds as the drop target after we've orphaned it (parent becomes NULL) and
/// segfaults. Scheduling at idle priority — below GDK's event priority — runs
/// the rebuild only once every pending crossing event has been drained, so the
/// rows are alive for all of them. (Scheduling from `drag-end` rather than the
/// drop handler matters too: an idle queued mid-gesture can fire between motion
/// events, i.e. before the drag is over.)
fn run_post_drag(post_drag: &Rc<RefCell<Option<Box<dyn FnOnce()>>>>) {
    if let Some(action) = post_drag.borrow_mut().take() {
        glib::idle_add_local_once(move || action());
    }
}

fn main() {
    let repo_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, repo_path.clone()));
    app.run_with_args::<&str>(&[]);
}

fn buffer_text(buffer: &sourceview5::Buffer) -> String {
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

/// Buffer iter at a structured-edit [`Cursor`] (line + character column).
fn iter_at(buffer: &sourceview5::Buffer, c: &Cursor) -> gtk::TextIter {
    buffer
        .iter_at_line_offset(c.line as i32, c.col as i32)
        .unwrap_or_else(|| buffer.end_iter())
}

/// The current selection (or a collapsed caret) as a structured-edit
/// [`Selection`].
fn buffer_selection(buffer: &sourceview5::Buffer) -> Selection {
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
fn apply_patch_edit(
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

/// The fraction of the visible height at which buffer `line`'s top currently
/// sits in `view` (0.0 = top edge, 1.0 = bottom). Used to keep a clicked hunk
/// header at the same place across a re-render. Falls back to a third down.
fn vertical_fraction_of_line(
    view: &sourceview5::View,
    buffer: &sourceview5::Buffer,
    line: usize,
) -> f64 {
    let Some(vadjustment) = view.vadjustment() else {
        return 0.3;
    };
    let page = vadjustment.page_size();
    if page <= 0.0 {
        return 0.3;
    }
    let Some(iter) = buffer.iter_at_line(line as i32) else {
        return 0.3;
    };
    let line_top = view.iter_location(&iter).y() as f64;
    ((line_top - vadjustment.value()) / page).clamp(0.0, 1.0)
}

fn change_label(change: &FileChange) -> String {
    let sigil = match change.kind {
        ChangeKind::Added => "+",
        ChangeKind::Modified => "~",
        ChangeKind::Removed => "-",
    };
    format!("{sigil} {}", change.path)
}

fn build_ui(app: &Application, repo_path: PathBuf) {
    let repo = match Repo::open(&repo_path) {
        Ok(repo) => Rc::new(RefCell::new(repo)),
        Err(err) => {
            present_error(app, &format!("Failed to open {repo_path:?}:\n{err:?}"));
            return;
        }
    };

    // Print the starting HEAD so the user has a recovery anchor: if a rewrite
    // ever messes up their working branch, `git reset --hard <id>` restores it.
    if let Some(head) = repo.borrow().head_commit_hex() {
        println!("commedit: use `git reset --hard {head}` to undo this session");
    }

    // Shared UI state.
    let commits: Rc<RefCell<Vec<CommitInfo>>> = Rc::new(RefCell::new(Vec::new()));
    // How many history rows the normal (non-conflict) view currently loads, and
    // whether older commits remain below them. `refresh` reads the limit and sets
    // the flag; scrolling near the bottom bumps the limit by `HISTORY_PAGE`.
    let history_limit: Rc<Cell<usize>> = Rc::new(Cell::new(HISTORY_PAGE));
    let history_has_more: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let selected_change: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let changes: Rc<RefCell<Vec<FileChange>>> = Rc::new(RefCell::new(Vec::new()));
    // The file the dropdown points at / the diff is scrolled to. Used for the
    // post-save cursor restore and as the scroll-jump target.
    let current_file: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    // Placement of each file within the combined diff buffer (from the last
    // render): drives dropdown↔scroll navigation, expand-click file mapping, and
    // per-file editability.
    let combined_files: Rc<RefCell<Vec<CombinedFile>>> = Rc::new(RefCell::new(Vec::new()));
    // Guards the dropdown↔scroll feedback loop: set while one side programmatically
    // drives the other so the reaction doesn't bounce back.
    let nav_sync: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // Per-file hunk context expansion, keyed by path. Reset when the selected
    // commit changes (see `load_changes`).
    let expansions: Rc<RefCell<HashMap<String, ContextExpansion>>> =
        Rc::new(RefCell::new(HashMap::new()));
    // Commits dropped to the trash this session, newest drop last. They are no
    // longer on the branch but their objects survive, so they can be dragged back
    // into history to restore them (see `Repo::restore_commit`).
    let trashed: Rc<RefCell<Vec<CommitInfo>>> = Rc::new(RefCell::new(Vec::new()));
    // Which list the in-flight drag started in, set on drag prepare.
    let drag_origin: Rc<Cell<DragOrigin>> = Rc::new(Cell::new(DragOrigin::History));
    // Whether the diff pane is showing a normal diff or a conflict to resolve.
    let pane_mode: Rc<RefCell<PaneMode>> = Rc::new(RefCell::new(PaneMode::Diff));
    // Per-file state of the combined conflict-snippet buffer for the selected
    // commit (rebuilt by `load_conflict_files`, in dropdown/file order).
    let conflict_view: Rc<RefCell<Vec<ConflictFileView>>> = Rc::new(RefCell::new(Vec::new()));
    // Whether the read-only working-copy (@) row is the current selection, in
    // which case the diff is shown read-only and Save is inert.
    let viewing_wc: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // Styling for drag-and-drop reordering: the insertion gap placeholder and the
    // dimmed row being dragged. Installed once for the display.
    if let Some(display) = gdk::Display::default() {
        let css = gtk::CssProvider::new();
        css.load_from_data(
            ".drop-placeholder { background-color: rgba(53, 132, 228, 0.22); \
             border: 1px dashed rgb(53, 132, 228); border-radius: 5px; margin: 1px 6px; } \
             row.commit-dragging { opacity: 0.35; } \
             row.squash-recommended { background-color: rgba(46, 194, 126, 0.18); \
             border: 1px dashed rgb(46, 194, 126); border-radius: 5px; } \
             row.squash-sibling { background-color: rgba(245, 194, 17, 0.18); \
             border: 1px dashed rgb(245, 194, 17); border-radius: 5px; } \
             row.squash-drop-target { background-color: rgba(224, 27, 36, 0.38); \
             border: 1px solid rgb(224, 27, 36); border-radius: 5px; }",
        );
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // --- History pane (left) ---
    // The working-copy row: a read-only entry above the history showing the
    // uncommitted changes (jj's `@` commit). It is its own single-row list — not
    // part of the history `list` — so the reorder/drop/squash index arithmetic
    // and drag wiring below are untouched, and it can never be dragged or
    // reordered. Hidden while the tree is clean.
    let wc_label = gtk::Label::new(None);
    wc_label.set_halign(gtk::Align::Start);
    wc_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    wc_label.set_margin_start(8);
    wc_label.set_margin_end(8);
    wc_label.set_margin_top(4);
    wc_label.set_margin_bottom(4);
    let wc_row = ListBoxRow::new();
    wc_row.set_child(Some(&wc_label));
    let wc_list = ListBox::new();
    wc_list.append(&wc_row);
    wc_list.set_visible(false);
    wc_list.set_tooltip_text(Some(
        "Uncommitted working-tree changes — edit the diff here (Save writes the working \
         tree); preserved across rewrites",
    ));

    let list = ListBox::new();
    let history_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vexpand(true)
        .width_request(480)
        .child(&list)
        .build();

    // The trash panel: a short, always-visible list at the bottom of the history
    // pane. Dropping a commit here removes it from the branch; it stays listed so
    // it can be dragged back into the history above to restore it.
    let trash_list = ListBox::new();
    // Size the panel to its contents (so it grows as commits pile up) but cap it
    // so a full trash can never swallow the history list above; past the cap it
    // scrolls.
    let trash_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(280)
        .child(&trash_list)
        .build();
    let trash_header = gtk::Image::from_icon_name("user-trash-symbolic");
    trash_header.set_halign(gtk::Align::Start);
    trash_header.set_margin_start(8);
    trash_header.set_margin_end(8);
    trash_header.set_margin_top(4);
    trash_header.set_margin_bottom(2);
    trash_header.set_tooltip_text(Some("Trash — drop commits here to remove them"));
    trash_header.add_css_class("dim-label");
    let trash_box = GtkBox::new(Orientation::Vertical, 0);
    trash_box.append(&gtk::Separator::new(Orientation::Horizontal));
    trash_box.append(&trash_header);
    trash_box.append(&trash_scroll);

    let history_box = GtkBox::new(Orientation::Vertical, 0);
    history_box.append(&wc_list);
    history_box.append(&history_scroll);
    history_box.append(&trash_box);

    // --- Message pane (top-right) ---
    let message_buffer = sourceview5::Buffer::new(None);
    let message_view = sourceview5::View::with_buffer(&message_buffer);
    message_view.set_monospace(true);
    message_view.set_wrap_mode(gtk::WrapMode::WordChar);
    message_view.set_left_margin(8);
    message_view.set_top_margin(8);

    // Identity fields above the message editor: one combined "Name <email>"
    // field per role (with a built-in ▼ to pick an identity used elsewhere) and
    // a date field with a calendar button to its right.
    let author_id = identity_entry("Author — Name <email>");
    let author_date = identity_entry("YYYY-MM-DD HH:MM:SS ±HHMM");
    let committer_id = identity_entry("Committer — Name <email>");
    let committer_date = identity_entry("YYYY-MM-DD HH:MM:SS ±HHMM");
    author_date.set_width_chars(26);
    committer_date.set_width_chars(26);
    // Distinct identities harvested from history, offered by the in-field ▼.
    let identities: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    attach_identity_picker(&author_id, &identities);
    attach_identity_picker(&committer_id, &identities);

    let identity_grid = Grid::builder()
        .row_spacing(4)
        .column_spacing(6)
        .margin_start(8)
        .margin_top(8)
        .margin_end(8)
        .margin_bottom(4)
        .build();
    let author_label = Label::builder().label("Author").xalign(0.0).build();
    let committer_label = Label::builder().label("Committer").xalign(0.0).build();
    identity_grid.attach(&author_label, 0, 0, 1, 1);
    identity_grid.attach(&author_id, 1, 0, 1, 1);
    identity_grid.attach(&date_field(&author_date), 2, 0, 1, 1);
    identity_grid.attach(&committer_label, 0, 1, 1, 1);
    identity_grid.attach(&committer_id, 1, 1, 1, 1);
    identity_grid.attach(&date_field(&committer_date), 2, 1, 1, 1);

    let message_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&message_view)
        .build();
    let message_box = GtkBox::new(Orientation::Vertical, 0);
    message_box.append(&identity_grid);
    message_box.append(&message_scroll);

    // The original identity of the selected commit, to detect edits on save.
    let original_identity: Rc<RefCell<Option<Identity>>> = Rc::new(RefCell::new(None));
    // The identity entries in a fixed order (see `read_identity`):
    // [author "Name <email>", author date, committer "Name <email>", committer date].
    let identity_fields: [Entry; 4] = [
        author_id.clone(),
        author_date.clone(),
        committer_id.clone(),
        committer_date.clone(),
    ];

    // --- File / diff pane (bottom-right) ---
    let file_dropdown = DropDown::from_strings(&[]);
    // We render the diff with our own text tags (line backgrounds, syntect
    // language coloring, intra-line emphasis) rather than a GtkSourceView
    // grammar, so no language is set on the buffer.
    let file_buffer = sourceview5::Buffer::new(None);
    install_diff_tags(&file_buffer);
    let file_view = sourceview5::View::with_buffer(&file_buffer);
    file_view.set_monospace(true);
    file_view.set_left_margin(8);
    file_view.set_top_margin(8);
    // Set while we mutate the diff buffer ourselves (loading a file, or applying
    // a structured edit) so the firewall signal handlers below let it through
    // instead of treating it as an interactive edit.
    let editing = Rc::new(Cell::new(false));
    let file_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&file_view)
        .build();
    let files_box = GtkBox::new(Orientation::Vertical, 0);
    file_dropdown.set_margin_start(8);
    // The vertical Paned overlays its ~9px resize handle on top of the bottom
    // pane's top edge, and the handle's drag gesture (capture phase, on the Paned
    // ancestor) claims presses over that band before they reach this dropdown's
    // toggle button. Flush against the handle, the dropdown's top was swallowed —
    // clicks there never opened it. Push the dropdown clear of the handle band.
    file_dropdown.set_margin_top(14);
    file_dropdown.set_margin_end(8);
    file_dropdown.set_margin_bottom(4);
    // Transient feedback line for blocked edits and save errors. It lives at the
    // left of the action bar below, so the message shows inline beside the Save
    // button. It ellipsizes rather than expands: a flexible spacer (not the
    // label) holds the slack, so the Save button stays pinned right whether or
    // not the label is currently visible.
    let status_label = Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .valign(gtk::Align::Center)
        .build();
    status_label.add_css_class("dim-label");
    status_label.set_visible(false);

    // Action bar along the bottom of the file pane. The Save button is
    // right-aligned behind a flexible spacer; the status label sits to the left
    // of that spacer. Living inside `files_box` keeps it only as wide as the
    // file/diff editing field rather than spanning the whole window.
    let save_button = Button::with_label("Save");
    save_button.add_css_class("suggested-action");
    save_button.set_tooltip_text(Some(SAVE_HINT_DIFF));
    // Sits left of Save. Splits the selected commit into the edited diff plus a
    // follow-up "Split of …" commit; enabled only while the diff has pending edits
    // (wired by `update_split_sensitivity`), never in the conflict/working-copy views.
    let split_button = Button::with_label("Split");
    split_button.set_tooltip_text(Some(SPLIT_HINT));
    split_button.set_sensitive(false);
    // Conflict-mode quick resolution is driven inline: clicking a block's marker
    // line (with its "use ours/theirs/both" cue) keeps that side — see
    // `append_resolve_cues` and the click gesture below. No toolbar buttons.
    // Previous/next-conflict navigation jumps the view between blocks; only
    // shown while resolving conflicts.
    let prev_conflict_button = Button::with_label("◀");
    prev_conflict_button.set_tooltip_text(Some("Scroll to the previous conflict"));
    prev_conflict_button.set_visible(false);
    let next_conflict_button = Button::with_label("▶");
    next_conflict_button.set_tooltip_text(Some("Scroll to the next conflict"));
    next_conflict_button.set_visible(false);
    let bottom_bar = GtkBox::new(Orientation::Horizontal, 4);
    bottom_bar.set_margin_start(8);
    bottom_bar.set_margin_end(8);
    bottom_bar.set_margin_top(4);
    bottom_bar.set_margin_bottom(8);
    let bottom_spacer = GtkBox::new(Orientation::Horizontal, 0);
    bottom_spacer.set_hexpand(true);
    bottom_bar.append(&status_label);
    bottom_bar.append(&bottom_spacer);
    bottom_bar.append(&prev_conflict_button);
    bottom_bar.append(&next_conflict_button);
    bottom_bar.append(&split_button);
    bottom_bar.append(&save_button);

    // A banner above the file list, shown only while a conflicted rewrite is held
    // back from git: it states the blocking condition and offers to abort the
    // whole rewrite. Conflicts are applied automatically once the last one is
    // resolved, so there is no explicit "finalize" button.
    let conflict_label = Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .wrap(true)
        .build();
    let abort_button = Button::with_label("Abort rewrite");
    abort_button.add_css_class("destructive-action");
    abort_button.set_tooltip_text(Some(ABORT_HINT));
    let conflict_banner = GtkBox::new(Orientation::Horizontal, 8);
    conflict_banner.add_css_class("error");
    conflict_banner.set_margin_start(8);
    conflict_banner.set_margin_end(8);
    conflict_banner.set_margin_top(4);
    conflict_banner.set_margin_bottom(4);
    let banner_icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
    conflict_banner.append(&banner_icon);
    conflict_banner.append(&conflict_label);
    conflict_banner.append(&abort_button);
    conflict_banner.set_visible(false);

    files_box.append(&conflict_banner);
    files_box.append(&file_dropdown);
    files_box.append(&file_scroll);
    files_box.append(&bottom_bar);

    // The file pane carries the Save action bar at its bottom. A Paned lets
    // either child be shrunk below its minimum by default, so dragging this
    // divider down (or shrinking the window) would squeeze `files_box` until its
    // bottom bar overflowed off the window edge — the Save button ending up
    // partly or wholly off-screen and unclickable. Pin the end child to its
    // minimum so the action bar always stays visible; the message pane (the
    // start child, still shrinkable) absorbs the slack instead.
    let right_paned = Paned::builder()
        .orientation(Orientation::Vertical)
        .start_child(&message_box)
        .end_child(&files_box)
        .position(200)
        .shrink_end_child(false)
        .build();

    let paned = Paned::builder()
        .orientation(Orientation::Horizontal)
        .start_child(&history_box)
        .end_child(&right_paned)
        .position(480)
        .build();

    // --- Review view (full-window, read-only session diff) ---
    // A second diff surface shown in place of the whole editor while the "Review"
    // toggle is on: the content delta between the current tree and the one the
    // session started with (see `Repo::session_changes`). Its own buffer so the
    // editable diff pane is left untouched; rendered on demand by `render_review`
    // below. Read-only — none of the diff pane's edit wiring applies here.
    let review_buffer = sourceview5::Buffer::new(None);
    install_diff_tags(&review_buffer);
    let review_view = sourceview5::View::with_buffer(&review_buffer);
    review_view.set_monospace(true);
    review_view.set_editable(false);
    review_view.set_left_margin(8);
    review_view.set_top_margin(8);
    let review_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&review_view)
        .build();

    // The editor and the review are mutually exclusive full-window pages; the
    // "Review" header toggle (wired below) flips between them.
    paned.set_vexpand(true);
    let content_stack = Stack::new();
    content_stack.add_named(&paned, Some("edit"));
    content_stack.add_named(&review_scroll, Some("review"));

    let root = GtkBox::new(Orientation::Vertical, 0);
    root.append(&content_stack);

    // The header bar keeps the window title and the window controls; the Save
    // action lives in the bottom action bar. The custom controls are the
    // top-right "Revert all" button (rolls the whole session back to the state
    // the repo was opened in) and a "Review" toggle that shows a read-only,
    // full-window diff of every content change made this session. Both are wired
    // below, once `refresh` & co. exist.
    let header = HeaderBar::new();
    let revert_button = Button::with_label("Revert all");
    revert_button.add_css_class("destructive-action");
    revert_button.set_tooltip_text(Some(
        "Discard all changes made this session and restore the repository to its original state",
    ));
    let review_button = ToggleButton::with_label("Review");
    review_button.set_tooltip_text(Some(
        "Review all content changes made this session (current tree vs. the session start)",
    ));
    // pack_end fills right-to-left, so packing "Revert all" first leaves "Review"
    // to its left: [ Review ][ Revert all ].
    header.pack_end(&revert_button);
    header.pack_end(&review_button);

    // Title with the repository folder name, e.g. "Commit editor - commedit".
    let folder = repo
        .borrow()
        .workspace
        .workspace_root()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "commedit".to_string());
    let window = ApplicationWindow::builder()
        .application(app)
        .title(format!("Commit editor - {folder}"))
        .default_width(1400)
        .default_height(900)
        .child(&root)
        .build();
    window.set_titlebar(Some(&header));

    // Syntax-highlighting resources (loaded once). A light theme to sit on the
    // light diff line backgrounds, like GitHub/delta.
    let syntax_set = Rc::new(SyntaxSet::load_defaults_newlines());
    let theme: Rc<Theme> = {
        let themes = ThemeSet::load_defaults().themes;
        let chosen = themes
            .get("InspiredGitHub")
            .or_else(|| themes.values().next())
            .cloned()
            .expect("at least one default theme");
        Rc::new(chosen)
    };

    // Fold any `-X`/`+X` pair the user has undone (edited a `+` line back to
    // equal its `-` line) into a single context line. The caret is moved onto the
    // merged line at the same column, so resuming the edit re-splits it there. The
    // rewrite is guarded so the firewall treats it as our own.
    //
    // We splice in only the span that actually changed (a localized delete+insert)
    // rather than `set_text`-ing the whole buffer: `set_text` resets the scroll to
    // the top, and re-pinning it afterwards fights GTK's deferred layout validation
    // and loses. An in-place edit near the (visible) caret leaves the view exactly
    // where it is, the same way ordinary typing does.
    let collapse: Rc<dyn Fn()> = {
        let file_buffer = file_buffer.clone();
        let editing = editing.clone();
        let pane_mode = pane_mode.clone();
        Rc::new(move || {
            // Collapsing -X/+X pairs is a unified-diff notion; in conflict mode the
            // buffer is whole-file content, so there is nothing to fold.
            if pane_mode.borrow().is_conflict() {
                return;
            }
            let text = buffer_text(&file_buffer);
            let it = file_buffer.iter_at_offset(file_buffer.cursor_position());
            let cursor = Cursor {
                line: it.line() as usize,
                col: it.line_offset() as usize,
            };
            let Some((collapsed, new_cursor)) = collapse_diff(&text, cursor) else {
                return;
            };
            // Reduce the whole-text change to its minimal differing span: the
            // common leading/trailing characters are untouched, so we only delete
            // and re-insert the lines that actually folded.
            let old: Vec<char> = text.chars().collect();
            let new: Vec<char> = collapsed.chars().collect();
            let mut head = 0;
            while head < old.len() && head < new.len() && old[head] == new[head] {
                head += 1;
            }
            let mut tail = 0;
            while tail < old.len() - head
                && tail < new.len() - head
                && old[old.len() - 1 - tail] == new[new.len() - 1 - tail]
            {
                tail += 1;
            }
            let middle: String = new[head..new.len() - tail].iter().collect();

            editing.set(true);
            file_buffer.begin_user_action();
            let mut start = file_buffer.iter_at_offset(head as i32);
            let mut end = file_buffer.iter_at_offset((old.len() - tail) as i32);
            file_buffer.delete(&mut start, &mut end);
            let mut at = file_buffer.iter_at_offset(head as i32);
            file_buffer.insert(&mut at, &middle);
            file_buffer.end_user_action();
            editing.set(false);

            let caret = iter_at(&file_buffer, &new_cursor);
            file_buffer.place_cursor(&caret);
        })
    };

    // Re-render the diff highlighting for whatever is currently in the buffer.
    let highlight: Rc<dyn Fn()> = {
        let file_buffer = file_buffer.clone();
        let current_file = current_file.clone();
        let syntax_set = syntax_set.clone();
        let theme = theme.clone();
        let pane_mode = pane_mode.clone();
        Rc::new(move || {
            let path = current_file.borrow().clone();
            if pane_mode.borrow().is_conflict() {
                highlight_conflict(&file_buffer, path.as_deref(), &syntax_set, &theme);
            } else {
                highlight_diff(&file_buffer, path.as_deref(), &syntax_set, &theme);
            }
        })
    };

    // Render the current file's diff into the buffer with its per-hunk context
    // expansion, appending a click-to-expand cue to each expandable `@@` header.
    // Self-referential (via `render_cell`) so a click can request a re-render
    // after widening a hunk.
    let render_cell: Rc<RefCell<Option<Renderer>>> = Rc::new(RefCell::new(None));
    // Hunks of the diff currently in the buffer, so an expand click can scroll
    // its (now re-rendered) hunk back into view instead of jumping to the top.
    let rendered_hunks: Rc<RefCell<Vec<HunkInfo>>> = Rc::new(RefCell::new(Vec::new()));
    // The conflict pane's "expand hidden lines" action, late-bound (it needs the
    // conflict renderer defined below). The expand-click gesture invokes it by
    // buffer line, mirroring `render_cell` for the diff pane.
    let conflict_expand_cell: Rc<RefCell<Option<Rc<dyn Fn(usize)>>>> =
        Rc::new(RefCell::new(None));
    let render_diff_view: Renderer = {
        let changes = changes.clone();
        let combined_files = combined_files.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let editing = editing.clone();
        let expansions = expansions.clone();
        let rendered_hunks = rendered_hunks.clone();
        let highlight = highlight.clone();
        Rc::new(move || {
            // Render the whole change — every file's diff in one buffer, files
            // separated by `diff --git` lines — rather than one file at a time.
            let combined = render_commit_diff(&changes.borrow(), &expansions.borrow());
            editing.set(true);
            file_buffer.set_text(&combined.text);
            // Append a click-to-expand cue to each expandable @@ header, across all
            // files (header lines are already mapped to the combined text). Inserts
            // at a line's end so line numbering — and the cached `header_line`s —
            // stay valid. The click is handled by a GestureClick on the view; we
            // must not embed a real widget, since removing it on the next set_text
            // crashes GTK.
            let mut all_hunks: Vec<HunkInfo> = Vec::new();
            for file in &combined.files {
                for hunk in &file.hunks {
                    let label = match (hunk.can_expand_up, hunk.can_expand_down) {
                        (true, true) => "↕ expand context",
                        (true, false) => "↑ expand context",
                        (false, true) => "↓ expand context",
                        (false, false) => {
                            all_hunks.push(hunk.clone());
                            continue;
                        }
                    };
                    if let Some(mut iter) = file_buffer.iter_at_line(hunk.header_line as i32) {
                        iter.forward_to_line_end();
                        file_buffer.insert(&mut iter, &format!("  {}", pill(label)));
                    }
                    all_hunks.push(hunk.clone());
                }
            }
            file_view.set_editable(combined.files.iter().any(|f| f.editable));
            *rendered_hunks.borrow_mut() = all_hunks;
            *combined_files.borrow_mut() = combined.files;
            // Highlight in this same main-loop turn, before GTK paints, so the
            // diff appears once fully colored instead of flashing plain first and
            // then re-highlighting via the debounced `changed` handler (which is
            // suppressed below while `editing` is set).
            highlight();
            editing.set(false);
        })
    };
    *render_cell.borrow_mut() = Some(render_diff_view.clone());

    // Clicking a @@ header line (anywhere on it, including the "expand context"
    // cue) widens that hunk's context. In conflict mode the same gesture turns a
    // click on a block's marker line into the matching quick resolution. The
    // mutation is deferred to an idle so it runs outside the gesture's event
    // handling.
    let expand_click = gtk::GestureClick::new();
    expand_click.set_button(gdk::BUTTON_PRIMARY);
    expand_click.set_propagation_phase(PropagationPhase::Capture);
    expand_click.connect_pressed({
        let file_view = file_view.clone();
        let file_buffer = file_buffer.clone();
        let rendered_hunks = rendered_hunks.clone();
        let expansions = expansions.clone();
        let render_cell = render_cell.clone();
        let combined_files = combined_files.clone();
        let pane_mode = pane_mode.clone();
        let editing = editing.clone();
        let highlight = highlight.clone();
        let nav_sync = nav_sync.clone();
        let conflict_expand_cell = conflict_expand_cell.clone();
        move |gesture, _n_press, x, y| {
            let (bx, by) = file_view.window_to_buffer_coords(
                gtk::TextWindowType::Widget,
                x as i32,
                y as i32,
            );
            let Some(iter) = file_view.iter_at_location(bx, by) else {
                return;
            };
            let line = iter.line() as usize;
            // Conflict mode: a click on an elision cue expands that gap; a click on
            // a marker line's inline "➜ use …" cue resolves that block. Clicks
            // elsewhere fall through so the caret places for free-form edits.
            if pane_mode.borrow().is_conflict() {
                let line_text = buffer_line_text(&file_buffer, line);
                if line_text == pill(CONFLICT_CUE_LABEL) {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    if let Some(expand) = conflict_expand_cell.borrow().clone() {
                        glib::idle_add_local_once(move || expand(line));
                    }
                    return;
                }
                let text = buffer_text(&file_buffer);
                let col = iter.line_offset() as usize;
                let Some(side) = conflict_cue_side_at(&text, line, col) else {
                    return;
                };
                // We own this click: don't let the view also place the caret in
                // the marker line we're about to delete.
                gesture.set_state(gtk::EventSequenceState::Claimed);
                let file_buffer = file_buffer.clone();
                let editing = editing.clone();
                let highlight = highlight.clone();
                glib::idle_add_local_once(move || {
                    resolve_conflict_at(&file_buffer, &editing, line, side, &*highlight);
                });
                return;
            }
            // Only the inline pill cue is clickable, not the whole @@ header.
            let col = iter.line_offset() as usize;
            let line_text = buffer_line_text(&file_buffer, line);
            let hit = expand_cue_at(&rendered_hunks.borrow(), &line_text, line, col);
            let Some((first, last)) = hit else { return };
            // The combined diff holds several files; find which one owns the
            // clicked hunk so we widen *its* per-path expansion (group indices are
            // file-relative).
            let path = combined_files
                .borrow()
                .iter()
                .find(|f| f.hunks.iter().any(|h| h.header_line == line))
                .map(|f| f.path.clone());
            let Some(path) = path else { return };
            // We own this click: don't let the view also place the caret.
            gesture.set_state(gtk::EventSequenceState::Claimed);

            // Record where the clicked header sits in the viewport now, so that
            // after re-rendering (which resets the scroll and shifts lines down
            // as context appears above) we can pin that same header back to the
            // same spot — expansion then grows around it instead of jumping.
            let frac = vertical_fraction_of_line(&file_view, &file_buffer, line);
            // The view is monospace and never wraps, so every line is the same
            // height. Measure it now (layout is valid) to compute the post-render
            // scroll offset arithmetically — see the idle below.
            let line_height = file_view
                .iter_location(&file_buffer.start_iter())
                .height() as f64;

            let expansions = expansions.clone();
            let render_cell = render_cell.clone();
            let combined_files = combined_files.clone();
            let file_buffer = file_buffer.clone();
            let file_view = file_view.clone();
            let nav_sync = nav_sync.clone();
            glib::idle_add_local_once(move || {
                // The re-render's set_text resets the scroll to the top and we then
                // re-pin it; guard so the transient doesn't flip the dropdown.
                nav_sync.set(true);
                expansions
                    .borrow_mut()
                    .entry(path.clone())
                    .or_default()
                    .expand(first, last);
                if let Some(render) = render_cell.borrow().clone() {
                    render();
                }
                // Pin the (possibly moved or merged) hunk header to its prior
                // viewport position. set_text reset the scroll to the top; the
                // deferred scrollers (scroll_to_mark / scroll_to_iter) only run on
                // a later frame, so the top is painted first and *then* corrected —
                // the visible jump-to-top flash. Instead, set the scroll offset
                // synchronously here, before GTK paints, so the next paint already
                // shows the final position. Line height is uniform (monospace, no
                // wrap), so the header's offset is just `line * line_height`, and
                // the document height is `lines * line_height + margins`. We set
                // the adjustment's upper too, so set_value isn't clamped against
                // the stale (pre-render) range; GTK's own validation later sets the
                // same values, leaving the position unchanged. The hunk is found
                // scoped to this file, since group indices repeat across files in
                // the combined buffer.
                let header = combined_files
                    .borrow()
                    .iter()
                    .find(|f| f.path == path)
                    .and_then(|f| {
                        f.hunks
                            .iter()
                            .find(|h| h.first_group <= first && last <= h.last_group)
                            .map(|h| h.header_line)
                    });
                if let (Some(line), Some(vadj)) = (header, file_view.vadjustment()) {
                    let page = vadj.page_size();
                    if line_height > 0.0 && page > 0.0 {
                        let top = file_view.top_margin() as f64;
                        let bottom = file_view.bottom_margin() as f64;
                        let height = file_buffer.line_count() as f64 * line_height + top + bottom;
                        let upper = height.max(page);
                        let target = (line as f64 * line_height + top - frac * page)
                            .clamp(0.0, (upper - page).max(0.0));
                        vadj.set_upper(upper);
                        vadj.set_value(target);
                        // Keep the cursor on the now-visible header so GTK's
                        // validation doesn't scroll the (offset-0) caret back into
                        // view and undo the offset we just set.
                        if let Some(iter) = file_buffer.iter_at_line(line as i32) {
                            file_buffer.place_cursor(&iter);
                        }
                    }
                }
                nav_sync.set(false);
            });
        }
    });
    file_view.add_controller(expand_click);

    // Hover cursor: show a hand over the clickable affordances — the conflict
    // "use …" buttons and the diff "expand context" pills — and the text I-beam
    // everywhere else. GtkTextView otherwise only ever shows the I-beam over
    // content; we override it per the gtk hypertext pattern (set the widget
    // cursor from the motion handler). A `Cell` tracks the current state so we
    // only touch the cursor when it actually flips.
    let hover_hand = Rc::new(Cell::new(false));
    let hover_motion = gtk::EventControllerMotion::new();
    hover_motion.connect_motion({
        let file_view = file_view.clone();
        let file_buffer = file_buffer.clone();
        let rendered_hunks = rendered_hunks.clone();
        let pane_mode = pane_mode.clone();
        let hover_hand = hover_hand.clone();
        move |_, x, y| {
            let (bx, by) =
                file_view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
            let over_button = file_view.iter_at_location(bx, by).is_some_and(|iter| {
                let line = iter.line() as usize;
                let col = iter.line_offset() as usize;
                if pane_mode.borrow().is_conflict() {
                    let line_text = buffer_line_text(&file_buffer, line);
                    line_text == pill(CONFLICT_CUE_LABEL)
                        || conflict_cue_side_at(&buffer_text(&file_buffer), line, col).is_some()
                } else {
                    let line_text = buffer_line_text(&file_buffer, line);
                    expand_cue_at(&rendered_hunks.borrow(), &line_text, line, col).is_some()
                }
            });
            if over_button != hover_hand.get() {
                hover_hand.set(over_button);
                file_view.set_cursor_from_name(Some(if over_button { "pointer" } else { "text" }));
            }
        }
    });
    file_view.add_controller(hover_motion);

    // Jump the (already-rendered) combined diff to the file at dropdown `idx`,
    // pinning its `diff --git` header to the top of the viewport. The whole change
    // is rendered once by `render_diff_view`; the dropdown is just a navigation
    // aid. Skips the scroll when `nav_sync` is set — i.e. when this selection was
    // itself driven by the scroll→dropdown sync, so the two don't fight.
    let scroll_to_file: Rc<dyn Fn(usize)> = {
        let combined_files = combined_files.clone();
        let current_file = current_file.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let nav_sync = nav_sync.clone();
        Rc::new(move |idx: usize| {
            let file = combined_files.borrow().get(idx).cloned();
            let Some(file) = file else { return };
            *current_file.borrow_mut() = Some(file.path.clone());
            if nav_sync.get() {
                return;
            }
            if let Some(mut iter) = file_buffer.iter_at_line(file.start_line as i32) {
                // yalign 0.0 pins the header to the top edge without moving the caret.
                file_view.scroll_to_iter(&mut iter, 0.0, true, 0.0, 0.0);
            }
        })
    };

    // Re-highlight after edits, debounced/coalesced so typing stays responsive.
    // (Applying tags does not emit `changed`, so this can't loop.)
    let highlight_gen = Rc::new(RefCell::new(0u64));
    // Light the Split button only when the diff carries pending file-content edits
    // (the same edits Save would apply, via `collect_file_edits`) — and never in
    // the conflict or working-copy views. Runs on every buffer change below, so it
    // also resets to insensitive after a (re)load renders a fresh, unedited diff.
    let update_split_sensitivity: Rc<dyn Fn()> = {
        let split_button = split_button.clone();
        let file_buffer = file_buffer.clone();
        let changes = changes.clone();
        let pane_mode = pane_mode.clone();
        let viewing_wc = viewing_wc.clone();
        Rc::new(move || {
            let has_edits = !pane_mode.borrow().is_conflict()
                && !viewing_wc.get()
                && matches!(
                    collect_file_edits(&buffer_text(&file_buffer), &changes.borrow()),
                    Ok(edits) if !edits.is_empty()
                );
            split_button.set_sensitive(has_edits);
        })
    };

    file_buffer.connect_changed({
        let collapse = collapse.clone();
        let highlight = highlight.clone();
        let highlight_gen = highlight_gen.clone();
        let editing = editing.clone();
        let update_split_sensitivity = update_split_sensitivity.clone();
        move |_| {
            // Track Split-button sensitivity on every change, including programmatic
            // renders (a load leaves an unedited diff -> insensitive).
            update_split_sensitivity();
            // A full programmatic render highlights itself synchronously; don't
            // also schedule a redundant (and flash-inducing) debounced pass.
            if editing.get() {
                return;
            }
            let mine = {
                let mut g = highlight_gen.borrow_mut();
                *g = g.wrapping_add(1);
                *g
            };
            let collapse = collapse.clone();
            let highlight = highlight.clone();
            let highlight_gen = highlight_gen.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
                if *highlight_gen.borrow() == mine {
                    // Fold any undone change first, then highlight the result.
                    collapse();
                    highlight();
                }
            });
        }
    });

    // Show a message in the status line for a few seconds, then clear it (a
    // generation counter coalesces rapid messages so only the latest clears).
    let status_gen = Rc::new(RefCell::new(0u64));
    let show_status: Rc<dyn Fn(&str)> = {
        let status_label = status_label.clone();
        let status_gen = status_gen.clone();
        Rc::new(move |msg: &str| {
            status_label.set_text(msg);
            status_label.set_visible(true);
            let mine = {
                let mut g = status_gen.borrow_mut();
                *g = g.wrapping_add(1);
                *g
            };
            let status_label = status_label.clone();
            let status_gen = status_gen.clone();
            glib::timeout_add_local_once(std::time::Duration::from_secs(4), move || {
                if *status_gen.borrow() == mine {
                    status_label.set_text("");
                    status_label.set_visible(false);
                }
            });
        })
    };

    // Render every conflicted file of the selected commit into the one buffer:
    // each file's section is a header line then its conflict *snippets* (the
    // `<<< … >>>` blocks with context, the unconflicted runs elided behind a cue),
    // or a notice for a structural conflict. Records per file the pieces and gaps
    // for reconstruction / expansion. The whole change is rendered once; the
    // dropdown is a jump aid, just like the diff pane.
    let render_conflict_view: Rc<dyn Fn()> = {
        let conflict_view = conflict_view.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let editing = editing.clone();
        let highlight = highlight.clone();
        Rc::new(move || {
            let cue = pill(CONFLICT_CUE_LABEL);
            let mut out: Vec<String> = Vec::new();
            {
                let mut view = conflict_view.borrow_mut();
                for fv in view.iter_mut() {
                    out.push(conflict_header_line(&fv.path));
                    if !fv.resolvable {
                        out.push(CONFLICT_STRUCTURAL_NOTICE.to_string());
                        fv.pieces.clear();
                        fv.gaps.clear();
                        continue;
                    }
                    let snip = render_conflict_snippets(&fv.full_text, &fv.exp, &cue);
                    for l in snip.text.split('\n') {
                        out.push(l.to_string());
                    }
                    fv.gaps = snip.gaps.iter().map(|g| (g.above, g.below)).collect();
                    fv.pieces = snip.pieces;
                }
            }
            editing.set(true);
            file_buffer.set_text(&out.join("\n"));
            // Inline "use ours/theirs/both" cues on each conflict block's markers.
            append_resolve_cues(&file_buffer);
            editing.set(false);
            file_view.set_editable(conflict_view.borrow().iter().any(|fv| fv.resolvable));
            highlight();
        })
    };

    // Refresh each resolvable file's full conflict text from the (edited) buffer,
    // so a re-render or save reflects edits made since the last render. Splits the
    // buffer into per-file sections by header line, then reconstructs each from its
    // shown snippets + the verbatim elided runs recorded in `pieces`.
    let sync_conflict_from_buffer: Rc<dyn Fn()> = {
        let conflict_view = conflict_view.clone();
        let file_buffer = file_buffer.clone();
        Rc::new(move || {
            let cue = pill(CONFLICT_CUE_LABEL);
            let combined = buffer_text(&file_buffer);
            let buf_lines: Vec<&str> = combined.split('\n').collect();
            let mut sections: Vec<Vec<&str>> = Vec::new();
            for &l in &buf_lines {
                if conflict_header_path(l).is_some() {
                    sections.push(Vec::new());
                } else if let Some(cur) = sections.last_mut() {
                    cur.push(l);
                }
            }
            let mut view = conflict_view.borrow_mut();
            for (fv, section) in view.iter_mut().zip(sections.iter()) {
                if fv.resolvable {
                    // Drop the inline resolve cues we appended to marker lines, so
                    // they don't accrete into the reconstructed text on re-render.
                    let cleaned: Vec<String> = section.iter().map(|l| strip_marker_cue(l)).collect();
                    let refs: Vec<&str> = cleaned.iter().map(String::as_str).collect();
                    fv.full_text = reconstruct_conflict_file(&refs, &fv.pieces, &cue);
                }
            }
        })
    };

    // Jump the (already-rendered) combined conflict buffer to the file at dropdown
    // `idx`, pinning its header to the top. The conflict analogue of
    // `scroll_to_file`; skips the scroll under `nav_sync` (scroll→dropdown sync).
    let scroll_to_conflict_file: Rc<dyn Fn(usize)> = {
        let conflict_view = conflict_view.clone();
        let current_file = current_file.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let nav_sync = nav_sync.clone();
        Rc::new(move |idx: usize| {
            let path = conflict_view.borrow().get(idx).map(|fv| fv.path.clone());
            let Some(path) = path else { return };
            *current_file.borrow_mut() = Some(path);
            if nav_sync.get() {
                return;
            }
            if let Some(line) = conflict_file_header_line(&file_buffer, idx) {
                if let Some(mut iter) = file_buffer.iter_at_line(line as i32) {
                    file_view.scroll_to_iter(&mut iter, 0.0, true, 0.0, 0.0);
                }
            }
        })
    };

    // Expand the elided gap at the clicked cue `line`: capture any buffer edits,
    // widen that gap's context, re-render, and re-pin the cue (or the file header)
    // to where it sat — the conflict-pane analogue of the diff expand handler.
    let conflict_expand: Rc<dyn Fn(usize)> = {
        let conflict_view = conflict_view.clone();
        let sync_conflict_from_buffer = sync_conflict_from_buffer.clone();
        let render_conflict_view = render_conflict_view.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let nav_sync = nav_sync.clone();
        Rc::new(move |line: usize| {
            let Some((fi, k)) = conflict_cue_gap_at(&file_buffer, line) else {
                return;
            };
            // Record the clicked cue's viewport position to re-pin it afterwards.
            let frac = vertical_fraction_of_line(&file_view, &file_buffer, line);
            let line_height = file_view.iter_location(&file_buffer.start_iter()).height() as f64;
            nav_sync.set(true);
            sync_conflict_from_buffer();
            {
                let mut view = conflict_view.borrow_mut();
                if let Some(fv) = view.get_mut(fi) {
                    if let Some(&(above, below)) = fv.gaps.get(k) {
                        fv.exp.expand_gap(above, below);
                    }
                }
            }
            render_conflict_view();
            // Re-pin to the gap's new cue line (or the file header if it merged).
            let new_line = conflict_section_cue_line(&file_buffer, fi, k)
                .or_else(|| conflict_file_header_line(&file_buffer, fi));
            if let (Some(nl), Some(vadj)) = (new_line, file_view.vadjustment()) {
                let page = vadj.page_size();
                if line_height > 0.0 && page > 0.0 {
                    let top = file_view.top_margin() as f64;
                    let bottom = file_view.bottom_margin() as f64;
                    let height = file_buffer.line_count() as f64 * line_height + top + bottom;
                    let upper = height.max(page);
                    let target = (nl as f64 * line_height + top - frac * page)
                        .clamp(0.0, (upper - page).max(0.0));
                    vadj.set_upper(upper);
                    vadj.set_value(target);
                    if let Some(iter) = file_buffer.iter_at_line(nl as i32) {
                        file_buffer.place_cursor(&iter);
                    }
                }
            }
            nav_sync.set(false);
        })
    };
    *conflict_expand_cell.borrow_mut() = Some(conflict_expand);

    // Build the conflict-view state for `commit`'s conflicted files, fill the
    // dropdown, render the combined snippet buffer, and land on the first conflict.
    let load_conflict_files: Rc<dyn Fn(&CommitInfo)> = {
        let repo = repo.clone();
        let pane_mode = pane_mode.clone();
        let conflict_view = conflict_view.clone();
        let file_dropdown = file_dropdown.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let editing = editing.clone();
        let current_file = current_file.clone();
        let render_conflict_view = render_conflict_view.clone();
        let scroll_to_conflict_file = scroll_to_conflict_file.clone();
        Rc::new(move |commit: &CommitInfo| {
            let files: Vec<(String, String, bool)> = {
                let mode = pane_mode.borrow();
                let PaneMode::Conflict(ctx) = &*mode else {
                    return;
                };
                ctx.commits
                    .iter()
                    .find(|c| c.change_id_hex() == commit.change_id_hex())
                    .map(|c| {
                        c.files
                            .iter()
                            .map(|f| (c.change_id_hex(), f.path_str(), f.resolvable))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            // Materialize each resolvable file's full conflict text up front.
            let mut view = Vec::new();
            for (change_hex, path, resolvable) in &files {
                let (full_text, marker_len) = if *resolvable {
                    match repo.borrow().read_conflict(change_hex, path) {
                        Ok(cf) => (cf.text, cf.marker_len),
                        Err(_) => (String::new(), 7),
                    }
                } else {
                    (String::new(), 7)
                };
                view.push(ConflictFileView {
                    path: path.clone(),
                    resolvable: *resolvable,
                    marker_len,
                    full_text,
                    exp: ContextExpansion::default(),
                    pieces: Vec::new(),
                    gaps: Vec::new(),
                });
            }
            let labels: Vec<String> = view
                .iter()
                .map(|fv| {
                    let mark = if fv.resolvable { "" } else { "⚠ " };
                    format!("{mark}{}", fv.path)
                })
                .collect();
            *conflict_view.borrow_mut() = view;

            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            file_dropdown.set_model(Some(&StringList::new(&refs)));
            if labels.is_empty() {
                *current_file.borrow_mut() = None;
                editing.set(true);
                file_buffer.set_text("(no remaining conflicts in this commit)");
                editing.set(false);
                file_view.set_editable(false);
                return;
            }
            // Render all files' snippets at once, then land on the first conflict.
            render_conflict_view();
            file_dropdown.set_selected(0);
            scroll_to_conflict_file(0);
            if let Some(&first) = conflict_block_lines(&file_buffer).first() {
                scroll_to_line(&file_view, &file_buffer, first);
            }
        })
    };

    const READ_ONLY_HINT: &str = "Edit blocked — this change would break the patch structure.";
    const CONFLICT_LAYOUT_HINT: &str =
        "Edit blocked — this line is part of the conflict view layout. Edit within a snippet.";

    // Firewall: every interactive mutation of the diff buffer goes through the
    // structured-edit planner so it can never produce a patch that fails to
    // apply. Programmatic loads/edits set the `editing` guard and pass straight
    // through. `insert-text` covers typing and paste; `delete-range` covers cut,
    // drag and selection deletes.
    file_buffer.connect_insert_text({
        let editing = editing.clone();
        let show_status = show_status.clone();
        let highlight = highlight.clone();
        let pane_mode = pane_mode.clone();
        move |buffer, iter, text| {
            if editing.get() {
                return;
            }
            // Conflict resolution is free-form *within* snippets — the unified-diff
            // firewall doesn't apply — but the view's structural lines (file
            // headers, elision cues, notices) must stay intact so the snippet→full
            // reconstruction keeps its anchors.
            if pane_mode.borrow().is_conflict() {
                if is_conflict_protected_line(&buffer_line_text(buffer, iter.line() as usize)) {
                    buffer.stop_signal_emission_by_name("insert-text");
                    show_status(CONFLICT_LAYOUT_HINT);
                }
                return;
            }
            let caret = Selection::caret(Cursor {
                line: iter.line() as usize,
                col: iter.line_offset() as usize,
            });
            match plan_edit(&buffer_text(buffer), caret, EditGesture::Insert(text.to_string())) {
                EditPlan::Allow => {}
                EditPlan::Block => {
                    buffer.stop_signal_emission_by_name("insert-text");
                    show_status(READ_ONLY_HINT);
                }
                EditPlan::Edit(edit) => {
                    buffer.stop_signal_emission_by_name("insert-text");
                    apply_patch_edit(buffer, &editing, &edit, &*highlight);
                }
            }
        }
    });
    file_buffer.connect_delete_range({
        let editing = editing.clone();
        let show_status = show_status.clone();
        let pane_mode = pane_mode.clone();
        move |buffer, start, end| {
            if editing.get() {
                return;
            }
            // Free-form within snippets, but block deletes that touch a structural
            // layout line (header / elision cue / notice).
            if pane_mode.borrow().is_conflict() {
                let touches_layout = (start.line()..=end.line())
                    .any(|li| is_conflict_protected_line(&buffer_line_text(buffer, li as usize)));
                if touches_layout {
                    buffer.stop_signal_emission_by_name("delete-range");
                    show_status(CONFLICT_LAYOUT_HINT);
                }
                return;
            }
            let s = Cursor {
                line: start.line() as usize,
                col: start.line_offset() as usize,
            };
            let e = Cursor {
                line: end.line() as usize,
                col: end.line_offset() as usize,
            };
            if !deletion_is_safe(&buffer_text(buffer), s, e) {
                buffer.stop_signal_emission_by_name("delete-range");
                show_status(READ_ONLY_HINT);
            }
        }
    });

    // Enter / Backspace / Delete carry structural intent that a plain
    // insert/delete can't express (split a line, un-remove a line, join added
    // lines), so handle them as gestures via the planner. Printable keys are left
    // alone so input methods keep working — they flow through `insert-text`. The
    // controller runs in the capture phase to pre-empt the view's own handling.
    let key_controller = EventControllerKey::new();
    key_controller.set_propagation_phase(PropagationPhase::Capture);
    key_controller.connect_key_pressed({
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let editing = editing.clone();
        let show_status = show_status.clone();
        let highlight = highlight.clone();
        let pane_mode = pane_mode.clone();
        move |_, keyval, _, state| {
            if !file_view.is_editable() {
                return glib::Propagation::Proceed;
            }
            // In conflict mode, structural diff gestures don't apply — let the view
            // handle Enter/Backspace/Delete as ordinary text editing.
            if pane_mode.borrow().is_conflict() {
                return glib::Propagation::Proceed;
            }
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let gesture = match keyval {
                gdk::Key::Return | gdk::Key::KP_Enter => EditGesture::Newline,
                gdk::Key::BackSpace => EditGesture::Backspace,
                gdk::Key::Delete | gdk::Key::KP_Delete => EditGesture::Delete,
                gdk::Key::d | gdk::Key::D if ctrl => EditGesture::DeleteLine,
                _ => return glib::Propagation::Proceed,
            };
            match plan_edit(&buffer_text(&file_buffer), buffer_selection(&file_buffer), gesture) {
                EditPlan::Allow => glib::Propagation::Proceed,
                EditPlan::Block => {
                    show_status(READ_ONLY_HINT);
                    glib::Propagation::Stop
                }
                EditPlan::Edit(edit) => {
                    apply_patch_edit(&file_buffer, &editing, &edit, &*highlight);
                    glib::Propagation::Stop
                }
            }
        }
    });
    file_view.add_controller(key_controller);

    file_dropdown.connect_selected_notify({
        let scroll_to_file = scroll_to_file.clone();
        let scroll_to_conflict_file = scroll_to_conflict_file.clone();
        let pane_mode = pane_mode.clone();
        move |dd| {
            let idx = dd.selected();
            if idx == gtk::INVALID_LIST_POSITION {
                return;
            }
            if pane_mode.borrow().is_conflict() {
                scroll_to_conflict_file(idx as usize);
            } else {
                scroll_to_file(idx as usize);
            }
        }
    });

    // Scrolling the diff view updates the dropdown to the file now at the top of
    // the viewport — the reverse of selecting a file to jump to it. Ignored while
    // we're mid-render (`editing`), already driving navigation (`nav_sync`), or in
    // conflict mode (handled separately). Sets `nav_sync` around the dropdown
    // change so the resulting `selected_notify` doesn't scroll back.
    if let Some(vadj) = file_view.vadjustment() {
        vadj.connect_value_changed({
            let file_view = file_view.clone();
            let file_buffer = file_buffer.clone();
            let file_dropdown = file_dropdown.clone();
            let nav_sync = nav_sync.clone();
            let editing = editing.clone();
            let pane_mode = pane_mode.clone();
            move |vadj| {
                if editing.get() || nav_sync.get() {
                    return;
                }
                let (iter, _) = file_view.line_at_y(vadj.value() as i32);
                let top_line = iter.line() as usize;
                let idx = if pane_mode.borrow().is_conflict() {
                    conflict_file_index_at_line(&file_buffer, top_line)
                } else {
                    diff_file_index_at_line(&file_buffer, top_line)
                } as u32;
                if file_dropdown.selected() != idx {
                    nav_sync.set(true);
                    file_dropdown.set_selected(idx);
                    nav_sync.set(false);
                }
            }
        });
    }

    // Scrolling over the (closed) drop-down steps through the files of the diff,
    // so flipping between files doesn't require opening the popover each time.
    let scroll_controller = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
    scroll_controller.connect_scroll({
        let file_dropdown = file_dropdown.clone();
        move |_, _, dy| {
            let n = file_dropdown.model().map_or(0, |m| m.n_items());
            if n == 0 {
                return glib::Propagation::Proceed;
            }
            let cur = file_dropdown.selected();
            if cur == gtk::INVALID_LIST_POSITION {
                return glib::Propagation::Proceed;
            }
            // Scroll down (dy > 0) advances to the next file; up goes back.
            // Clamp at the ends rather than wrapping.
            let next = if dy > 0.0 {
                (cur + 1).min(n - 1)
            } else if dy < 0.0 {
                cur.saturating_sub(1)
            } else {
                cur
            };
            if next != cur {
                file_dropdown.set_selected(next);
            }
            glib::Propagation::Stop
        }
    });
    file_dropdown.add_controller(scroll_controller);

    // Load the changed-files list for the selected commit into the dropdown.
    // Populate the file dropdown and diff view from an already-loaded change
    // set, shared by the commit and working-copy loaders below.
    let apply_changes: Rc<dyn Fn(Vec<FileChange>)> = {
        let changes = changes.clone();
        let current_file = current_file.clone();
        let combined_files = combined_files.clone();
        let file_dropdown = file_dropdown.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let editing = editing.clone();
        let expansions = expansions.clone();
        let render_diff_view = render_diff_view.clone();
        let scroll_to_file = scroll_to_file.clone();
        Rc::new(move |loaded: Vec<FileChange>| {
            *changes.borrow_mut() = loaded;
            *current_file.borrow_mut() = None;
            expansions.borrow_mut().clear();
            if changes.borrow().is_empty() {
                *combined_files.borrow_mut() = Vec::new();
                editing.set(true);
                file_buffer.set_text("");
                editing.set(false);
                file_view.set_editable(false);
                file_dropdown.set_model(Some(&StringList::new(&[])));
                return;
            }
            // Render the whole change once; the dropdown is now a jump aid.
            render_diff_view();
            let labels: Vec<String> = changes.borrow().iter().map(change_label).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            file_dropdown.set_model(Some(&StringList::new(&refs)));
            file_dropdown.set_selected(0);
            // Land at the first file's top (and set current_file) even if
            // set_selected(0) didn't fire a change notification.
            scroll_to_file(0);
        })
    };

    let load_changes: Rc<dyn Fn(&CommitInfo)> = {
        let repo = repo.clone();
        let apply_changes = apply_changes.clone();
        Rc::new(move |commit: &CommitInfo| {
            let loaded = commit_changes(&repo.borrow().repo, &commit.id).unwrap_or_default();
            apply_changes(loaded);
        })
    };

    // Load the read-only working-copy (@) diff into the file pane.
    let load_wc_changes: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let apply_changes = apply_changes.clone();
        Rc::new(move || {
            let loaded = {
                let r = repo.borrow();
                match r.working_copy_info() {
                    Some(info) => commit_changes(&r.repo, &info.commit_id).unwrap_or_default(),
                    None => Vec::new(),
                }
            };
            apply_changes(loaded);
        })
    };

    // Selecting a commit loads its message and changed files.
    list.connect_row_selected({
        let commits = commits.clone();
        let message_buffer = message_buffer.clone();
        let message_view = message_view.clone();
        let selected_change = selected_change.clone();
        let load_changes = load_changes.clone();
        let load_conflict_files = load_conflict_files.clone();
        let pane_mode = pane_mode.clone();
        let identity_fields = identity_fields.clone();
        let original_identity = original_identity.clone();
        let viewing_wc = viewing_wc.clone();
        let wc_list = wc_list.clone();
        move |_list, row| {
            let Some(row) = row else { return };
            let idx = row.index();
            if idx < 0 {
                return;
            }
            // Leaving the read-only working-copy view: re-enable editing and
            // drop its (mutually exclusive) selection.
            viewing_wc.set(false);
            wc_list.unselect_all();
            message_view.set_editable(true);
            for f in identity_fields.iter() {
                f.set_sensitive(true);
            }
            let info = commits.borrow().get(idx as usize).cloned();
            let Some(info) = info else { return };
            *selected_change.borrow_mut() = Some(info.change_id_hex());
            message_buffer.set_text(&info.description);
            // In conflict mode the dropdown lists the commit's conflicted files,
            // not its diff; identity editing is disabled until conflicts resolve.
            if pane_mode.borrow().is_conflict() {
                load_conflict_files(&info);
                return;
            }
            set_identity_fields(&identity_fields, &info);
            *original_identity.borrow_mut() = Some(read_identity(&identity_fields));
            load_changes(&info);
        }
    });

    // Selecting the working-copy (@) row shows its diff read-only: there is no
    // message or identity to edit, and Save is inert (see the `save` closure).
    wc_list.connect_row_selected({
        let viewing_wc = viewing_wc.clone();
        let list = list.clone();
        let load_wc_changes = load_wc_changes.clone();
        let message_buffer = message_buffer.clone();
        let message_view = message_view.clone();
        let identity_fields = identity_fields.clone();
        let pane_mode = pane_mode.clone();
        move |_wc_list, row| {
            if row.is_none() || pane_mode.borrow().is_conflict() {
                return;
            }
            viewing_wc.set(true);
            // Mutually exclusive with the history selection.
            list.unselect_all();
            message_buffer.set_text("");
            message_view.set_editable(false);
            for f in identity_fields.iter() {
                f.set_text("");
                f.set_sensitive(false);
            }
            load_wc_changes();
        }
    });

    // Update the read-only working-copy (@) row from the engine: show it with a
    // summary when the tree is dirty, hide it when clean or while resolving
    // conflicts.
    let refresh_wc: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let wc_list = wc_list.clone();
        let wc_label = wc_label.clone();
        let pane_mode = pane_mode.clone();
        Rc::new(move || {
            if pane_mode.borrow().is_conflict() {
                wc_list.set_visible(false);
                return;
            }
            match repo.borrow().working_copy_info() {
                Some(info) => {
                    let n = info.changed_files;
                    let s = if n == 1 { "" } else { "s" };
                    wc_label.set_text(&if info.has_conflict {
                        format!("\u{26A0} Uncommitted changes \u{2014} conflicts in {n} file{s}")
                    } else {
                        format!("\u{270E} Uncommitted changes \u{2014} {n} file{s}")
                    });
                    wc_list.set_visible(true);
                }
                None => wc_list.set_visible(false),
            }
        })
    };

    // Reload history from the engine, preserving the selected commit by its
    // (rewrite-stable) change id.
    let refresh: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let commits = commits.clone();
        let list = list.clone();
        let selected_change = selected_change.clone();
        let identities = identities.clone();
        let history_limit = history_limit.clone();
        let history_has_more = history_has_more.clone();
        let refresh_wc = refresh_wc.clone();
        Rc::new(move || {
            let (loaded, has_more) = {
                let r = repo.borrow();
                match r.head_commit_id() {
                    Some(head) => history_limited(&r.repo, &head, history_limit.get())
                        .unwrap_or_default(),
                    None => (Vec::new(), false),
                }
            };
            history_has_more.set(has_more);
            *commits.borrow_mut() = loaded;
            {
                let cs = commits.borrow();
                populate_list(&list, &cs, &HashSet::new());
                // Harvest the distinct identities seen across history, offered by
                // the in-field ▼ picker.
                let mut ids: Vec<(String, String)> = Vec::new();
                for c in cs.iter() {
                    for pair in [
                        (c.author_name.clone(), c.author_email.clone()),
                        (c.committer_name.clone(), c.committer_email.clone()),
                    ] {
                        if !ids.contains(&pair) {
                            ids.push(pair);
                        }
                    }
                }
                ids.sort();
                *identities.borrow_mut() = ids;
            }
            let target_row = selected_change.borrow().clone().and_then(|change| {
                commits
                    .borrow()
                    .iter()
                    .position(|c| c.change_id_hex() == change)
            });
            if let Some(idx) = target_row {
                if let Some(row) = list.row_at_index(idx as i32) {
                    list.select_row(Some(&row));
                }
            }
            refresh_wc();
        })
    };

    // Lazy paging: when the user scrolls to the bottom of the history and older
    // commits remain, load another page and rebuild. Only in the normal diff view
    // — conflict mode populates the list from a different (unbounded) walk, so we
    // must not let a scroll clobber it. The grown limit persists across refreshes.
    history_scroll.connect_edge_reached({
        let history_limit = history_limit.clone();
        let history_has_more = history_has_more.clone();
        let pane_mode = pane_mode.clone();
        let refresh = refresh.clone();
        move |_, pos| {
            if pos != gtk::PositionType::Bottom
                || !history_has_more.get()
                || !matches!(&*pane_mode.borrow(), PaneMode::Diff)
            {
                return;
            }
            history_limit.set(history_limit.get() + HISTORY_PAGE);
            refresh();
        }
    });

    // Rebuild the history list from jj's pending (not-yet-exported) head while a
    // conflicted rewrite is being resolved, badging the still-conflicted commits
    // and updating the banner's progress text. Selecting a row cascades through
    // `row-selected` -> `load_conflict_files`.
    let refresh_conflict: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let commits = commits.clone();
        let list = list.clone();
        let pane_mode = pane_mode.clone();
        let conflict_label = conflict_label.clone();
        let selected_change = selected_change.clone();
        let wc_list = wc_list.clone();
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
            // The working copy @ resolves inline among the conflicted commits, so
            // hide the standalone @ row and prepend @ to the chain when it's the
            // (or a) conflicted commit.
            wc_list.set_visible(false);
            if let Some(wc_info) = repo.borrow().working_copy_commit_info() {
                if badges.contains(&wc_info.change_id_hex()) {
                    commits.borrow_mut().insert(0, wc_info);
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
    };

    // Leave conflict mode: back to the normal diff pane, banner hidden.
    let exit_conflict_mode: Rc<dyn Fn()> = {
        let pane_mode = pane_mode.clone();
        let conflict_banner = conflict_banner.clone();
        let prev_conflict_button = prev_conflict_button.clone();
        let next_conflict_button = next_conflict_button.clone();
        let save_button = save_button.clone();
        Rc::new(move || {
            *pane_mode.borrow_mut() = PaneMode::Diff;
            conflict_banner.set_visible(false);
            prev_conflict_button.set_visible(false);
            next_conflict_button.set_visible(false);
            save_button.set_tooltip_text(Some(SAVE_HINT_DIFF));
        })
    };

    // Enter conflict mode with the engine's reported conflicts: show the banner,
    // select the oldest conflicted commit, and render the pending chain. The
    // quick-resolve affordances are the inline marker-line cues (see
    // `append_resolve_cues`).
    let enter_conflict_mode: Rc<dyn Fn(Vec<ConflictedCommit>)> = {
        let pane_mode = pane_mode.clone();
        let conflict_banner = conflict_banner.clone();
        let selected_change = selected_change.clone();
        let refresh_conflict = refresh_conflict.clone();
        let prev_conflict_button = prev_conflict_button.clone();
        let next_conflict_button = next_conflict_button.clone();
        let save_button = save_button.clone();
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
    };

    // Resolve the conflicted file currently in the buffer. The engine re-checks
    // the whole chain: when the last conflict clears it exports the rewrite and we
    // return to the normal view, otherwise the remaining conflicts are re-shown.
    let resolve_current: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let pane_mode = pane_mode.clone();
        let selected_change = selected_change.clone();
        let conflict_view = conflict_view.clone();
        let sync_conflict_from_buffer = sync_conflict_from_buffer.clone();
        let refresh = refresh.clone();
        let refresh_conflict = refresh_conflict.clone();
        let exit_conflict_mode = exit_conflict_mode.clone();
        let show_status = show_status.clone();
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
    };

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

    // Render the session "Review" into its (read-only) full-window buffer: the
    // content delta between the current tree and the one the session started
    // with. Recomputed each time the view is shown — and after "Revert all" — so
    // it always reflects the live tree. A message/identity-only edit changes no
    // tree, so it produces an empty review; after a revert it empties too.
    let render_review: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let review_buffer = review_buffer.clone();
        let syntax_set = syntax_set.clone();
        let theme = theme.clone();
        let show_status = show_status.clone();
        Rc::new(move || {
            let changes = match repo.borrow_mut().session_changes() {
                Ok(changes) => changes,
                Err(err) => {
                    show_status(&format!("Review failed: {err}"));
                    return;
                }
            };
            if changes.is_empty() {
                review_buffer.set_text("No content changes since the session started.");
                return;
            }
            // Default context, no expand cues: the review is read-only, so the
            // diff pane's hunk-expansion wiring deliberately doesn't apply.
            let combined = render_commit_diff(&changes, &HashMap::new());
            review_buffer.set_text(&combined.text);
            let first = combined.files.first().map(|f| f.path.as_str());
            highlight_diff(&review_buffer, first, &syntax_set, &theme);
        })
    };

    // "Review" toggle: swap the whole window between the editor and the
    // read-only session diff. Computing the diff snapshots the working copy
    // (which can move `@`), so refresh the now-hidden editor to keep its
    // history/`@`-row consistent.
    review_button.connect_toggled({
        let content_stack = content_stack.clone();
        let render_review = render_review.clone();
        let refresh = refresh.clone();
        move |btn| {
            if btn.is_active() {
                render_review();
                content_stack.set_visible_child_name("review");
                refresh();
            } else {
                content_stack.set_visible_child_name("edit");
            }
        }
    });

    // "Revert all" (top-right header button): after confirmation, roll the whole
    // session back to the state the repo was opened in — original commits *and*
    // the session-start working copy — then reload. Mirrors the abort handler's
    // reload, and additionally empties the trash (its drops are undone).
    revert_button.connect_clicked({
        let repo = repo.clone();
        let window = window.clone();
        let exit_conflict_mode = exit_conflict_mode.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let list = list.clone();
        let trashed = trashed.clone();
        let trash_list = trash_list.clone();
        let trash_scroll = trash_scroll.clone();
        let review_button = review_button.clone();
        let render_review = render_review.clone();
        move |_| {
            let target = repo
                .borrow()
                .session_start_head_hex()
                .map(|h| h[..h.len().min(12)].to_string())
                .unwrap_or_else(|| "its original state".to_string());
            let dialog = gtk::AlertDialog::builder()
                .modal(true)
                .message("Revert all changes?")
                .detail(format!(
                    "Discard everything done this session and restore the repository to \
                     {target}. Uncommitted changes made this session are lost. This cannot \
                     be undone."
                ))
                .buttons(["Cancel", "Revert all"])
                .cancel_button(0)
                .default_button(0)
                .build();
            let repo = repo.clone();
            let exit_conflict_mode = exit_conflict_mode.clone();
            let refresh = refresh.clone();
            let show_status = show_status.clone();
            let list = list.clone();
            let trashed = trashed.clone();
            let trash_list = trash_list.clone();
            let trash_scroll = trash_scroll.clone();
            let review_button = review_button.clone();
            let render_review = render_review.clone();
            dialog.choose(
                Some(&window),
                gtk::gio::Cancellable::NONE,
                move |result| {
                    // Index 1 is "Revert all"; anything else (Cancel / dismissed)
                    // leaves the session untouched.
                    if result != Ok(1) {
                        return;
                    }
                    if let Err(err) = repo.borrow_mut().revert_all() {
                        show_status(&format!("Revert failed: {err}"));
                        return;
                    }
                    // Drop conflict mode if we were resolving (idempotent otherwise).
                    exit_conflict_mode();
                    // The session's drops are undone, so empty the trash bin.
                    trashed.borrow_mut().clear();
                    populate_trash(&trash_list, &trash_scroll, &trashed.borrow());
                    // Force the reselect to re-fire `row-selected` (rows are reused),
                    // so the diff pane reloads the reverted content — same reasoning
                    // as the abort handler above.
                    list.unselect_all();
                    refresh();
                    // If we're reviewing, re-render: the revert restored the
                    // session-start tree, so the review now shows no changes.
                    if review_button.is_active() {
                        render_review();
                    }
                    show_status("Reverted to the original session state.");
                },
            );
        }
    });

    // Drag-and-drop to reorder commits. While dragging, a placeholder row opens a
    // gap at the hover position (the surrounding commits slide to make room) and
    // the dragged row is dimmed; dropping rebases the commit into that slot via
    // the engine and reloads. The reorder is applied immediately — there is no
    // separate Save step for it.
    let placeholder = ListBoxRow::new();
    placeholder.set_selectable(false);
    placeholder.set_activatable(false);
    placeholder.set_height_request(28);
    placeholder.add_css_class("drop-placeholder");
    // The placeholder is a passive visual gap. Make it non-targetable so GTK's
    // pointer/drop picking skips it: otherwise GTK can cache it as the drop-
    // crossing target, and `clear_gap` later removes (unparents) it, leaving GTK
    // to walk an orphaned row on the next event and segfault.
    placeholder.set_can_target(false);
    // The row currently being dragged, so it can be un-dimmed when the drag ends.
    let drag_row: Rc<RefCell<Option<ListBoxRow>>> = Rc::new(RefCell::new(None));
    // The dragged row's original index (newest-first), captured at drag start so
    // motion can tell whether a hover gap is a real move (vs. the no-op gaps just
    // above/below the row, or an off-chain row).
    let drag_from: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
    // The insertion gap (newest-first index, 0..=len) the placeholder marks.
    let drop_gap: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
    // The commit display index a squash would drop ONTO (center-zone hover over a
    // valid target), or None. Mutually exclusive with `drop_gap`: a row's edges
    // open a reorder gap, its middle marks a squash target.
    let drop_onto: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
    // A drop handler rewrites history and rebuilds both lists, which destroys the
    // ListBoxRow widgets. Doing that while the drag is still in flight frees a row
    // GTK still holds as the drop-crossing target, crashing the next pointer event
    // (it walks the freed widget). So a drop only *stages* its work here; the drag
    // source runs it from `drag-end`, once the gesture — and GTK's DnD bookkeeping
    // — is fully torn down.
    let post_drag: Rc<RefCell<Option<Box<dyn FnOnce()>>>> = Rc::new(RefCell::new(None));

    // Map a y coordinate to an insertion gap: onto a row's lower half drops below
    // it; past the last row drops at the bottom; above the first, at the top. The
    // placeholder must not be inserted when this runs, or row indices are off.
    let gap_at: Rc<dyn Fn(f64) -> usize> = {
        let list = list.clone();
        let commits = commits.clone();
        Rc::new(move |y: f64| -> usize {
            let n = commits.borrow().len();
            match list.row_at_y(y as i32) {
                Some(row) => {
                    let alloc = row.allocation();
                    let below = (y as i32) > alloc.y() + alloc.height() / 2;
                    (row.index() as usize + usize::from(below)).min(n)
                }
                None if y <= 0.0 => 0,
                None => n,
            }
        })
    };

    // Move the placeholder to the gap under `y`, but only when that gap actually
    // changes — re-inserting it on every motion event makes the rows below
    // flicker. The placeholder occupies list index == its gap, so a row hit at
    // that index is the placeholder (gap unchanged); other rows' indices are
    // mapped back past it to commit coordinates.
    let show_gap: Rc<dyn Fn(f64)> = {
        let list = list.clone();
        let placeholder = placeholder.clone();
        let commits = commits.clone();
        let drop_gap = drop_gap.clone();
        let repo = repo.clone();
        let drag_from = drag_from.clone();
        let drag_origin = drag_origin.clone();
        let trashed = trashed.clone();
        Rc::new(move |y: f64| {
            let n = commits.borrow().len();
            let current = drop_gap.get();
            let new_gap = match list.row_at_y(y as i32) {
                Some(row) => {
                    let li = row.index() as usize;
                    if current == Some(li) {
                        return; // hovering the placeholder: the gap is unchanged
                    }
                    let alloc = row.allocation();
                    let below = (y as i32) > alloc.y() + alloc.height() / 2;
                    let ci = match current {
                        Some(g) if li > g => li - 1,
                        _ => li,
                    };
                    (ci + usize::from(below)).min(n)
                }
                None if y <= 0.0 => 0,
                None => n,
            };
            if current == Some(new_gap) {
                return;
            }
            // Only open a gap where dropping would actually move/graft the
            // commit. For a history drag the no-op gaps just above/below the
            // dragged row (and off-chain rows) yield None; for a trash drag the
            // same gate runs through plan_restore on the trashed commit.
            let real_move = drag_from.get().is_some_and(|from| match drag_origin.get() {
                DragOrigin::History => repo
                    .borrow()
                    .plan_reorder(&commits.borrow(), from, new_gap)
                    .is_some(),
                DragOrigin::Trash => trashed.borrow().get(from).is_some_and(|info| {
                    repo.borrow()
                        .plan_restore(&commits.borrow(), info, new_gap)
                        .is_some()
                }),
            });
            if !real_move {
                if placeholder.parent().is_some() {
                    list.remove(&placeholder);
                }
                drop_gap.set(None);
                return;
            }
            if placeholder.parent().is_some() {
                list.remove(&placeholder);
            }
            drop_gap.set(Some(new_gap));
            list.insert(&placeholder, new_gap as i32);
        })
    };
    let clear_gap: Rc<dyn Fn()> = {
        let list = list.clone();
        let placeholder = placeholder.clone();
        let drop_gap = drop_gap.clone();
        Rc::new(move || {
            if placeholder.parent().is_some() {
                list.remove(&placeholder);
            }
            drop_gap.set(None);
        })
    };

    // Mark the row at commit index `ci` as the active squash target (red — a
    // drop will rewrite it), if squashing the dragged row onto it is valid; clear
    // any previous target first. A no-op when `ci` is already the active target
    // (flicker guard, mirroring `show_gap`).
    let set_squash_target: Rc<dyn Fn(usize)> = {
        let list = list.clone();
        let commits = commits.clone();
        let repo = repo.clone();
        let drag_from = drag_from.clone();
        let drag_origin = drag_origin.clone();
        let drop_onto = drop_onto.clone();
        let trashed = trashed.clone();
        Rc::new(move |ci: usize| {
            if drop_onto.get() == Some(ci) {
                return;
            }
            if let Some(prev) = drop_onto.get() {
                if let Some(r) = list.row_at_index(prev as i32) {
                    r.remove_css_class("squash-drop-target");
                }
            }
            // A history drag squashes one chain commit onto another; a trash drag
            // squashes the trashed commit onto the chain commit at `ci`.
            let valid = drag_from.get().is_some_and(|from| match drag_origin.get() {
                DragOrigin::History => {
                    repo.borrow().plan_squash(&commits.borrow(), from, ci).is_some()
                }
                DragOrigin::Trash => trashed.borrow().get(from).is_some_and(|info| {
                    repo.borrow()
                        .plan_squash_restore(&commits.borrow(), info, ci)
                        .is_some()
                }),
            });
            if valid {
                if let Some(r) = list.row_at_index(ci as i32) {
                    r.add_css_class("squash-drop-target");
                }
                drop_onto.set(Some(ci));
            } else {
                drop_onto.set(None);
            }
        })
    };
    let clear_squash_target: Rc<dyn Fn()> = {
        let list = list.clone();
        let drop_onto = drop_onto.clone();
        Rc::new(move || {
            if let Some(prev) = drop_onto.get() {
                if let Some(r) = list.row_at_index(prev as i32) {
                    r.remove_css_class("squash-drop-target");
                }
            }
            drop_onto.set(None);
        })
    };

    // Motion dispatcher: a row's top/bottom quarter opens a reorder gap
    // (`show_gap`), its middle half marks a squash target (`set_squash_target`).
    // At most one is active at a time — switching zones clears the other's
    // visual, which also keeps the placeholder absent whenever a squash index is
    // computed, so the list-vs-commit index math stays simple.
    let show_zone: Rc<dyn Fn(f64)> = {
        let list = list.clone();
        let show_gap = show_gap.clone();
        let clear_gap = clear_gap.clone();
        let set_squash_target = set_squash_target.clone();
        let clear_squash_target = clear_squash_target.clone();
        let drop_gap = drop_gap.clone();
        Rc::new(move |y: f64| {
            let Some(row) = list.row_at_y(y as i32) else {
                // Above the first / below the last row: a pure reorder gap.
                clear_squash_target();
                show_gap(y);
                return;
            };
            let li = row.index() as usize;
            // Hovering the placeholder itself: the gap is unchanged, leave it.
            if drop_gap.get() == Some(li) {
                return;
            }
            let alloc = row.allocation();
            let local = (y as i32) - alloc.y();
            let h = alloc.height().max(1);
            if local < h / 4 || local >= h - h / 4 {
                // Edge: reorder gap.
                clear_squash_target();
                show_gap(y);
            } else {
                // Center: squash onto this commit. Map the list index past a
                // present placeholder (same rule as `show_gap`) before removing it.
                let ci = match drop_gap.get() {
                    Some(g) if li > g => li - 1,
                    _ => li,
                };
                clear_gap();
                set_squash_target(ci);
            }
        })
    };

    let drag_source = DragSource::new();
    drag_source.set_actions(gdk::DragAction::MOVE);
    drag_source.connect_prepare({
        let list = list.clone();
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let drag_origin = drag_origin.clone();
        move |source, _x, y| {
            let row = list.row_at_y(y as i32)?;
            // Show the dragged row under the cursor for feedback.
            let paintable = gtk::WidgetPaintable::new(Some(&row));
            source.set_icon(Some(&paintable), 0, 0);
            *drag_row.borrow_mut() = Some(row.clone());
            drag_from.set(Some(row.index() as usize));
            drag_origin.set(DragOrigin::History);
            Some(gdk::ContentProvider::for_value(&row.index().to_value()))
        }
    });
    drag_source.connect_drag_begin({
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let repo = repo.clone();
        let commits = commits.clone();
        let list = list.clone();
        move |_source, _drag| {
            if let Some(row) = drag_row.borrow().as_ref() {
                row.add_css_class("commit-dragging");
            }
            // Highlight where this commit would squash: green for the real
            // target(s), yellow for other autosquash commits aimed at the same
            // target. Empty (no-op) unless the dragged commit is prefixed.
            if let Some(from) = drag_from.get() {
                let recs = repo.borrow().squash_recommendations(&commits.borrow(), from);
                for i in recs.targets {
                    if let Some(r) = list.row_at_index(i as i32) {
                        r.add_css_class("squash-recommended");
                    }
                }
                for i in recs.siblings {
                    if let Some(r) = list.row_at_index(i as i32) {
                        r.add_css_class("squash-sibling");
                    }
                }
            }
        }
    });
    drag_source.connect_drag_end({
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let clear_gap = clear_gap.clone();
        let clear_squash_target = clear_squash_target.clone();
        let list = list.clone();
        let post_drag = post_drag.clone();
        move |_source, _drag, _delete| {
            if let Some(row) = drag_row.borrow_mut().take() {
                row.remove_css_class("commit-dragging");
            }
            drag_from.set(None);
            clear_gap();
            // populate_rows won't touch our highlight classes, so strip them here.
            let mut i = 0;
            while let Some(r) = list.row_at_index(i) {
                r.remove_css_class("squash-recommended");
                r.remove_css_class("squash-sibling");
                i += 1;
            }
            clear_squash_target();
            run_post_drag(&post_drag);
        }
    });
    list.add_controller(drag_source);

    let drop_target = DropTarget::new(i32::static_type(), gdk::DragAction::MOVE);
    drop_target.connect_enter({
        let show_zone = show_zone.clone();
        move |_target, _x, y| {
            show_zone(y);
            gdk::DragAction::MOVE
        }
    });
    drop_target.connect_motion({
        let show_zone = show_zone.clone();
        move |_target, _x, y| {
            show_zone(y);
            gdk::DragAction::MOVE
        }
    });
    drop_target.connect_leave({
        let clear_gap = clear_gap.clone();
        let clear_squash_target = clear_squash_target.clone();
        move |_target| {
            clear_gap();
            clear_squash_target();
        }
    });
    drop_target.connect_drop({
        let commits = commits.clone();
        let repo = repo.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let gap_at = gap_at.clone();
        let clear_gap = clear_gap.clone();
        let drop_gap = drop_gap.clone();
        let drop_onto = drop_onto.clone();
        let list = list.clone();
        let drag_origin = drag_origin.clone();
        let trashed = trashed.clone();
        let trash_list = trash_list.clone();
        let trash_scroll = trash_scroll.clone();
        let selected_change = selected_change.clone();
        let post_drag = post_drag.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        move |_target, value, _x, y| {
            let Ok(from) = value.get::<i32>() else {
                return false;
            };
            // A center-zone hover marks a squash target; snapshot it now, since
            // `drag-end` clears it before the staged work runs.
            let onto = drop_onto.get();
            // Prefer the gap the placeholder marked; fall back to the drop point.
            let to = match drop_gap.get() {
                Some(to) => to,
                None => gap_at(y),
            };
            clear_gap();
            // Stage the work; `drag-end` runs it once the gesture is fully over
            // (rewriting history rebuilds these rows, which is unsafe mid-drag).
            match drag_origin.get() {
                DragOrigin::History if onto.is_some() => {
                    // Dropped ONTO a commit: squash the dragged commit into it. A
                    // prefixed commit acts immediately; an unprefixed one opens a
                    // popover to pick the mode.
                    let onto = onto.unwrap();
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        let plan = repo.borrow().plan_squash(&commits.borrow(), from as usize, onto);
                        let Some((source, dest)) = plan else {
                            return;
                        };
                        let subject = commits.borrow()[from as usize].subject.clone();

                        // Run a chosen mode and report the outcome.
                        let apply: Rc<dyn Fn(SquashMode)> = {
                            let repo = repo.clone();
                            let refresh = refresh.clone();
                            let show_status = show_status.clone();
                            let enter_conflict_mode = enter_conflict_mode.clone();
                            Rc::new(move |mode| {
                                let outcome = repo.borrow_mut().squash_into(&source, &dest, mode);
                                match outcome {
                                    Ok(SaveOutcome::Clean) => refresh(),
                                    Ok(SaveOutcome::Conflicts { commits }) => {
                                        enter_conflict_mode(commits)
                                    }
                                    Err(err) => show_status(&format!("Squash failed: {err}")),
                                }
                            })
                        };

                        match parse_squash_mode(&subject) {
                            // Prefixed: the prefix picks the mode, apply at once.
                            Some(mode) => apply(mode),
                            // Unprefixed: ask how to merge, anchored at the target.
                            None => {
                                let Some(target_row) = list.row_at_index(onto as i32) else {
                                    return;
                                };
                                show_squash_popover(&target_row, &apply);
                            }
                        }
                    }));
                    true
                }
                DragOrigin::History => {
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        // Plan against the current branch's linear chain (the view
                        // may also show other branches/tags); a no-op or off-branch
                        // drop yields None.
                        let plan =
                            repo.borrow().plan_reorder(&commits.borrow(), from as usize, to);
                        let Some(mv) = plan else {
                            return;
                        };
                        let outcome = repo.borrow_mut().reorder_commit(
                            &mv.target,
                            mv.new_parents,
                            mv.new_children,
                            &mv.new_tip,
                        );
                        match outcome {
                            Ok(SaveOutcome::Clean) => refresh(),
                            Ok(SaveOutcome::Conflicts { commits }) => enter_conflict_mode(commits),
                            Err(err) => show_status(&format!("Reorder failed: {err}")),
                        }
                    }));
                    true
                }
                DragOrigin::Trash if onto.is_some() => {
                    // Dropped a trashed commit ONTO a chain commit: squash its
                    // changes into that commit and forget it from the trash. A
                    // prefixed trashed subject acts at once; otherwise a popover
                    // picks the mode — mirroring the history squash arm above.
                    let onto = onto.unwrap();
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let trashed = trashed.clone();
                    let trash_list = trash_list.clone();
                    let trash_scroll = trash_scroll.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        let Some(info) = trashed.borrow().get(from as usize).cloned() else {
                            return;
                        };
                        let plan =
                            repo.borrow().plan_squash_restore(&commits.borrow(), &info, onto);
                        let Some((source, dest)) = plan else {
                            return;
                        };
                        let subject = info.subject.clone();
                        let change_hex = info.change_id_hex();

                        // Run a chosen mode and report the outcome.
                        let apply: Rc<dyn Fn(SquashMode)> = {
                            let repo = repo.clone();
                            let refresh = refresh.clone();
                            let show_status = show_status.clone();
                            let enter_conflict_mode = enter_conflict_mode.clone();
                            let trashed = trashed.clone();
                            let trash_list = trash_list.clone();
                            let trash_scroll = trash_scroll.clone();
                            Rc::new(move |mode| {
                                let outcome =
                                    repo.borrow_mut().squash_restore_into(&source, &dest, mode);
                                // On success (Clean or pending Conflicts) the
                                // changes now live in the target, so forget the
                                // trashed commit — match by change id, since the
                                // popover may have let the trash drift.
                                match outcome {
                                    Ok(SaveOutcome::Clean) => {
                                        trashed
                                            .borrow_mut()
                                            .retain(|c| c.change_id_hex() != change_hex);
                                        populate_trash(&trash_list, &trash_scroll, &trashed.borrow());
                                        refresh();
                                    }
                                    Ok(SaveOutcome::Conflicts { commits }) => {
                                        trashed
                                            .borrow_mut()
                                            .retain(|c| c.change_id_hex() != change_hex);
                                        populate_trash(&trash_list, &trash_scroll, &trashed.borrow());
                                        enter_conflict_mode(commits);
                                    }
                                    Err(err) => show_status(&format!("Squash failed: {err}")),
                                }
                            })
                        };

                        match parse_squash_mode(&subject) {
                            // Prefixed: the prefix picks the mode, apply at once.
                            Some(mode) => apply(mode),
                            // Unprefixed: ask how to merge, anchored at the target.
                            None => {
                                let Some(target_row) = list.row_at_index(onto as i32) else {
                                    return;
                                };
                                show_squash_popover(&target_row, &apply);
                            }
                        }
                    }));
                    true
                }
                DragOrigin::Trash => {
                    // Restoring a trashed commit: graft it back into the chain at
                    // the drop gap, drop it from the trash, and select it.
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let trashed = trashed.clone();
                    let trash_list = trash_list.clone();
                    let trash_scroll = trash_scroll.clone();
                    let selected_change = selected_change.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        let Some(info) = trashed.borrow().get(from as usize).cloned() else {
                            return;
                        };
                        let plan = repo.borrow().plan_restore(&commits.borrow(), &info, to);
                        let Some(mv) = plan else {
                            return;
                        };
                        let outcome = repo.borrow_mut().restore_commit(
                            &mv.target,
                            mv.new_parents,
                            mv.new_children,
                            &mv.new_tip,
                        );
                        match outcome {
                            Ok(SaveOutcome::Clean) => {
                                trashed.borrow_mut().remove(from as usize);
                                *selected_change.borrow_mut() = Some(info.change_id_hex());
                                refresh();
                                populate_trash(&trash_list, &trash_scroll, &trashed.borrow());
                            }
                            Ok(SaveOutcome::Conflicts { commits }) => {
                                trashed.borrow_mut().remove(from as usize);
                                enter_conflict_mode(commits);
                                populate_trash(&trash_list, &trash_scroll, &trashed.borrow());
                            }
                            Err(err) => show_status(&format!("Restore failed: {err}")),
                        }
                    }));
                    true
                }
            }
        }
    });
    list.add_controller(drop_target);

    // The trash list mirrors the history list's drag-and-drop: a source so its
    // rows can be dragged back into history (restore), and a drop target so
    // history rows dragged onto it are dropped (abandoned). Reordering within the
    // trash is meaningless, so trash→trash drops are ignored.
    let trash_drag = DragSource::new();
    trash_drag.set_actions(gdk::DragAction::MOVE);
    trash_drag.connect_prepare({
        let trash_list = trash_list.clone();
        let trashed = trashed.clone();
        let drag_row = drag_row.clone();
        let drag_origin = drag_origin.clone();
        let drag_from = drag_from.clone();
        move |source, _x, y| {
            if trashed.borrow().is_empty() {
                return None; // only the hint row is present
            }
            let row = trash_list.row_at_y(y as i32)?;
            let paintable = gtk::WidgetPaintable::new(Some(&row));
            source.set_icon(Some(&paintable), 0, 0);
            *drag_row.borrow_mut() = Some(row.clone());
            drag_origin.set(DragOrigin::Trash);
            // The motion handlers (show_gap / set_squash_target) read drag_from to
            // validate the restore/squash; it's the trash row index here.
            drag_from.set(Some(row.index() as usize));
            Some(gdk::ContentProvider::for_value(&row.index().to_value()))
        }
    });
    trash_drag.connect_drag_begin({
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let trashed = trashed.clone();
        let repo = repo.clone();
        let commits = commits.clone();
        let list = list.clone();
        move |_source, _drag| {
            if let Some(row) = drag_row.borrow().as_ref() {
                row.add_css_class("commit-dragging");
            }
            // Same green/yellow squash hints as a history drag, for a trashed
            // commit whose subject carries an autosquash prefix. Empty otherwise.
            if let Some(info) = drag_from.get().and_then(|f| trashed.borrow().get(f).cloned()) {
                let recs = repo.borrow().squash_recommendations_for(&commits.borrow(), &info);
                for i in recs.targets {
                    if let Some(r) = list.row_at_index(i as i32) {
                        r.add_css_class("squash-recommended");
                    }
                }
                for i in recs.siblings {
                    if let Some(r) = list.row_at_index(i as i32) {
                        r.add_css_class("squash-sibling");
                    }
                }
            }
        }
    });
    trash_drag.connect_drag_end({
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let clear_gap = clear_gap.clone();
        let clear_squash_target = clear_squash_target.clone();
        let list = list.clone();
        let post_drag = post_drag.clone();
        move |_source, _drag, _delete| {
            if let Some(row) = drag_row.borrow_mut().take() {
                row.remove_css_class("commit-dragging");
            }
            drag_from.set(None);
            clear_gap();
            // The trash drag highlights history rows too (green/yellow recs, red
            // target); strip them here, as populate_rows leaves them alone.
            let mut i = 0;
            while let Some(r) = list.row_at_index(i) {
                r.remove_css_class("squash-recommended");
                r.remove_css_class("squash-sibling");
                i += 1;
            }
            clear_squash_target();
            run_post_drag(&post_drag);
        }
    });
    trash_list.add_controller(trash_drag);

    let trash_drop = DropTarget::new(i32::static_type(), gdk::DragAction::MOVE);
    // Deliberately no widget mutation in enter/leave (no hover highlight): those
    // run inside GTK's drop-crossing synthesis, where touching the widget tree is
    // unsafe. Enter just advertises that the trash accepts the drag.
    trash_drop.connect_enter(move |_target, _x, _y| gdk::DragAction::MOVE);
    trash_drop.connect_drop({
        let commits = commits.clone();
        let repo = repo.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let drag_origin = drag_origin.clone();
        let trashed = trashed.clone();
        let trash_list = trash_list.clone();
        let trash_scroll = trash_scroll.clone();
        let post_drag = post_drag.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        move |_target, value, _x, _y| {
            if drag_origin.get() != DragOrigin::History {
                return false; // dragging within the trash: nothing to do
            }
            let Ok(from) = value.get::<i32>() else {
                return false;
            };
            // Stage the work; the history drag source runs it from `drag-end`,
            // once the gesture is fully over (rewriting + rebuilding the rows
            // mid-drag frees a row GTK still tracks, crashing the next event).
            let repo = repo.clone();
            let commits = commits.clone();
            let refresh = refresh.clone();
            let show_status = show_status.clone();
            let trashed = trashed.clone();
            let trash_list = trash_list.clone();
            let trash_scroll = trash_scroll.clone();
            let enter_conflict_mode = enter_conflict_mode.clone();
            *post_drag.borrow_mut() = Some(Box::new(move || {
                let Some(info) = commits.borrow().get(from as usize).cloned() else {
                    return;
                };
                // Only commits on the current branch's linear chain (and not its
                // sole commit) can be dropped; refuse merges/off-branch/root rows.
                let target = repo.borrow().plan_drop(&commits.borrow(), from as usize);
                let Some(target) = target else {
                    show_status("Can't drop this commit");
                    return;
                };
                let outcome = repo.borrow_mut().abandon_commit(&target);
                match outcome {
                    Ok(SaveOutcome::Clean) => {
                        trashed.borrow_mut().push(info);
                        refresh();
                        populate_trash(&trash_list, &trash_scroll, &trashed.borrow());
                    }
                    Ok(SaveOutcome::Conflicts { commits }) => {
                        trashed.borrow_mut().push(info);
                        enter_conflict_mode(commits);
                        populate_trash(&trash_list, &trash_scroll, &trashed.borrow());
                    }
                    Err(err) => show_status(&format!("Drop failed: {err}")),
                }
            }));
            true
        }
    });
    trash_box.add_controller(trash_drop);
    populate_trash(&trash_list, &trash_scroll, &trashed.borrow());

    // Save: rewrite the message and/or the selected file's content, then reload.
    // Reloading re-selects the commit, which cascades through `row-selected` ->
    // `load_changes` and resets the file dropdown to index 0 with the cursor at
    // the start. We capture the selected file and cursor offset beforehand and
    // restore them afterwards so a save is invisible to the user's place in the
    // diff.
    let save: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let commits = commits.clone();
        let changes = changes.clone();
        let current_file = current_file.clone();
        let message_buffer = message_buffer.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let file_dropdown = file_dropdown.clone();
        let selected_change = selected_change.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let identity_fields = identity_fields.clone();
        let original_identity = original_identity.clone();
        let pane_mode = pane_mode.clone();
        let resolve_current = resolve_current.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        let viewing_wc = viewing_wc.clone();
        let load_wc_changes = load_wc_changes.clone();
        let refresh_wc = refresh_wc.clone();
        Rc::new(move || {
            // In conflict mode, "Save" means "resolve the current conflicted file".
            if pane_mode.borrow().is_conflict() {
                resolve_current();
                return;
            }
            // Viewing the working copy: edit @ in place (no message/identity, and
            // the branch tip doesn't move), then reload the @ diff and row.
            if viewing_wc.get() {
                let saved_file = current_file.borrow().clone();
                let saved_cursor = file_buffer.cursor_position();
                // Edit each changed file of @ in place (no rebase, so a loop is
                // fine); the branch tip doesn't move.
                let edits = match collect_file_edits(&buffer_text(&file_buffer), &changes.borrow()) {
                    Ok(edits) => edits,
                    Err(msg) => {
                        show_status(&msg);
                        return;
                    }
                };
                for (path, content) in &edits {
                    if let Err(err) = repo.borrow_mut().edit_working_copy_file(path, content) {
                        show_status(&format!("Working-copy edit failed: {err}"));
                        return;
                    }
                }
                refresh_wc();
                load_wc_changes();
                if let Some(path) = saved_file {
                    if let Some(idx) = changes.borrow().iter().position(|c| c.path == path) {
                        file_dropdown.set_selected(idx as u32);
                    }
                }
                let offset = saved_cursor.min(file_buffer.char_count());
                file_buffer.place_cursor(&file_buffer.iter_at_offset(offset));
                return;
            }
            let Some(change_id) = selected_change.borrow().clone() else {
                return;
            };
            let target = commits
                .borrow()
                .iter()
                .find(|c| c.change_id_hex() == change_id)
                .map(|c| (c.id.clone(), c.description.clone()));
            let Some((mut commit_id, original_message)) = target else {
                return;
            };

            // Remember where the user is so we can restore it after the reload.
            let saved_file = current_file.borrow().clone();
            let saved_cursor = file_buffer.cursor_position();
            let file_had_focus = file_view.has_focus();

            // Message edit (if changed).
            let new_message = buffer_text(&message_buffer);
            if new_message != original_message {
                // Bind in its own statement so the `RefMut` is dropped before the
                // match arms run — `enter_conflict_mode` re-borrows `repo`.
                let outcome = repo.borrow_mut().rewrite_message(&commit_id, &new_message);
                match outcome {
                    Ok(SaveOutcome::Clean) => {}
                    Ok(SaveOutcome::Conflicts { commits }) => {
                        enter_conflict_mode(commits);
                        return;
                    }
                    Err(err) => {
                        show_status(&format!("Message save failed: {err}"));
                        return;
                    }
                }
                // The commit id changed; re-resolve by change id.
                if let Some(info) = resolve_commit(&repo, &change_id) {
                    commit_id = info.id;
                }
            }

            // File content edits across every file of the combined diff, applied
            // in one rewrite so a multi-file Save is a single transaction.
            let edits = match collect_file_edits(&buffer_text(&file_buffer), &changes.borrow()) {
                Ok(edits) => edits,
                Err(msg) => {
                    show_status(&msg);
                    return;
                }
            };
            if !edits.is_empty() {
                let outcome = repo.borrow_mut().rewrite_files(&commit_id, &edits);
                match outcome {
                    Ok(SaveOutcome::Clean) => {}
                    Ok(SaveOutcome::Conflicts { commits }) => {
                        enter_conflict_mode(commits);
                        return;
                    }
                    Err(err) => {
                        show_status(&format!("File save failed: {err}"));
                        return;
                    }
                }
            }

            // Identity / date edit (if changed). Run last so the explicitly set
            // committer survives jj stamping it to "now" on the edits above.
            let new_identity = read_identity(&identity_fields);
            if original_identity.borrow().as_ref() != Some(&new_identity) {
                // Prior rewrites may have changed the commit id; re-resolve it.
                if let Some(info) = resolve_commit(&repo, &change_id) {
                    commit_id = info.id;
                }
                let outcome = repo.borrow_mut().rewrite_identity(&commit_id, &new_identity);
                match outcome {
                    Ok(SaveOutcome::Clean) => {}
                    Ok(SaveOutcome::Conflicts { commits }) => {
                        enter_conflict_mode(commits);
                        return;
                    }
                    Err(err) => {
                        show_status(&format!("Identity save failed: {err}"));
                        return;
                    }
                }
            }

            refresh();

            // Restore the selected file and cursor (refresh reset both).
            if let Some(path) = saved_file {
                if let Some(idx) = changes.borrow().iter().position(|c| c.path == path) {
                    file_dropdown.set_selected(idx as u32);
                }
            }
            let offset = saved_cursor.min(file_buffer.char_count());
            let cursor = file_buffer.iter_at_offset(offset);
            file_buffer.place_cursor(&cursor);
            file_view.scroll_to_mark(&file_buffer.get_insert(), 0.0, false, 0.0, 0.0);
            if file_had_focus {
                file_view.grab_focus();
            }
        })
    };

    save_button.connect_clicked({
        let save = save.clone();
        move |_| save()
    });

    // Split: rewrite the selected commit to the edited diff, and insert a new
    // "Split of …" commit holding its original tree right after it. Mirrors the
    // save closure's commit-content path (and its place-restoring reload), but is
    // diff-only — message/identity edits are left for Save. The button is
    // insensitive unless the diff has pending edits, but guard the modes anyway.
    split_button.connect_clicked({
        let repo = repo.clone();
        let commits = commits.clone();
        let changes = changes.clone();
        let current_file = current_file.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let file_dropdown = file_dropdown.clone();
        let selected_change = selected_change.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let pane_mode = pane_mode.clone();
        let viewing_wc = viewing_wc.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        move |_| {
            if pane_mode.borrow().is_conflict() || viewing_wc.get() {
                return;
            }
            let Some(change_id) = selected_change.borrow().clone() else {
                return;
            };
            let target = commits
                .borrow()
                .iter()
                .find(|c| c.change_id_hex() == change_id)
                .map(|c| c.id.clone());
            let Some(commit_id) = target else {
                return;
            };
            let edits = match collect_file_edits(&buffer_text(&file_buffer), &changes.borrow()) {
                Ok(edits) => edits,
                Err(msg) => {
                    show_status(&msg);
                    return;
                }
            };
            if edits.is_empty() {
                return;
            }

            // Remember where the user is so the reload is invisible.
            let saved_file = current_file.borrow().clone();
            let saved_cursor = file_buffer.cursor_position();
            let file_had_focus = file_view.has_focus();

            // Own statement so the `RefMut` drops before the match arms run
            // (`enter_conflict_mode`/`refresh` re-borrow `repo`).
            let outcome = repo.borrow_mut().split_commit(&commit_id, &edits);
            match outcome {
                Ok(SaveOutcome::Clean) => {}
                Ok(SaveOutcome::Conflicts { commits }) => {
                    enter_conflict_mode(commits);
                    return;
                }
                Err(err) => {
                    show_status(&format!("Split failed: {err}"));
                    return;
                }
            }

            refresh();

            // Restore the selected file and cursor (refresh reset both). The
            // selected change id resolves to the edited commit, which kept it.
            if let Some(path) = saved_file {
                if let Some(idx) = changes.borrow().iter().position(|c| c.path == path) {
                    file_dropdown.set_selected(idx as u32);
                }
            }
            let offset = saved_cursor.min(file_buffer.char_count());
            file_buffer.place_cursor(&file_buffer.iter_at_offset(offset));
            file_view.scroll_to_mark(&file_buffer.get_insert(), 0.0, false, 0.0, 0.0);
            if file_had_focus {
                file_view.grab_focus();
            }
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

    // Ctrl+S triggers the same save.
    let save_shortcut = {
        let save = save.clone();
        let action = CallbackAction::new(move |_, _| {
            save();
            glib::Propagation::Stop
        });
        let trigger = ShortcutTrigger::parse_string("<Control>s")
            .expect("valid shortcut trigger");
        Shortcut::new(Some(trigger), Some(action))
    };
    let shortcuts = ShortcutController::new();
    shortcuts.add_shortcut(save_shortcut);
    window.add_controller(shortcuts);

    // Initial population and selection.
    refresh();
    if selected_change.borrow().is_none() {
        if let Some(row) = list.row_at_index(0) {
            list.select_row(Some(&row));
        }
    }

    window.present();
}

/// Split the combined diff buffer into the per-file edits whose reconstructed
/// content differs from the commit's current version. Removed/binary files (no
/// `new_text`) are skipped; the original trailing-newline style is preserved.
/// `Err` carries an apply-failure message (the patch firewall should make that
/// unreachable, but a save surfaces it rather than dropping silently).
fn collect_file_edits(
    combined: &str,
    changes: &[FileChange],
) -> Result<Vec<(String, String)>, String> {
    let mut edits = Vec::new();
    for (path, patch) in split_combined_patch(combined) {
        let Some(change) = changes.iter().find(|c| c.path == path) else {
            continue;
        };
        let Some(original) = change.new_text.as_deref() else {
            continue;
        };
        let old = change.old_text.as_deref().unwrap_or("");
        match apply_patch(old, &patch) {
            Ok(mut content) => {
                if !original.is_empty() && !original.ends_with('\n') && content.ends_with('\n') {
                    content.pop();
                }
                if content != original {
                    edits.push((path, content));
                }
            }
            Err(err) => return Err(format!("Cannot apply edited patch for {path}: {err}")),
        }
    }
    Ok(edits)
}

/// Re-resolve a commit's current id from its rewrite-stable change id.
fn resolve_commit(
    repo: &Rc<RefCell<Repo>>,
    change_id: &str,
) -> Option<commedit_engine::history::CommitInfo> {
    let repo = repo.borrow();
    let head = repo.head_commit_id()?;
    history(&repo.repo, &head)
        .ok()?
        .into_iter()
        .find(|c| c.change_id_hex() == change_id)
}

/// Create the static, named tags used for diff line backgrounds and intra-line
/// emphasis (idempotent). Per-syntax foreground tags are created lazily in
/// [`fg_tag`]. Colors follow GitHub's light diff palette.
fn install_diff_tags(buffer: &sourceview5::Buffer) {
    let table = buffer.tag_table();
    let add = |name: &str, build: &dyn Fn(&TextTag)| {
        if table.lookup(name).is_none() {
            let tag = TextTag::new(Some(name));
            build(&tag);
            table.add(&tag);
        }
    };
    add("add-line", &|t| t.set_paragraph_background(Some("#e6ffec")));
    add("del-line", &|t| t.set_paragraph_background(Some("#ffebe9")));
    add("hunk", &|t| {
        t.set_paragraph_background(Some("#ddf4ff"));
        t.set_foreground(Some("#0550ae"));
    });
    add("meta", &|t| t.set_foreground(Some("#6e7781")));
    add("add-word", &|t| t.set_background(Some("#abf2bc")));
    add("del-word", &|t| t.set_background(Some("#ffc0bd")));
    // Conflict-resolution pane: "our" side, "their" side, and the marker lines.
    add("ours-line", &|t| t.set_paragraph_background(Some("#e6ffec")));
    add("theirs-line", &|t| t.set_paragraph_background(Some("#ddf4ff")));
    add("base-line", &|t| t.set_paragraph_background(Some("#fff8c5")));
    add("conflict-marker", &|t| {
        t.set_paragraph_background(Some("#ffd7d5"));
        t.set_foreground(Some("#cf222e"));
        t.set_weight(700);
    });
    // Inline banner buttons (the conflict "use …" cues and the diff "expand
    // context" cues). Each is an inverse of its host line: a solid body filled in
    // the line's accent colour with the line's background colour as text, end-
    // capped by full-height triangles drawn in the body colour on the bare line
    // background so the ends point outward and stay flush. Added last so the
    // body's text colour outranks the host line's own foreground (GTK tag
    // priority follows tag-table insertion order).
    add("resolve-cue", &|t| {
        t.set_background(Some("#cf222e"));
        t.set_foreground(Some("#ffd7d5"));
        t.set_weight(700);
    });
    add("resolve-cue-cap", &|t| {
        t.set_foreground(Some("#cf222e"));
        t.set_weight(700);
    });
    add("expand-cue", &|t| {
        t.set_background(Some("#0550ae"));
        t.set_foreground(Some("#ddf4ff"));
        t.set_weight(700);
    });
    add("expand-cue-cap", &|t| {
        t.set_foreground(Some("#0550ae"));
        t.set_weight(700);
    });
}

/// Look up (or lazily create and cache, via the buffer's tag table) a foreground
/// color tag for a `#rrggbb` value produced by syntect.
fn fg_tag(buffer: &sourceview5::Buffer, hex: &str) -> TextTag {
    let name = format!("fg{hex}");
    if let Some(tag) = buffer.tag_table().lookup(&name) {
        return tag;
    }
    let tag = TextTag::new(Some(&name));
    tag.set_foreground(Some(hex));
    buffer.tag_table().add(&tag);
    tag
}

/// Re-apply all diff highlighting tags to `buffer` for the unified diff it
/// currently holds: line backgrounds by kind, syntect language coloring of the
/// code portion (keeping separate parser state for the removed/added sides so
/// multi-line constructs stay correct), and intra-line change emphasis.
fn highlight_diff(buffer: &sourceview5::Buffer, path: Option<&str>, ps: &SyntaxSet, theme: &Theme) {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    let text = buffer.text(&start, &end, false).to_string();
    buffer.remove_all_tags(&start, &end);

    let raw_lines: Vec<&str> = text.split('\n').collect();
    let parsed = parse_diff_lines(&text);

    // Pick a syntect syntax from a file extension, falling back to plain text.
    let pick = |p: &str| {
        std::path::Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| ps.find_syntax_by_extension(ext))
            .unwrap_or_else(|| ps.find_syntax_plain_text())
    };
    // The combined buffer holds several files; `path` is only the fallback. The
    // per-section language is re-derived from each `--- a/PATH` header below.
    let mut syntax = path.map(pick).unwrap_or_else(|| ps.find_syntax_plain_text());
    let mut old_hl = HighlightLines::new(syntax, theme);
    let mut new_hl = HighlightLines::new(syntax, theme);

    for (li, line) in parsed.iter().enumerate() {
        let raw = raw_lines[li];
        if let Some(name) = line_bg_tag(line.kind) {
            apply_line_tag(buffer, li as i32, name);
        }
        match line.kind {
            // Hunk boundary: reset both parser states (the shown regions are
            // discontiguous, so state must not leak across the gap).
            DiffLineKind::Hunk => {
                old_hl = HighlightLines::new(syntax, theme);
                new_hl = HighlightLines::new(syntax, theme);
                // Paint the trailing "expand context" cue as a pill button.
                paint_pill(buffer, li as i32, raw, "expand-cue-cap", "expand-cue");
                continue;
            }
            DiffLineKind::Header => {
                // A new file section starts at `--- a/PATH`: switch language and
                // reset the parser state so the previous file doesn't bleed in.
                if let Some(p) = raw.strip_prefix("--- a/") {
                    syntax = pick(p);
                    old_hl = HighlightLines::new(syntax, theme);
                    new_hl = HighlightLines::new(syntax, theme);
                }
                continue;
            }
            DiffLineKind::Meta => continue,
            _ => {}
        }

        let prefix = if raw.is_empty() { 0 } else { 1 };
        let code = &raw[prefix..];
        let owned = format!("{code}\n");
        let spans = match line.kind {
            DiffLineKind::Removed => old_hl.highlight_line(&owned, ps),
            DiffLineKind::Added => new_hl.highlight_line(&owned, ps),
            // Context advances both sides; color from the (identical) new side.
            DiffLineKind::Context => {
                let _ = old_hl.highlight_line(&owned, ps);
                new_hl.highlight_line(&owned, ps)
            }
            _ => continue,
        };
        if let Ok(spans) = spans {
            let mut byte = 0usize;
            for (style, piece) in spans {
                if byte >= code.len() {
                    break;
                }
                let plen = piece.len().min(code.len() - byte); // clip the trailing '\n'
                if plen > 0 {
                    let cs = prefix + code[..byte].chars().count();
                    let ce = prefix + code[..byte + plen].chars().count();
                    let fg = style.foreground;
                    let hex = format!("#{:02x}{:02x}{:02x}", fg.r, fg.g, fg.b);
                    apply_cols(buffer, li as i32, cs as i32, ce as i32, &fg_tag(buffer, &hex));
                }
                byte += plen;
            }
        }

        if !line.intra.is_empty() {
            let word_tag = if line.kind == DiffLineKind::Added {
                "add-word"
            } else {
                "del-word"
            };
            if let Some(tag) = buffer.tag_table().lookup(word_tag) {
                for &(s, e) in &line.intra {
                    let cs = prefix + code[..s].chars().count();
                    let ce = prefix + code[..e].chars().count();
                    apply_cols(buffer, li as i32, cs as i32, ce as i32, &tag);
                }
            }
        }
    }
}

/// Highlight a *conflicted* file (whole-file content with 2-way markers) in
/// `buffer`: a colored background per region (ours/theirs/base), the marker lines
/// emphasized, and syntect language coloring of the code, with the parser state
/// reset at each marker so the discontiguous regions don't bleed into each other.
fn highlight_conflict(
    buffer: &sourceview5::Buffer,
    path: Option<&str>,
    ps: &SyntaxSet,
    theme: &Theme,
) {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    let text = buffer.text(&start, &end, false).to_string();
    buffer.remove_all_tags(&start, &end);

    let raw_lines: Vec<&str> = text.split('\n').collect();
    let kinds = classify_conflict_lines(&text);
    let cue = pill(CONFLICT_CUE_LABEL);

    let pick = |p: &str| {
        std::path::Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| ps.find_syntax_by_extension(ext))
            .unwrap_or_else(|| ps.find_syntax_plain_text())
    };
    // `path` is only the fallback; the combined buffer holds several files and the
    // per-section language is re-derived from each `─── PATH ───` header.
    let mut syntax = path.map(pick).unwrap_or_else(|| ps.find_syntax_plain_text());
    let mut hl = HighlightLines::new(syntax, theme);

    for (li, &kind) in kinds.iter().enumerate() {
        let raw = raw_lines.get(li).copied().unwrap_or("");
        // A file header starts a new section: switch language, reset state, paint
        // it as a header, and skip the content coloring.
        if let Some(p) = conflict_header_path(raw) {
            syntax = pick(p);
            hl = HighlightLines::new(syntax, theme);
            apply_line_tag(buffer, li as i32, "hunk");
            continue;
        }
        // The elision cue is a pill button standing in for a hidden run.
        if raw == cue {
            apply_line_tag(buffer, li as i32, "hunk");
            paint_pill(buffer, li as i32, raw, "expand-cue-cap", "expand-cue");
            continue;
        }
        if raw == CONFLICT_STRUCTURAL_NOTICE {
            apply_line_tag(buffer, li as i32, "meta");
            continue;
        }
        if let Some(name) = conflict_bg_tag(kind) {
            apply_line_tag(buffer, li as i32, name);
        }
        if kind.is_marker() {
            // A marker line is structural; reset the syntax parser so the next
            // region starts clean, and don't language-color the marker itself.
            hl = HighlightLines::new(syntax, theme);
            // Paint the trailing "use ours/theirs/both" cue as a pill button.
            paint_pill(buffer, li as i32, raw, "resolve-cue-cap", "resolve-cue");
            continue;
        }
        // Unlike a unified diff, conflict lines carry no prefix char — column 0
        // is real content.
        let owned = format!("{raw}\n");
        if let Ok(spans) = hl.highlight_line(&owned, ps) {
            let mut byte = 0usize;
            for (style, piece) in spans {
                if byte >= raw.len() {
                    break;
                }
                let plen = piece.len().min(raw.len() - byte);
                if plen > 0 {
                    let cs = raw[..byte].chars().count();
                    let ce = raw[..byte + plen].chars().count();
                    let fg = style.foreground;
                    let hex = format!("#{:02x}{:02x}{:02x}", fg.r, fg.g, fg.b);
                    apply_cols(buffer, li as i32, cs as i32, ce as i32, &fg_tag(buffer, &hex));
                }
                byte += plen;
            }
        }
    }
}

/// The line-background tag name for a conflict line kind (`None` = plain content).
fn conflict_bg_tag(kind: ConflictLineKind) -> Option<&'static str> {
    match kind {
        ConflictLineKind::Ours => Some("ours-line"),
        ConflictLineKind::Theirs => Some("theirs-line"),
        ConflictLineKind::Base => Some("base-line"),
        ConflictLineKind::MarkerOurs
        | ConflictLineKind::MarkerBase
        | ConflictLineKind::MarkerSep
        | ConflictLineKind::MarkerTheirs => Some("conflict-marker"),
        ConflictLineKind::Plain => None,
    }
}

/// The line-background tag name for a diff line kind (`None` = context, no bg).
fn line_bg_tag(kind: DiffLineKind) -> Option<&'static str> {
    match kind {
        DiffLineKind::Added => Some("add-line"),
        DiffLineKind::Removed => Some("del-line"),
        DiffLineKind::Hunk => Some("hunk"),
        DiffLineKind::Header | DiffLineKind::Meta => Some("meta"),
        DiffLineKind::Context => None,
    }
}

/// Apply a named tag across the whole of buffer line `li` (including its newline,
/// so paragraph backgrounds fill the row).
fn apply_line_tag(buffer: &sourceview5::Buffer, li: i32, name: &str) {
    let Some(tag) = buffer.tag_table().lookup(name) else {
        return;
    };
    let Some(s) = buffer.iter_at_line(li) else {
        return;
    };
    let e = buffer.iter_at_line(li + 1).unwrap_or_else(|| buffer.end_iter());
    buffer.apply_tag(&tag, &s, &e);
}

/// Apply `tag` over the character-column range `[cs, ce)` of buffer line `li`.
fn apply_cols(buffer: &sourceview5::Buffer, li: i32, cs: i32, ce: i32, tag: &TextTag) {
    if let (Some(s), Some(e)) = (
        buffer.iter_at_line_offset(li, cs),
        buffer.iter_at_line_offset(li, ce),
    ) {
        buffer.apply_tag(tag, &s, &e);
    }
}

/// A horizontally-expanding text entry with placeholder text, for an identity
/// name/email/date field.
fn identity_entry(placeholder: &str) -> Entry {
    let entry = Entry::new();
    entry.set_placeholder_text(Some(placeholder));
    entry.set_hexpand(true);
    entry
}

/// A date entry with a calendar button to its right, as a single grid cell.
fn date_field(date: &Entry) -> GtkBox {
    let date_box = GtkBox::new(Orientation::Horizontal, 4);
    date_box.append(date);
    date_box.append(&calendar_button(date));
    date_box
}

/// A 📅 menu button whose popover holds a [`Calendar`] that edits the date
/// portion of `entry`, preserving its time-of-day and timezone suffix.
fn calendar_button(entry: &Entry) -> MenuButton {
    let calendar = Calendar::new();
    let popover = Popover::new();
    popover.set_child(Some(&calendar));
    let button = MenuButton::new();
    button.set_icon_name("x-office-calendar-symbolic");
    button.set_popover(Some(&popover));
    button.set_tooltip_text(Some("Pick the date"));

    // Open the calendar on the date currently in the field.
    calendar.connect_map({
        let entry = entry.clone();
        move |cal| {
            if let Some((y, m, d)) = entry_date_parts(&entry.text()) {
                if let Ok(dt) = glib::DateTime::from_local(y, m, d, 0, 0, 0.0) {
                    cal.select_day(&dt);
                }
            }
        }
    });
    calendar.connect_day_selected({
        let entry = entry.clone();
        move |cal| {
            let date = cal.date();
            set_entry_date(&entry, date.year(), date.month(), date.day_of_month());
        }
    });
    button
}

/// Parse the leading `YYYY-MM-DD` of a timestamp field into `(year, month, day)`.
fn entry_date_parts(text: &str) -> Option<(i32, i32, i32)> {
    let date = text.split_whitespace().next()?;
    let mut parts = date.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    Some((year, month, day))
}

/// Replace the date portion of `entry`, keeping its `HH:MM:SS ±HHMM` suffix (or
/// a sensible default when the field has none yet).
fn set_entry_date(entry: &Entry, year: i32, month: i32, day: i32) {
    let text = entry.text();
    let rest = text
        .trim()
        .split_once(' ')
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_else(|| "00:00:00 +0000".to_string());
    entry.set_text(&format!("{year:04}-{month:02}-{day:02} {rest}"));
}

/// Give `entry` a built-in ▼ icon that opens a popover of `identities`; picking
/// one sets the field to its `Name <email>` form.
fn attach_identity_picker(entry: &Entry, identities: &Rc<RefCell<Vec<(String, String)>>>) {
    entry.set_icon_from_icon_name(gtk::EntryIconPosition::Secondary, Some("pan-down-symbolic"));
    entry.set_icon_activatable(gtk::EntryIconPosition::Secondary, true);
    entry.set_icon_tooltip_text(
        gtk::EntryIconPosition::Secondary,
        Some("Use an identity from another commit"),
    );

    let list = ListBox::new();
    let scroll = ScrolledWindow::builder()
        .propagate_natural_height(true)
        .propagate_natural_width(true)
        .min_content_width(280)
        .max_content_height(280)
        .hscrollbar_policy(PolicyType::Never)
        .child(&list)
        .build();
    let popover = Popover::new();
    popover.set_child(Some(&scroll));
    popover.set_parent(entry);

    entry.connect_icon_press({
        let identities = identities.clone();
        let list = list.clone();
        let popover = popover.clone();
        move |_, pos| {
            if pos != gtk::EntryIconPosition::Secondary {
                return;
            }
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            for (name, email) in identities.borrow().iter() {
                let label = Label::builder()
                    .label(join_name_email(name, email))
                    .xalign(0.0)
                    .margin_start(6)
                    .margin_end(6)
                    .build();
                list.append(&label);
            }
            popover.popup();
        }
    });
    list.connect_row_activated({
        let entry = entry.clone();
        let popover = popover.clone();
        move |_, row| {
            if let Some(label) = row.child().and_downcast::<Label>() {
                entry.set_text(&label.text());
            }
            popover.popdown();
        }
    });
}

/// Split a `Name <email>` field into its `(name, email)` parts; an unbracketed
/// value is taken as a bare name.
fn split_name_email(text: &str) -> (String, String) {
    let text = text.trim();
    if let Some(open) = text.rfind('<') {
        if let Some(close) = text[open..].find('>') {
            let name = text[..open].trim().to_string();
            let email = text[open + 1..open + close].trim().to_string();
            return (name, email);
        }
    }
    (text.to_string(), String::new())
}

/// Format `(name, email)` as a `Name <email>` field value.
fn join_name_email(name: &str, email: &str) -> String {
    if email.is_empty() {
        name.to_string()
    } else {
        format!("{name} <{email}>")
    }
}

/// Read the identity entry fields into an [`Identity`]. Field order is
/// `[author "Name <email>", author date, committer "Name <email>", committer date]`.
fn read_identity(fields: &[Entry; 4]) -> Identity {
    let (author_name, author_email) = split_name_email(&fields[0].text());
    let (committer_name, committer_email) = split_name_email(&fields[2].text());
    Identity {
        author_name,
        author_email,
        author_time: fields[1].text().to_string(),
        committer_name,
        committer_email,
        committer_time: fields[3].text().to_string(),
    }
}

/// Populate the identity entry fields from a commit (see [`read_identity`] for
/// the field order).
fn set_identity_fields(fields: &[Entry; 4], commit: &CommitInfo) {
    fields[0].set_text(&join_name_email(&commit.author_name, &commit.author_email));
    fields[1].set_text(&commit.author_time);
    fields[2].set_text(&join_name_email(&commit.committer_name, &commit.committer_email));
    fields[3].set_text(&commit.committer_time);
}

/// A small popover anchored at `target_row` letting the user pick how to merge
/// an unprefixed commit dropped onto another: Fixup / Squash / Amend, or Cancel.
/// Each verb runs `apply(mode)` and dismisses; Cancel (or a click outside) just
/// dismisses. Shown from the post-drag idle, where the row is alive and GTK's
/// drag bookkeeping is already torn down.
fn show_squash_popover(target_row: &ListBoxRow, apply: &Rc<dyn Fn(SquashMode)>) {
    let popover = Popover::new();
    let vbox = GtkBox::new(Orientation::Vertical, 0);
    let button = |label: &str, tip: &str| {
        let b = Button::with_label(label);
        b.add_css_class("flat");
        b.set_tooltip_text(Some(tip));
        b.set_halign(gtk::Align::Fill);
        vbox.append(&b);
        b
    };
    let fixup_btn = button("Fixup", "Merge changes in; keep this commit's message.");
    let squash_btn = button("Squash", "Merge changes in; append the dragged commit's message.");
    let amend_btn = button(
        "Amend",
        "Merge changes in; replace this commit's message with the dragged commit's.",
    );
    vbox.append(&gtk::Separator::new(Orientation::Horizontal));
    let cancel_btn = button("Cancel", "Don't merge — leave history unchanged.");

    popover.set_child(Some(&vbox));
    // Parent to the list (the row's container), NOT the row itself: a *selected*
    // target row carries the selected-state foreground (white), which the
    // popover's button labels would inherit through the widget tree — leaving
    // white-on-grey, unreadable text. The list carries the normal theme colors.
    // Point the popover at the row's allocation (in list coordinates) so it
    // still anchors at the drop target.
    if let Some(parent) = target_row.parent() {
        popover.set_parent(&parent);
        popover.set_pointing_to(Some(&target_row.allocation()));
    } else {
        popover.set_parent(target_row);
    }
    popover.set_autohide(true);

    let wire = |btn: &Button, mode: Option<SquashMode>| {
        let apply = apply.clone();
        let popover = popover.clone();
        btn.connect_clicked(move |_| {
            if let Some(mode) = mode {
                apply(mode);
            }
            popover.popdown();
        });
    };
    wire(&fixup_btn, Some(SquashMode::Fixup));
    wire(&squash_btn, Some(SquashMode::Squash));
    wire(&amend_btn, Some(SquashMode::Amend));
    wire(&cancel_btn, None);

    // Detach when dismissed (verb click or outside-click) so a popover doesn't
    // leak per drop.
    popover.connect_closed(|p| p.unparent());
    popover.popup();
}

/// Build the `short-id   subject   ⚠` content box shown inside a history/trash
/// row. The trailing warning icon is present but hidden unless `conflicted`.
fn commit_row_box(commit: &CommitInfo, conflicted: bool) -> GtkBox {
    let short = commit.id_hex().chars().take(8).collect::<String>();
    let subject = if commit.subject.is_empty() {
        "(no description)"
    } else {
        &commit.subject
    };
    let id_label = Label::builder().xalign(0.0).build();
    id_label.set_markup(&format!("<tt>{short}</tt>"));
    let subject_label = Label::builder()
        .label(subject)
        .xalign(0.0)
        .halign(gtk::Align::Fill)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let badge = gtk::Image::from_icon_name("dialog-warning-symbolic");
    badge.set_tooltip_text(Some("This commit has unresolved conflicts"));
    badge.set_visible(conflicted);
    let row_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .margin_start(8)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    row_box.append(&id_label);
    row_box.append(&subject_label);
    row_box.append(&badge);
    row_box
}

/// Update the content of a row's existing labels in place, without replacing the
/// child widget — so the labels (and the row) survive a drag-triggered rebuild.
/// Falls back to building a fresh child if the row has none yet.
fn set_row_commit(row: &ListBoxRow, commit: &CommitInfo, conflicted: bool) {
    let short = commit.id_hex().chars().take(8).collect::<String>();
    let subject = if commit.subject.is_empty() {
        "(no description)"
    } else {
        &commit.subject
    };
    let row_box = row.child().and_downcast::<GtkBox>();
    let id_label = row_box
        .as_ref()
        .and_then(|b| b.first_child())
        .and_downcast::<Label>();
    let subject_label = id_label
        .as_ref()
        .and_then(|l| l.next_sibling())
        .and_downcast::<Label>();
    let badge = row_box
        .as_ref()
        .and_then(|b| b.last_child())
        .and_downcast::<gtk::Image>();
    match (id_label, subject_label, badge) {
        (Some(id_label), Some(subject_label), Some(badge)) => {
            id_label.set_markup(&format!("<tt>{short}</tt>"));
            subject_label.set_text(subject);
            badge.set_visible(conflicted);
        }
        // Older row layout (or a freshly-created empty row): build it whole.
        _ => row.set_child(Some(&commit_row_box(commit, conflicted))),
    }
}

/// Show one row per commit, **reusing** the existing `ListBoxRow` widgets in
/// place (updating their labels) and **hiding** — never removing — any surplus
/// rows.
///
/// This is load-bearing for drag-and-drop, not a micro-optimization. A drop
/// rewrites history and repopulates immediately afterwards, while GTK still
/// holds one of these rows as the drop-crossing target for the just-ended
/// gesture (the trash sits below the list, so dragging to it leaves a lower row
/// cached). GTK walks that cached widget's parent chain on the next pointer
/// event; if we had unparented it (by removing or clearing rows) the walk hits a
/// NULL parent and segfaults. Reusing rows and only *hiding* the surplus keeps
/// every row parented, so the walk always terminates. Hidden rows are reused
/// when the list grows again.
fn populate_rows(
    list: &ListBox,
    commits: &[CommitInfo],
    selectable: bool,
    conflicts: &HashSet<String>,
) {
    for (i, commit) in commits.iter().enumerate() {
        let row = list.row_at_index(i as i32).unwrap_or_else(|| {
            let row = ListBoxRow::new();
            // Trash rows aren't editable; they exist only to be dragged back out.
            row.set_selectable(selectable);
            row.set_activatable(selectable);
            list.append(&row);
            row
        });
        row.set_visible(true);
        set_row_commit(&row, commit, conflicts.contains(&commit.change_id_hex()));
    }
    // Hide surplus rows rather than removing them (see the note above).
    let mut i = commits.len() as i32;
    while let Some(extra) = list.row_at_index(i) {
        extra.set_visible(false);
        i += 1;
    }
}

/// Show the history commits in `list` (newest first), reusing rows. See
/// [`populate_rows`].
fn populate_list(list: &ListBox, commits: &[CommitInfo], conflicts: &HashSet<String>) {
    populate_rows(list, commits, true, conflicts);
}

/// Fill the trash list with the session's dropped commits, reusing rows. When
/// empty, the scrolled list is hidden so the panel collapses to just its trash
/// icon (the icon still carries the drop target).
fn populate_trash(list: &ListBox, scroll: &ScrolledWindow, commits: &[CommitInfo]) {
    scroll.set_visible(!commits.is_empty());
    populate_rows(list, commits, false, &HashSet::new());
}

fn present_error(app: &Application, message: &str) {
    let label = Label::builder()
        .label(message)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(16)
        .wrap(true)
        .build();
    let window = ApplicationWindow::builder()
        .application(app)
        .title("commedit — error")
        .default_width(600)
        .default_height(200)
        .child(&label)
        .build();
    window.present();
}
