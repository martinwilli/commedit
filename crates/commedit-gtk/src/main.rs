//! commedit GTK4 UI (Milestone 2): browse history, edit a commit message, and
//! edit the content of files a commit changes. Saving transparently rewrites the
//! commit and rebases descendants via the engine.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::diff::{
    apply_patch, commit_changes, reconstruct_conflict_file,
    render_commit_diff, render_conflict_snippets, revert_groups, split_combined_patch,
    CombinedFile, ContextExpansion, FileChange, HunkInfo,
};
use commedit_engine::history::{history, history_limited, CommitInfo};
use commedit_engine::patch_edit::{
    collapse_diff, deletion_is_safe, plan_edit, Cursor, EditGesture, EditPlan, Selection,
};
use commedit_engine::repo::Repo;
use commedit_engine::rewrite::Identity;
use commedit_engine::workcopy::WorkingCopyEntry;
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    gdk, Application, ApplicationWindow, Box as GtkBox, Button, CallbackAction,
    DropDown, Entry, EventControllerKey, EventControllerScroll,
    EventControllerScrollFlags, Grid, HeaderBar, Label, ListBox, ListBoxRow,
    Orientation, Paned, PolicyType, PropagationPhase, ScrolledWindow, Shortcut,
    ShortcutController, ShortcutTrigger, Stack, StringList, ToggleButton,
};
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

mod state;
use crate::state::*;
mod buffer_util;
use crate::buffer_util::*;
mod highlight;
use crate::highlight::*;
mod rows;
use crate::rows::*;
mod identity;
use crate::identity::*;
mod conflict;
use crate::conflict::*;
mod dragdrop;

/// The [`DiffCue`] a click/hover at buffer `(line, col)` lands on, if it falls on
/// one of the diff view's inline pills. `line_text` is that line's text; `col` is
/// a character offset. The single hit test shared by the click gesture (which
/// acts on it) and the hover cursor (which shows a hand over it), restricting both
/// to the pill rather than the whole line. A `@@` line may carry two pills
/// (expand + revert), so the cue is resolved by the clicked pill's label, not its
/// position.
fn diff_cue_at(hunks: &[HunkInfo], line_text: &str, line: usize, col: usize) -> Option<DiffCue> {
    let (_, _, label) = pills_on_line(line_text)
        .into_iter()
        .find(|(lc, rc, _)| col >= *lc && col <= *rc)?;
    if label == REVERT_FILE_LABEL {
        return Some(DiffCue::RevertFile);
    }
    let hunk = hunks.iter().find(|h| h.header_line == line)?;
    if label == REVERT_HUNK_LABEL {
        Some(DiffCue::RevertHunk(hunk.first_group, hunk.last_group))
    } else if hunk.can_expand_up || hunk.can_expand_down {
        Some(DiffCue::Expand(hunk.first_group, hunk.last_group))
    } else {
        None
    }
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

fn main() {
    let repo_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, repo_path.clone()));
    app.run_with_args::<&str>(&[]);
}

/// Build the diff pane's full buffer text — the combined unified diff for
/// `changes` with each expandable `@@` header's inline "expand context" cue
/// appended — together with the hunks (for hit-testing the cues) and the per-
/// file placement. Appending the cue at a line's *end* keeps every `header_line`
/// valid, so the returned hunks/files match the text exactly. Shared by the full
/// (`set_text`) render and the in-place spliced re-render.
fn build_diff_buffer_text(
    changes: &[FileChange],
    expansions: &HashMap<String, ContextExpansion>,
) -> (String, Vec<HunkInfo>, Vec<CombinedFile>) {
    let combined = render_commit_diff(changes, expansions);
    let mut lines: Vec<String> = combined.text.split('\n').map(str::to_string).collect();
    // A file's changes can be reverted only if both sides exist as text — i.e. a
    // *modified* file. Added (no old) / removed (no new) files are excluded: the
    // content-only edit path can't delete or recreate a path. (`file.editable`
    // alone would still include additions, whose old side is absent.)
    let revertable = |path: &str| {
        changes
            .iter()
            .find(|c| c.path == path)
            .is_some_and(|c| c.old_text.is_some() && c.new_text.is_some())
    };
    let mut all_hunks: Vec<HunkInfo> = Vec::new();
    for file in &combined.files {
        let revert = file.editable && revertable(&file.path);
        for hunk in &file.hunks {
            if let Some(l) = lines.get_mut(hunk.header_line) {
                match (hunk.can_expand_up, hunk.can_expand_down) {
                    (true, true) => l.push_str(&format!("  {}", pill("↕ expand context"))),
                    (true, false) => l.push_str(&format!("  {}", pill("↑ expand context"))),
                    (false, true) => l.push_str(&format!("  {}", pill("↓ expand context"))),
                    (false, false) => {}
                }
                if revert {
                    l.push_str(&format!("  {}", pill(REVERT_HUNK_LABEL)));
                }
            }
            all_hunks.push(hunk.clone());
        }
        // The "revert file" cue rides the `diff --git` separator. Only where there
        // is something to revert (an editable, modified file with hunks).
        if revert && !file.hunks.is_empty() {
            if let Some(l) = lines.get_mut(file.start_line) {
                l.push_str(&format!("  {}", pill(REVERT_FILE_LABEL)));
            }
        }
    }
    (lines.join("\n"), all_hunks, combined.files)
}

fn build_ui(app: &Application, repo_path: PathBuf) {
    let repo = match Repo::open(&repo_path) {
        Ok(repo) => Rc::new(RefCell::new(repo)),
        Err(err) => {
            present_error(app, &format!("Failed to open {repo_path:?}:\n{err:?}"));
            return;
        }
    };

    // Shared UI state.
    let commits: Rc<RefCell<Vec<CommitInfo>>> = Rc::new(RefCell::new(Vec::new()));
    // How many history rows the normal (non-conflict) view currently loads, and
    // whether older commits remain below them. `refresh` reads the limit and sets
    // the flag; scrolling near the bottom bumps the limit by `HISTORY_PAGE`.
    let history_limit: Rc<Cell<usize>> = Rc::new(Cell::new(HISTORY_PAGE));
    let history_has_more: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let selected_change: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let changes: Rc<RefCell<Vec<FileChange>>> = Rc::new(RefCell::new(Vec::new()));
    // The *render baseline*: `changes` holds the content currently shown, which a
    // revert mutates (a hunk/file dropped back to its old side) so the change
    // survives a re-render. `orig_changes` keeps the commit's pristine content so
    // save-detection (`collect_file_edits`) sees a revert as a divergence to apply.
    // With no reverts the two are equal, so manual-edit behaviour is unchanged.
    let orig_changes: Rc<RefCell<Vec<FileChange>>> = Rc::new(RefCell::new(Vec::new()));
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
    // A trash add/remove held back while a conflicted drop/restore is resolved —
    // applied on a clean resolution, discarded on abort (see `PendingTrashOp`).
    let pending_trash_op: Rc<RefCell<Option<PendingTrashOp>>> = Rc::new(RefCell::new(None));
    // Which list the in-flight drag started in, set on drag prepare.
    let drag_origin: Rc<Cell<DragOrigin>> = Rc::new(Cell::new(DragOrigin::History));
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

    // The drag-and-drop insertion-gap placeholder (its styling is in the CSS
    // above). Constructed here with the other drag state; wired in `dragdrop`.
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

    // --- History pane (left) ---
    // The working-copy rows: read-only entries above the history showing the
    // uncommitted changes (jj's `@` commit and any pieces split off it). They are
    // their own list — not part of the history `list` — so the reorder/drop/squash
    // index arithmetic below is untouched. Each row can be *dragged onto* a commit
    // to fold its changes in as a fixup, but never reordered into history. Hidden
    // while the tree is clean. `wc_entries` mirrors the rows (newest first, the
    // leaf `@` first); `selected_wc_change` is the entry the diff pane shows.
    let wc_entries: Rc<RefCell<Vec<WorkingCopyEntry>>> = Rc::new(RefCell::new(Vec::new()));
    let selected_wc_change: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let wc_list = ListBox::new();
    wc_list.set_visible(false);
    wc_list.set_tooltip_text(Some(
        "Uncommitted working-tree changes — edit the diff here (Save writes the working \
         tree), Split to peel off a piece, or drag a row onto a commit to fold it in",
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
    trash_header.set_tooltip_text(Some(
        "Trash — drop a commit here to remove it (drag it back to restore), \
         or drop uncommitted changes here to discard them",
    ));
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
    // follow-up "fixup! …" commit; enabled only while the diff has pending edits
    // (wired by `update_split_sensitivity`), never in the conflict/working-copy views.
    let split_button = Button::with_label("Split");
    split_button.set_tooltip_text(Some(SPLIT_HINT));
    split_button.set_sensitive(false);
    // Conflict-mode quick resolution is driven inline: clicking a block's marker
    // line (with its "use ours/theirs/both" cue) keeps that side — see
    // `with_resolve_cues` and the click gesture below. No toolbar buttons.
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

    // Bundle the state the peeled modules (`dragdrop`, `conflict`) capture: these
    // are clones of the locals above — both point at the same `Rc`s/widgets — so
    // the closures left in `build_ui` and the moved ones share one source of truth.
    // `Callbacks` is assembled later, once its callback members exist.
    let widgets = Widgets {
        list: list.clone(),
        placeholder: placeholder.clone(),
        trash_list: trash_list.clone(),
        trash_scroll: trash_scroll.clone(),
        trash_box: trash_box.clone(),
        wc_list: wc_list.clone(),
        file_buffer: file_buffer.clone(),
        file_view: file_view.clone(),
        save_button: save_button.clone(),
        prev_conflict_button: prev_conflict_button.clone(),
        next_conflict_button: next_conflict_button.clone(),
        conflict_banner: conflict_banner.clone(),
        conflict_label: conflict_label.clone(),
        abort_button: abort_button.clone(),
    };
    let data = Data {
        repo: repo.clone(),
        commits: commits.clone(),
        trashed: trashed.clone(),
        pending_trash_op: pending_trash_op.clone(),
        wc_entries: wc_entries.clone(),
        selected_change: selected_change.clone(),
        pane_mode: pane_mode.clone(),
        conflict_view: conflict_view.clone(),
    };
    let drag_state = DragState {
        drag_origin: drag_origin.clone(),
        drag_row: drag_row.clone(),
        drag_from: drag_from.clone(),
        drop_gap: drop_gap.clone(),
        drop_onto: drop_onto.clone(),
        post_drag: post_drag.clone(),
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

    // Hunks of the diff currently in the buffer, so an expand click can hit-test
    // the cue it lands on and re-render that hunk's widened context.
    let rendered_hunks: Rc<RefCell<Vec<HunkInfo>>> = Rc::new(RefCell::new(Vec::new()));
    // The conflict pane's "expand hidden lines" action, late-bound (it needs the
    // conflict renderer defined below). The expand-click gesture invokes it by
    // buffer line, mirroring the diff renderer.
    let conflict_expand_cell: Rc<RefCell<Option<Rc<dyn Fn(usize)>>>> =
        Rc::new(RefCell::new(None));
    // Push `new_text` (built by `build_diff_buffer_text`) into the buffer and
    // refresh the derived state + highlighting. `replace` chooses how the text
    // lands: a full `set_text` (used for a fresh load, where resetting the scroll
    // to the top is wanted) or an in-place `splice` (used when widening context,
    // where the scroll must stay put). The cue is handled by a GestureClick on
    // the view, not an embedded widget — removing a real widget on the next
    // render crashes GTK.
    let apply_diff_text: Rc<dyn Fn(String, Vec<HunkInfo>, Vec<CombinedFile>, bool)> = {
        let combined_files = combined_files.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let editing = editing.clone();
        let rendered_hunks = rendered_hunks.clone();
        let highlight = highlight.clone();
        Rc::new(move |text: String, all_hunks, files: Vec<CombinedFile>, splice: bool| {
            editing.set(true);
            if splice {
                splice_buffer_text(&file_buffer, &text);
            } else {
                file_buffer.set_text(&text);
            }
            file_view.set_editable(files.iter().any(|f| f.editable));
            *rendered_hunks.borrow_mut() = all_hunks;
            *combined_files.borrow_mut() = files;
            // Highlight in this same main-loop turn, before GTK paints, so the
            // diff appears once fully colored instead of flashing plain first and
            // then re-highlighting via the debounced `changed` handler (which is
            // suppressed while `editing` is set).
            highlight();
            editing.set(false);
        })
    };
    // Full render: every file's diff in one buffer, files separated by
    // `diff --git` lines. `set_text`s the buffer (scroll resets to the top).
    let render_diff_view: Renderer = {
        let changes = changes.clone();
        let expansions = expansions.clone();
        let apply_diff_text = apply_diff_text.clone();
        Rc::new(move || {
            let (text, hunks, files) =
                build_diff_buffer_text(&changes.borrow(), &expansions.borrow());
            apply_diff_text(text, hunks, files, false);
        })
    };
    // In-place re-render after widening a hunk's context: splices only the
    // changed span so GTK keeps the scroll exactly where it is. Expansion only
    // adds/changes lines at or below the clicked header — everything above is a
    // common prefix — so the header stays put and the new context grows below it.
    let rerender_diff_spliced: Renderer = {
        let changes = changes.clone();
        let expansions = expansions.clone();
        let apply_diff_text = apply_diff_text.clone();
        Rc::new(move || {
            let (text, hunks, files) =
                build_diff_buffer_text(&changes.borrow(), &expansions.borrow());
            apply_diff_text(text, hunks, files, true);
        })
    };

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
        let rerender_diff_spliced = rerender_diff_spliced.clone();
        let combined_files = combined_files.clone();
        let changes = changes.clone();
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
            // Only the inline pill cues are clickable, not the whole line.
            let col = iter.line_offset() as usize;
            let line_text = buffer_line_text(&file_buffer, line);
            let Some(cue) = diff_cue_at(&rendered_hunks.borrow(), &line_text, line, col) else {
                return;
            };
            // The combined diff holds several files; find which one owns the
            // clicked line — its `diff --git` separator (revert file) or one of its
            // `@@` headers (expand / revert hunk). Group indices and reverts are
            // file-relative.
            let path = combined_files
                .borrow()
                .iter()
                .find(|f| f.start_line == line || f.hunks.iter().any(|h| h.header_line == line))
                .map(|f| f.path.clone());
            let Some(path) = path else { return };
            // We own this click: don't let the view also place the caret.
            gesture.set_state(gtk::EventSequenceState::Claimed);

            let expansions = expansions.clone();
            let rerender_diff_spliced = rerender_diff_spliced.clone();
            let nav_sync = nav_sync.clone();
            let changes = changes.clone();
            glib::idle_add_local_once(move || {
                // Apply the cue and re-render in place (the spliced re-render edits
                // only the changed span, so GTK keeps the scroll where it sat — no
                // jump-to-top flash; see `splice_buffer_text`). Expansion only adds
                // context. A revert mutates the *render baseline* (`changes`) so the
                // dropped change survives later re-renders; Save/Split then see it as
                // a divergence from the pristine `orig_changes`. The re-render fires
                // `changed`, refreshing Split's sensitivity. Guard with `nav_sync`
                // so the settle doesn't flip the file dropdown.
                nav_sync.set(true);
                match cue {
                    DiffCue::Expand(first, last) => {
                        expansions
                            .borrow_mut()
                            .entry(path.clone())
                            .or_default()
                            .expand(first, last);
                    }
                    DiffCue::RevertHunk(first, last) => {
                        let mut ch = changes.borrow_mut();
                        if let Some(c) = ch.iter_mut().find(|c| c.path == path) {
                            let old = c.old_text.clone().unwrap_or_default();
                            let new = c.new_text.clone().unwrap_or_default();
                            c.new_text = Some(revert_groups(&old, &new, first, last));
                        }
                    }
                    DiffCue::RevertFile => {
                        let mut ch = changes.borrow_mut();
                        if let Some(c) = ch.iter_mut().find(|c| c.path == path) {
                            c.new_text = c.old_text.clone();
                        }
                    }
                }
                rerender_diff_spliced();
                nav_sync.set(false);
            });
        }
    });
    file_view.add_controller(expand_click);

    // Hover cursor: show a hand over the clickable affordances — the conflict
    // "use …" buttons and the diff "expand context" / "revert" pills — and the
    // text I-beam everywhere else. GtkTextView otherwise only ever shows the I-beam over
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
                    diff_cue_at(&rendered_hunks.borrow(), &line_text, line, col).is_some()
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
    // (the same edits Save would apply, via `collect_file_edits`) — never in the
    // conflict view, but now also for a working-copy entry (Split peels it). Runs
    // on every buffer change below, so it also resets to insensitive after a
    // (re)load renders a fresh, unedited diff.
    let update_split_sensitivity: Rc<dyn Fn()> = {
        let split_button = split_button.clone();
        let file_buffer = file_buffer.clone();
        let orig_changes = orig_changes.clone();
        let pane_mode = pane_mode.clone();
        Rc::new(move || {
            let has_edits = !pane_mode.borrow().is_conflict()
                && matches!(
                    collect_file_edits(&buffer_text(&file_buffer), &orig_changes.borrow()),
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
    // `splice` chooses how the rebuilt text lands: a full `set_text` (fresh
    // load, where resetting the scroll to the top is wanted) or an in-place
    // splice (expanding a gap, where the scroll must stay put) — mirroring the
    // diff pane's `apply_diff_text`.
    let render_conflict_view: Rc<dyn Fn(bool)> = {
        let conflict_view = conflict_view.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let editing = editing.clone();
        let highlight = highlight.clone();
        Rc::new(move |splice: bool| {
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
            // Build the full text including the inline "use ours/theirs/both" cues
            // up front, so a splice diffs against the same content set_text yields.
            let text = with_resolve_cues(&out.join("\n"));
            editing.set(true);
            if splice {
                splice_buffer_text(&file_buffer, &text);
            } else {
                file_buffer.set_text(&text);
            }
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
    // widen that gap's context, and re-render in place — the conflict-pane
    // analogue of the diff expand handler. The spliced re-render edits only the
    // changed span, so GTK keeps the scroll where it is: the clicked cue stays
    // put and the revealed lines grow around it, with no re-pin math and no
    // jump-to-top flash (see `splice_buffer_text`). `nav_sync` guards the rebuild
    // so it doesn't flip the file dropdown.
    let conflict_expand: Rc<dyn Fn(usize)> = {
        let conflict_view = conflict_view.clone();
        let sync_conflict_from_buffer = sync_conflict_from_buffer.clone();
        let render_conflict_view = render_conflict_view.clone();
        let file_buffer = file_buffer.clone();
        let nav_sync = nav_sync.clone();
        Rc::new(move |line: usize| {
            let Some((fi, k)) = conflict_cue_gap_at(&file_buffer, line) else {
                return;
            };
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
            render_conflict_view(true);
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
            render_conflict_view(false);
            file_dropdown.set_selected(0);
            scroll_to_conflict_file(0);
            if let Some(&first) = conflict_block_lines(&file_buffer).first() {
                scroll_to_line(&file_view, &file_buffer, first);
            }
        })
    };


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
        let orig_changes = orig_changes.clone();
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
            // Set the pristine baseline before any render: `set_text` synchronously
            // fires `changed` -> `update_split_sensitivity`, which reads it.
            *orig_changes.borrow_mut() = loaded.clone();
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

    // Load the selected working-copy entry's diff into the file pane (read-only).
    // The entry is named by its stable change id; falls back to the leaf `@` (the
    // first chain entry) when nothing is selected.
    let load_wc_changes: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let apply_changes = apply_changes.clone();
        let selected_wc_change = selected_wc_change.clone();
        Rc::new(move || {
            let loaded = {
                let r = repo.borrow();
                let chain = r.working_copy_chain();
                let want = selected_wc_change.borrow().clone();
                let entry = want
                    .and_then(|ch| chain.iter().find(|e| e.info.change_id_hex() == ch))
                    .or_else(|| chain.first());
                match entry {
                    Some(e) => commit_changes(&r.repo, &e.info.id).unwrap_or_default(),
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

    // Selecting a working-copy entry shows its diff read-only: there is no
    // message or identity to edit, and Save edits that entry in place (see the
    // `save` closure). The selected entry is tracked by its stable change id.
    wc_list.connect_row_selected({
        let viewing_wc = viewing_wc.clone();
        let list = list.clone();
        let load_wc_changes = load_wc_changes.clone();
        let message_buffer = message_buffer.clone();
        let message_view = message_view.clone();
        let identity_fields = identity_fields.clone();
        let pane_mode = pane_mode.clone();
        let wc_entries = wc_entries.clone();
        let selected_wc_change = selected_wc_change.clone();
        move |_wc_list, row| {
            let Some(row) = row else { return };
            if pane_mode.borrow().is_conflict() {
                return;
            }
            let idx = row.index();
            if idx < 0 {
                return;
            }
            let change = wc_entries
                .borrow()
                .get(idx as usize)
                .map(|e| e.info.change_id_hex());
            let Some(change) = change else { return };
            *selected_wc_change.borrow_mut() = Some(change);
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

    // Update the working-copy rows from the engine: one per uncommitted entry when
    // the tree is dirty, hidden when clean or while resolving conflicts. Drops a
    // stale selection that no longer names a live entry.
    let refresh_wc: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let wc_list = wc_list.clone();
        let wc_entries = wc_entries.clone();
        let selected_wc_change = selected_wc_change.clone();
        let viewing_wc = viewing_wc.clone();
        let pane_mode = pane_mode.clone();
        Rc::new(move || {
            if pane_mode.borrow().is_conflict() {
                wc_list.set_visible(false);
                return;
            }
            let chain = repo.borrow().working_copy_chain();
            let visible = !chain.is_empty();
            populate_wc(&wc_list, &chain);
            let still_present = selected_wc_change
                .borrow()
                .as_ref()
                .map(|ch| chain.iter().any(|e| e.info.change_id_hex() == *ch))
                .unwrap_or(false);
            *wc_entries.borrow_mut() = chain;
            wc_list.set_visible(visible);
            // If the viewed entry is gone (folded away, or the tree went clean),
            // forget it so a later edit/split doesn't target the wrong entry.
            if !still_present {
                selected_wc_change.borrow_mut().take();
                if !visible {
                    viewing_wc.set(false);
                }
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
    let refresh_conflict = conflict::build_refresh_conflict(&widgets, &data);

    // Leave conflict mode: back to the normal diff pane, banner hidden.
    let exit_conflict_mode = conflict::build_exit_conflict_mode(&widgets, &data);

    // Enter conflict mode with the engine's reported conflicts: show the banner,
    // select the oldest conflicted commit, and render the pending chain. The
    // quick-resolve affordances are the inline marker-line cues (see
    // `with_resolve_cues`).
    let enter_conflict_mode =
        conflict::build_enter_conflict_mode(&widgets, &data, refresh_conflict.clone());

    // The late-bound callbacks the peeled modules invoke. Assembled here, after
    // its members exist, and handed to `dragdrop`/`conflict` by reference.
    let callbacks = Callbacks {
        refresh: refresh.clone(),
        show_status: show_status.clone(),
        enter_conflict_mode: enter_conflict_mode.clone(),
        exit_conflict_mode: exit_conflict_mode.clone(),
    };

    // Wire the conflict-mode events (abort + previous/next-conflict navigation).
    conflict::wire(&widgets, &data, &callbacks);

    // Resolve the conflicted file currently in the buffer. The engine re-checks
    // the whole chain: when the last conflict clears it exports the rewrite and we
    // return to the normal view, otherwise the remaining conflicts are re-shown.
    let resolve_current = conflict::build_resolve_current(
        &widgets,
        &data,
        refresh.clone(),
        refresh_conflict.clone(),
        exit_conflict_mode.clone(),
        show_status.clone(),
        sync_conflict_from_buffer.clone(),
    );

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
        let pending_trash_op = pending_trash_op.clone();
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
            let pending_trash_op = pending_trash_op.clone();
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
                    // The session's drops are undone, so empty the trash bin and
                    // drop any trash change a held-back rewrite was waiting on.
                    pending_trash_op.borrow_mut().take();
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

    dragdrop::wire(&widgets, &data, &drag_state, &callbacks);
    populate_trash(&trash_list, &trash_scroll, &trashed.borrow());

    // Save: rewrite the message and/or the selected file's content, then reload.
    // Reloading re-selects the commit, which cascades through `row-selected` ->
    // `load_changes` and re-renders the diff from the top. We capture the scroll
    // position (as a fraction of the range, the only anchor that survives the
    // re-render's line shifts) beforehand and re-pin it afterwards, so a save is
    // invisible to the user's place in the diff.
    let save: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let commits = commits.clone();
        let changes = changes.clone();
        let orig_changes = orig_changes.clone();
        let current_file = current_file.clone();
        let message_buffer = message_buffer.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let file_dropdown = file_dropdown.clone();
        let nav_sync = nav_sync.clone();
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
        let selected_wc_change = selected_wc_change.clone();
        Rc::new(move || {
            // In conflict mode, "Save" means "resolve the current conflicted file".
            if pane_mode.borrow().is_conflict() {
                resolve_current();
                return;
            }
            // Viewing a working-copy entry: edit it in place (no message/identity,
            // and the branch tip doesn't move), then reload the diff and rows.
            if viewing_wc.get() {
                let saved_file = current_file.borrow().clone();
                let saved_cursor = file_buffer.cursor_position();
                // Edit each changed file of the selected entry in place (no rebase
                // that moves the tip, so a loop is fine).
                let edits = match collect_file_edits(&buffer_text(&file_buffer), &orig_changes.borrow()) {
                    Ok(edits) => edits,
                    Err(msg) => {
                        show_status(&msg);
                        return;
                    }
                };
                let change = selected_wc_change.borrow().clone();
                for (path, content) in &edits {
                    if let Err(err) =
                        repo.borrow_mut().edit_working_copy_file(change.as_deref(), path, content)
                    {
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
            // The reload re-renders the diff from scratch (context expansions
            // cleared, edits applied), so the buffer's length and absolute line
            // numbers change — restoring a saved line/offset overshoots the now
            // shorter buffer and scrolls to the end. A *fraction* of the
            // scrollable range survives the re-render; the (uniform, monospace,
            // unwrapped) line height lets us recompute the post-render offset
            // arithmetically rather than wait for GTK's deferred layout.
            let scroll_frac = file_view.vadjustment().map(|v| {
                let range = (v.upper() - v.page_size()).max(1.0);
                (v.value() / range).clamp(0.0, 1.0)
            });
            let line_height = file_view.iter_location(&file_buffer.start_iter()).height() as f64;
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
            let edits = match collect_file_edits(&buffer_text(&file_buffer), &orig_changes.borrow()) {
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

            // Reload, keeping the diff visually where it was. `refresh` re-selects
            // the commit, cascading through `row-selected` -> `load_changes` ->
            // `render_diff_view`, which `set_text`s the buffer (resetting the scroll
            // to the top) and calls `scroll_to_file(0)`. We guard the whole reload
            // with `nav_sync` so that scroll-to-top — and the scroll->dropdown sync —
            // is suppressed and queues no competing deferred scroll, then re-pin the
            // scroll ourselves to the fraction captured above.
            nav_sync.set(true);
            refresh();
            if let (Some(frac), Some(vadj)) = (scroll_frac, file_view.vadjustment()) {
                let page = vadj.page_size();
                if line_height > 0.0 && page > 0.0 {
                    // `set_text` left the adjustment's range stale (layout validates
                    // on a later frame). Recompute it arithmetically and set the
                    // offset synchronously, before GTK paints, so the saved fraction
                    // shows on the next frame instead of a jump-to-top flash; GTK's
                    // own validation later sets the same values, leaving it put.
                    let top = file_view.top_margin() as f64;
                    let bottom = file_view.bottom_margin() as f64;
                    let height = file_buffer.line_count() as f64 * line_height + top + bottom;
                    let upper = height.max(page);
                    let target = (frac * (upper - page)).clamp(0.0, (upper - page).max(0.0));
                    vadj.set_upper(upper);
                    vadj.set_value(target);
                    // Sync the dropdown to the file now at the top of the viewport
                    // (refresh reset it to the first file), and put the cursor on a
                    // visible line so grab_focus / validation don't scroll it away.
                    let top_line = ((target - top) / line_height).max(0.0) as usize;
                    if let Some(iter) = file_buffer.iter_at_line(top_line as i32) {
                        file_buffer.place_cursor(&iter);
                    }
                    file_dropdown.set_selected(diff_file_index_at_line(&file_buffer, top_line) as u32);
                }
            }
            nav_sync.set(false);
            if file_had_focus {
                file_view.grab_focus();
            }
        })
    };

    save_button.connect_clicked({
        let save = save.clone();
        move |_| save()
    });

    // Split: rewrite the selected commit (or working-copy entry) to the edited
    // diff and insert a new commit holding its original tree right after it.
    // Mirrors the save closure's commit-content path (and its place-restoring
    // reload), but is diff-only — message/identity edits are left for Save. The
    // button is insensitive unless the diff has pending edits; guard conflict mode.
    split_button.connect_clicked({
        let repo = repo.clone();
        let commits = commits.clone();
        let changes = changes.clone();
        let orig_changes = orig_changes.clone();
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
        let selected_wc_change = selected_wc_change.clone();
        let refresh_wc = refresh_wc.clone();
        let wc_list = wc_list.clone();
        let wc_entries = wc_entries.clone();
        move |_| {
            if pane_mode.borrow().is_conflict() {
                return;
            }
            let edits = match collect_file_edits(&buffer_text(&file_buffer), &orig_changes.borrow()) {
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
            // Restore the diff place after a reload; shared by both paths.
            let restore_place = {
                let changes = changes.clone();
                let file_buffer = file_buffer.clone();
                let file_view = file_view.clone();
                let file_dropdown = file_dropdown.clone();
                move || {
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
            };

            // Splitting a working-copy entry: a pure jj-side peel — no history
            // change, so only the working-copy rows and this diff reload. The
            // edited entry keeps its change id; re-select it so the highlight and
            // the reloaded diff stay in sync.
            if viewing_wc.get() {
                let change = selected_wc_change.borrow().clone();
                if let Err(err) = repo.borrow_mut().split_working_copy(change.as_deref(), &edits) {
                    show_status(&format!("Split failed: {err}"));
                    return;
                }
                refresh_wc();
                wc_list.unselect_all();
                if let Some(ch) = &change {
                    let idx = wc_entries
                        .borrow()
                        .iter()
                        .position(|e| e.info.change_id_hex() == *ch);
                    if let Some(row) = idx.and_then(|i| wc_list.row_at_index(i as i32)) {
                        wc_list.select_row(Some(&row)); // fires row-selected -> reload
                    }
                }
                restore_place();
                return;
            }

            // Splitting a history commit.
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
            restore_place();
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
/// content differs from the commit's current version. Files rendered read-only
/// are skipped: removed/binary ones (no `new_text`) and a conflicted merge base
/// (its notice has no hunks, so applying it would reconstruct empty content and
/// spuriously "edit" the file to nothing). The original trailing-newline style is
/// preserved. `Err` carries an apply-failure message (the patch firewall should
/// make that unreachable, but a save surfaces it rather than dropping silently).
fn collect_file_edits(
    combined: &str,
    changes: &[FileChange],
) -> Result<Vec<(String, String)>, String> {
    let mut edits = Vec::new();
    for (path, patch) in split_combined_patch(combined) {
        let Some(change) = changes.iter().find(|c| c.path == path) else {
            continue;
        };
        if change.conflicted_base {
            continue;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use commedit_engine::diff::ChangeKind;
    use std::collections::HashMap;

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

    fn added(path: &str, new: &str) -> FileChange {
        FileChange {
            path: path.to_string(),
            kind: ChangeKind::Added,
            old_text: None,
            new_text: Some(new.to_string()),
            is_binary: false,
            conflicted_base: false,
        }
    }

    /// The `@@` header / `diff --git` line of the first file in a built diff.
    fn hunk_line(text: &str) -> (usize, String) {
        text.split('\n')
            .enumerate()
            .find(|(_, l)| l.starts_with("@@"))
            .map(|(i, l)| (i, l.to_string()))
            .unwrap()
    }

    #[test]
    fn pills_on_line_finds_both_hunk_pills_with_ordered_ranges() {
        let line = format!(
            "@@ -1,3 +1,3 @@  {}  {}",
            pill("↕ expand context"),
            pill(REVERT_HUNK_LABEL)
        );
        let pills = pills_on_line(&line);
        assert_eq!(pills.len(), 2);
        assert_eq!(pills[0].2, "↕ expand context");
        assert_eq!(pills[1].2, REVERT_HUNK_LABEL);
        assert!(pills[0].1 < pills[1].0, "pill ranges are disjoint & ordered");
        let chars: Vec<char> = line.chars().collect();
        for &(lc, rc, _) in &pills {
            assert_eq!(chars[lc], CUE_CAP_L);
            assert_eq!(chars[rc], CUE_CAP_R);
        }
    }

    #[test]
    fn diff_cue_at_disambiguates_expand_and_revert_on_one_header() {
        // A 12-line file with one mid edit leaves hidden context both ways, so the
        // header carries both an expand and a revert pill.
        let old: String = (1..=12).map(|n| format!("l{n}\n")).collect();
        let new = old.replace("l6\n", "L6\n");
        let (text, hunks, _files) =
            build_diff_buffer_text(&[modified("f", &old, &new)], &HashMap::new());
        let (li, line) = hunk_line(&text);
        let pills = pills_on_line(&line);
        assert_eq!(pills.len(), 2, "expand + revert");
        assert_eq!(diff_cue_at(&hunks, &line, li, 0), None, "before any pill");
        assert!(matches!(
            diff_cue_at(&hunks, &line, li, pills[0].0),
            Some(DiffCue::Expand(_, _))
        ));
        assert!(matches!(
            diff_cue_at(&hunks, &line, li, pills[1].0),
            Some(DiffCue::RevertHunk(_, _))
        ));
        // A click on the revert pill's right cap still counts (inclusive).
        assert!(matches!(
            diff_cue_at(&hunks, &line, li, pills[1].1),
            Some(DiffCue::RevertHunk(_, _))
        ));
    }

    #[test]
    fn revert_hunk_pill_present_even_without_an_expand_pill() {
        // A 3-line file with one change has no hidden context: no expand pill, but
        // still a revert pill, and the hit-test resolves it.
        let (text, hunks, _files) =
            build_diff_buffer_text(&[modified("f", "a\nb\nc\n", "a\nB\nc\n")], &HashMap::new());
        let (li, line) = hunk_line(&text);
        let pills = pills_on_line(&line);
        assert_eq!(pills.len(), 1);
        assert_eq!(pills[0].2, REVERT_HUNK_LABEL);
        assert!(matches!(
            diff_cue_at(&hunks, &line, li, pills[0].0),
            Some(DiffCue::RevertHunk(_, _))
        ));
    }

    #[test]
    fn revert_file_cue_rides_the_diff_git_line() {
        let (text, _hunks, files) =
            build_diff_buffer_text(&[modified("f", "a\nb\n", "a\nB\n")], &HashMap::new());
        let lines: Vec<&str> = text.split('\n').collect();
        let sep = lines[files[0].start_line];
        assert!(sep.starts_with("diff --git "));
        let pills = pills_on_line(sep);
        assert_eq!(pills.len(), 1);
        assert_eq!(pills[0].2, REVERT_FILE_LABEL);
        assert_eq!(
            diff_cue_at(&[], sep, files[0].start_line, pills[0].0),
            Some(DiffCue::RevertFile)
        );
    }

    #[test]
    fn added_files_get_no_revert_cue() {
        // No old side means a content-only edit can't drop the change, so neither
        // revert cue is offered for an added file.
        let (text, _h, _f) = build_diff_buffer_text(&[added("new.txt", "x\ny\n")], &HashMap::new());
        assert!(!text.contains(REVERT_HUNK_LABEL));
        assert!(!text.contains(REVERT_FILE_LABEL));
    }
}
