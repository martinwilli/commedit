//! commedit GTK4 UI (Milestone 2): browse history, edit a commit message, and
//! edit the content of files a commit changes. Saving transparently rewrites the
//! commit and rebases descendants via the engine.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::diff::{
    apply_patch, combined_changes, commit_changes, reconstruct_conflict_file, render_commit_diff,
    render_conflict_snippets, revert_groups, split_combined_patch, CombinedFile, ContextExpansion,
    FileChange, HunkInfo,
};
use commedit_engine::graph::{compute_graph, GraphLayout};
use commedit_engine::history::{history, history_limited, CommitInfo};
use commedit_engine::patch_edit::{
    collapse_diff, deletion_is_safe, plan_edit, strip_selection_prefixes, Cursor, EditGesture,
    EditPlan, Selection,
};
use commedit_engine::repo::Repo;
use commedit_engine::rewrite::{BatchEdit, Identity};
use commedit_engine::tabwidth::{TabWidthResolver, DEFAULT_TAB_WIDTH};
use commedit_engine::tree::FileEdit;
use commedit_engine::workcopy::WorkingCopyEntry;
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    gdk, Application, ApplicationWindow, Box as GtkBox, Button, CallbackAction, DropDown, Entry,
    EventControllerKey, EventControllerScroll, EventControllerScrollFlags, Grid, HeaderBar, Label,
    ListBox, ListBoxRow, Orientation, Paned, PolicyType, Popover, PropagationPhase, ScrolledWindow,
    SearchEntry, Shortcut, ShortcutController, ShortcutTrigger, Stack, StringList, ToggleButton,
};
use sourceview5::prelude::ViewExt;
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
mod diff_cues;
mod dnd;
mod dragdrop;
mod linenums;
mod msglint;
mod search;
mod spelling;
mod window_state;

/// The conflict pane's late-bound "expand hidden lines" action, invoked by buffer
/// line; `None` until the conflict renderer (defined below it) binds it.
type ConflictExpand = Rc<RefCell<Option<Rc<dyn Fn(usize)>>>>;

/// Pushes freshly rendered diff text (with its derived hunk/file state) into the
/// buffer; the `bool` picks a full `set_text` (fresh load) over an in-place `splice`.
type ApplyDiffText = Rc<dyn Fn(String, Vec<HunkInfo>, Vec<CombinedFile>, bool)>;

/// Sets the diff view's tab width for the file path now at the top of the view.
type ApplyTabWidth = Rc<dyn Fn(Option<&str>)>;

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
    // `commedit [PATH] [BRANCH]`: an optional repo path and an optional branch to
    // edit (which need not be checked out). See `commedit_engine::cli`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (repo_path, branch) = commedit_engine::cli::parse_repo_and_branch(&args);

    // Each invocation targets its own repo path, so GTK's single-instance
    // activation (which would forward a second launch to the primary process
    // and its repo) must be disabled.
    let app = Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| build_ui(app, repo_path.clone(), branch.clone()));
    app.run_with_args::<&str>(&[]);
}

/// Build the diff pane's full buffer text — the combined unified diff for
/// `changes` — together with the hunks (for context expansion) and the per-file
/// placement. The expand / revert affordances are gutter buttons now (see
/// `diff_cues`), so the buffer text is the clean diff itself. Shared by the full
/// (`set_text`) render and the in-place spliced re-render.
fn build_diff_buffer_text(
    changes: &[FileChange],
    expansions: &HashMap<String, ContextExpansion>,
) -> (String, Vec<HunkInfo>, Vec<CombinedFile>) {
    let combined = render_commit_diff(changes, expansions);
    let all_hunks: Vec<HunkInfo> = combined
        .files
        .iter()
        .flat_map(|f| f.hunks.iter().cloned())
        .collect();
    (combined.text, all_hunks, combined.files)
}

/// Show `text` as a dim, italic note filling `buffer` (replacing its contents) —
/// the standalone notice the multi-commit view uses for "message not editable" and
/// "combined diff not representable". Creates the `note-italic` tag on first use, so
/// it works on the plain message buffer and the diff buffer alike. The caller must
/// guard a firewalled buffer (the diff buffer) with its `editing` flag, since
/// `set_text` fires the insert/delete signals the firewall watches.
fn set_note(buffer: &sourceview5::Buffer, text: &str) {
    let table = buffer.tag_table();
    let tag = table.lookup("note-italic").unwrap_or_else(|| {
        let tag = gtk::TextTag::builder()
            .name("note-italic")
            .style(gtk::pango::Style::Italic)
            .foreground("#6e7781")
            .build();
        table.add(&tag);
        tag
    });
    buffer.set_text(text);
    let (start, end) = buffer.bounds();
    buffer.apply_tag(&tag, &start, &end);
}

/// The subset of the render baseline `changes` that the diff view actually shows.
/// A file whose whole change was reverted away — no net change left (`old == new`)
/// *and* that's a divergence from the pristine `orig` (so it was reverted, not a
/// mode-only change that started with no textual diff) — is dropped, so a revert
/// leaves no empty placeholder behind in the buffer or the file dropdown. The full
/// baseline still drives the save: `collect_file_edits` reads the dropped file's
/// reverted `new_text` to emit the delete/restore.
fn visible_changes(changes: &[FileChange], orig: &[FileChange]) -> Vec<FileChange> {
    changes
        .iter()
        .filter(|c| {
            let no_net_change = c.old_text == c.new_text;
            let diverged = orig
                .iter()
                .find(|o| o.path == c.path)
                .is_none_or(|o| o.new_text != c.new_text);
            !(no_net_change && diverged)
        })
        .cloned()
        .collect()
}

fn build_ui(app: &Application, repo_path: PathBuf, branch: Option<String>) {
    // Use the index cache so repeated launches against the same repo skip
    // rebuilding jj's commit index from scratch (see `commedit_engine::index_cache`).
    // `branch`, when given, edits a branch that need not be checked out (an
    // off-worktree session: only that ref moves, no working copy).
    let repo = match Repo::open_branch(
        &repo_path,
        commedit_engine::index_cache::IndexCache::Default,
        branch.as_deref(),
    ) {
        Ok(repo) => Rc::new(RefCell::new(repo)),
        Err(err) => {
            present_error(
                app,
                "Unable to open repository",
                &format!("{}\n\n{err:#}", repo_path.display()),
            );
            return;
        }
    };

    // Off-worktree (editing a branch that isn't checked out): there is no working
    // copy, so the trash "restore to working tree" button can't work — it is
    // omitted, and the engine refuses working-copy operations.
    let worktree_bound = repo.borrow().is_worktree_bound();

    // Resolves the display tab width per file from the repo's editor-config files
    // (`.editorconfig` / `.vscode/settings.json` / `.clang-format`), applied to the
    // diff view as the user navigates between files. Built once: the config files
    // are fixed for the session and resolution is cached per path.
    let tab_resolver = Rc::new(TabWidthResolver::new(repo.borrow().workspace_root()));

    // Shared UI state.
    let commits: Rc<RefCell<Vec<CommitInfo>>> = Rc::new(RefCell::new(Vec::new()));
    // The ancestry-graph lane layout drawn beside the history rows, recomputed
    // whenever `commits` is reloaded (each row's drawing area reads its slice).
    let graph: Rc<RefCell<GraphLayout>> = Rc::new(RefCell::new(GraphLayout::default()));
    // How many history rows the normal (non-conflict) view currently loads, and
    // whether older commits remain below them. `refresh` reads the limit and sets
    // the flag; scrolling near the bottom bumps the limit by `HISTORY_PAGE`.
    let history_limit: Rc<Cell<usize>> = Rc::new(Cell::new(HISTORY_PAGE));
    let history_has_more: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // The *anchor* of the selection — the single commit that the single-commit
    // operations (conflict resolution, drag, revert/merge-out) target. `Some` when
    // at least one commit is selected. Lives in the shared `Data` bundle.
    let selected_change: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    // The *full* multi-selection as change ids, newest-first (display order). The
    // anchor is its first entry. With more than one entry the right pane is the
    // read-only multi-commit view (combined diff, common-or-differing identity, no
    // message editing). Main-local: only the pane router, refresh and save need it.
    let selected_changes: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    // Guards `update_selection_pane` against re-entrancy while we drive the list
    // selection programmatically (refresh re-selecting rows, the working-copy
    // handler clearing the history selection), so it runs once afterwards rather
    // than per `selected-rows-changed` emission. Mirrors `nav_sync`.
    let selection_sync: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // Header search state. `search_query` is the live text; `search_matches` the
    // row indices whose subject matches it (ascending, recomputed on each
    // change and after every refresh); `search_cursor` the index *into*
    // `search_matches` that Enter last selected (`None` = not yet stepped, so the
    // next Enter picks the first match). See the search wiring near the shortcuts.
    let search_query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let search_matches: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
    let search_cursor: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
    // The row a Shift-click extends the selection from — set on each plain/Ctrl
    // click (see the history list's click handler).
    let selection_anchor: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
    // Set while the diff pane shows a read-only view (the multi-commit combined
    // diff): the renderer then suppresses the revert cues and forces the view
    // non-editable. Cleared for a single commit / working-copy entry.
    let diff_read_only: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // The value each identity field was populated with when the multi-commit view
    // was entered (the shared value, or "" for a field that differs across the
    // selection); the multi-save compares the live entries against it to tell which
    // fields the user actually changed. Order matches `read_identity`.
    let multi_identity_baseline: Rc<RefCell<[String; 4]>> =
        Rc::new(RefCell::new(Default::default()));
    // The git-default identity prefilled into the fields when a working-copy entry
    // is selected (see the `wc_list` row handler); the working-copy commit save
    // compares the live fields against it to tell whether the user overrode the
    // author/committer — an unchanged set commits as `None`, letting the engine
    // stamp git config + a fresh "now". Order matches `read_identity`.
    let wc_identity_baseline: Rc<RefCell<[String; 4]>> = Rc::new(RefCell::new(Default::default()));
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
    // The selected display indices (newest-first) of a multi-selection being
    // dragged as a group, captured at drag start; empty for a single-commit drag.
    let drag_set: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
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
    let post_drag: PostDrag = Rc::new(RefCell::new(None));
    // Whether the diff pane is showing a normal diff or a conflict to resolve.
    let pane_mode: Rc<RefCell<PaneMode>> = Rc::new(RefCell::new(PaneMode::Diff));
    // Per-file state of the combined conflict-snippet buffer for the selected
    // commit (rebuilt by `load_conflict_files`, in dropdown/file order).
    let conflict_view: Rc<RefCell<Vec<ConflictFileView>>> = Rc::new(RefCell::new(Vec::new()));
    // Whether a working-copy (@) row is the current selection, in which case the
    // diff is editable and the message/identity craft a commit: Save with no
    // message writes the diff edits back to the working tree, Save with one
    // commits the changes on HEAD (see the `save` closure).
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
             border: 1px solid rgb(224, 27, 36); border-radius: 5px; } \
             row.op-affected { background-color: rgba(53, 132, 228, 0.22); \
             border: 1px dashed rgb(53, 132, 228); border-radius: 5px; } \
             .commit-id-copy { background-color: @theme_base_color; \
             color: @theme_fg_color; border-radius: 4px; padding: 0 1px; } \
             .commit-revert { background-color: @theme_base_color; \
             color: @theme_fg_color; border-radius: 4px; padding: 0 1px; } \
             .ref-pill { font-size: smaller; border-radius: 9px; padding: 0 7px; } \
             .ref-branch { background-color: rgba(46, 194, 126, 0.25); \
             border: 1px solid rgba(46, 194, 126, 0.8); } \
             .ref-tag { background-color: rgba(245, 194, 17, 0.25); \
             border: 1px solid rgba(245, 194, 17, 0.8); } \
             .ref-current { background-color: rgba(53, 132, 228, 0.30); \
             border: 1px solid rgba(53, 132, 228, 0.9); font-weight: bold; } \
             entry.identity-differs text { font-style: italic; } \
             entry.identity-differs > text > placeholder { font-style: italic; } \
             .history-list row { padding-top: 0; padding-bottom: 0; } \
             row.squash-blame { background-color: rgba(145, 65, 172, 0.20); \
             border: 1px dashed rgb(145, 65, 172); border-radius: 5px; }",
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
        "Uncommitted working-tree changes — edit the diff here, then Save (no commit \
         message writes back to the working tree, a message commits on HEAD), Split to \
         peel off a piece, or drag a row onto a commit to fold it in",
    ));

    let list = ListBox::new();
    // Allow ctrl/shift-click multi-selection: editing several commits' identity at
    // once, or viewing their combined diff (see `update_selection_pane`).
    list.set_selection_mode(gtk::SelectionMode::Multiple);
    // Strip the theme's vertical row padding: the ancestry-graph lines must run
    // edge-to-edge to connect across rows (the rows' content boxes keep their
    // own margins for breathing space).
    list.add_css_class("history-list");
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
    // Interactive spell checking (red squiggles + right-click suggestions) on
    // the message editor, via libspelling — it adapts to the GtkSourceBuffer.
    spelling::attach(&message_view, &message_buffer);

    // Identity fields above the message editor: one combined "Name <email>"
    // field per role (with a built-in ▼ to pick an identity used elsewhere) and
    // a date field with a calendar button to its right.
    let author_id = identity_entry(IDENTITY_PLACEHOLDERS[0]);
    let author_date = identity_entry(IDENTITY_PLACEHOLDERS[1]);
    let committer_id = identity_entry(IDENTITY_PLACEHOLDERS[2]);
    let committer_date = identity_entry(IDENTITY_PLACEHOLDERS[3]);
    // The date fields need only fit a formatted "YYYY-MM-DD HH:MM:SS ±HHMM"; pin
    // them to that width and stop them expanding so the grid gives the slack to
    // the identity column instead.
    for date in [&author_date, &committer_date] {
        date.set_hexpand(false);
        date.set_width_chars(26);
        date.set_max_width_chars(26);
    }
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
    // Draw whitespace so indentation (tab vs. space) and trailing whitespace are
    // visible. `ALL` locations is what surfaces leading indentation; the matrix
    // must be enabled for the per-location/type selection to apply. Newlines are
    // deliberately left undrawn (a glyph on every line is noisy). A context
    // line's space prefix shows a faint leading dot — an accepted minor cosmetic.
    let space_drawer = file_view.space_drawer();
    space_drawer.set_types_for_locations(
        sourceview5::SpaceLocationFlags::ALL,
        sourceview5::SpaceTypeFlags::SPACE
            | sourceview5::SpaceTypeFlags::TAB
            | sourceview5::SpaceTypeFlags::NBSP,
    );
    space_drawer.set_enable_matrix(true);
    // The file gutter: two columns, old | new. Each draws *either* a line number
    // *or* a clickable cue button per line (the two never coincide — a `@@` header,
    // a `diff --git` separator or a conflict marker carries no number), so the
    // action buttons sit at the same level as the numbers rather than in extra
    // columns (`diff_cues::GutterColumn`). A context line shows both numbers, a
    // `-` line only old, a `+` line only new. In the diff: expand rides col_old,
    // revert col_new; in the conflict view: ours/theirs numbers, with the resolve
    // "keep" button on col_new. Both columns' content is recomputed from the buffer
    // text on every change below; their click handlers are bound once the render
    // state exists.
    let line_gutter = sourceview5::prelude::ViewExt::gutter(&file_view, gtk::TextWindowType::Left);
    let col_old = diff_cues::GutterColumn::new(linenums::NumColumn::Old);
    let col_new = diff_cues::GutterColumn::new(linenums::NumColumn::New);
    line_gutter.insert(&col_old, 0);
    line_gutter.insert(&col_new, 1);
    file_buffer.connect_changed({
        let pane_mode = pane_mode.clone();
        let col_old = col_old.clone();
        let col_new = col_new.clone();
        let combined_files = combined_files.clone();
        let conflict_view = conflict_view.clone();
        let changes = changes.clone();
        let diff_read_only = diff_read_only.clone();
        move |buffer| {
            let text = buffer_text(buffer);
            // Conflict snippets (`<<<`/`>>>`) aren't a unified diff: the columns show
            // each side's line numbers (ours | theirs), with the elision "expand"
            // button on col_old and the resolve "keep" button per marker line on
            // col_new. Leaving conflict mode re-sets the buffer text, firing this
            // handler again to restore the diff numbers and cues.
            if pane_mode.borrow().is_conflict() {
                let nums = linenums::conflict_line_numbers(&conflict_view.borrow());
                let (elision, resolve) = conflict_cue_cells(&text);
                col_old.set_content(&nums, &elision);
                col_new.set_content(&nums, &resolve);
            } else {
                let nums = linenums::diff_line_numbers(&text);
                let (exp, rev) = diff_cues::diff_cue_cells(
                    &text,
                    &combined_files.borrow(),
                    &changes.borrow(),
                    diff_read_only.get(),
                );
                col_old.set_content(&nums, &exp);
                col_new.set_content(&nums, &rev);
            }
        }
    });
    // Set while we mutate the diff buffer ourselves (loading a file, or applying
    // a structured edit) so the firewall signal handlers below let it through
    // instead of treating it as an interactive edit.
    let editing = Rc::new(Cell::new(false));
    let file_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&file_view)
        .build();
    // The horizontal Paned's ~9px resize handle overlaps the left edge of this
    // pane, and its drag gesture (capture phase, on the Paned ancestor) claims
    // presses over that band before they reach the gutter. Flush against the
    // handle, the leftmost gutter column's buttons lost their left half to it.
    // Inset the editor clear of the handle band (the analogue of the dropdown's
    // top margin above).
    file_scroll.set_margin_start(12);
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
    // Lit only when there is something to save — a pending diff, message or
    // identity/date edit vs. the loaded commit (wired by `update_save_sensitivity`);
    // always lit in conflict mode, where Save resolves the current conflict.
    save_button.set_sensitive(false);
    // Sits left of Save. Splits the selected commit into the edited diff plus a
    // follow-up "fixup! …" commit; enabled only while the diff has pending edits
    // (wired by `update_split_sensitivity`), never in the conflict/working-copy views.
    let split_button = Button::with_label("Split");
    split_button.set_tooltip_text(Some(SPLIT_HINT));
    split_button.set_sensitive(false);
    // Conflict-mode quick resolution is driven from the gutter: clicking a marker
    // line's "keep" button keeps that side — see `conflict_cue_cells` and the
    // resolve renderer above. No toolbar buttons.
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

    // Window geometry remembered from the previous session (size + maximized
    // state + the two divider positions), falling back to the built-in defaults
    // on a fresh install. Saved on close-request below.
    let win_state = window_state::WindowState::load();

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
        .position(win_state.message_height)
        .shrink_end_child(false)
        .build();

    let paned = Paned::builder()
        .orientation(Orientation::Horizontal)
        .start_child(&history_box)
        .end_child(&right_paned)
        .position(win_state.list_width)
        .build();

    // --- Compare view (full-window, read-only session diff) ---
    // A second diff surface shown in place of the whole editor while the "Compare"
    // toggle is on: the content delta between the current tree and the one the
    // session started with (see `Repo::session_changes`). Its own buffer so the
    // editable diff pane is left untouched; rendered on demand by `render_compare`
    // below. Read-only — none of the diff pane's edit wiring applies here.
    let compare_buffer = sourceview5::Buffer::new(None);
    install_diff_tags(&compare_buffer);
    let compare_view = sourceview5::View::with_buffer(&compare_buffer);
    compare_view.set_monospace(true);
    compare_view.set_editable(false);
    compare_view.set_left_margin(8);
    compare_view.set_top_margin(8);
    let compare_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&compare_view)
        .build();

    // The editor and the compare view are mutually exclusive full-window pages;
    // the "Compare" header toggle (wired below) flips between them.
    paned.set_vexpand(true);
    let content_stack = Stack::new();
    content_stack.add_named(&paned, Some("edit"));
    content_stack.add_named(&compare_scroll, Some("compare"));

    let root = GtkBox::new(Orientation::Vertical, 0);
    root.append(&content_stack);

    // The header bar keeps the window title and the window controls; the Save
    // action lives in the bottom action bar. The custom controls are the
    // top-right "Revert all" button (rolls the whole session back to the state
    // the repo was opened in) and a "Compare" toggle that shows a read-only,
    // full-window diff of every content change made this session. Both are wired
    // below, once `refresh` & co. exist.
    let header = HeaderBar::new();
    // The "Edit history" button: a clock-with-arrow icon opening a dropdown of
    // this session's snapshots (every history edit, plus the session-start state)
    // to travel back — and forward — to. It replaces the old "Revert all" button:
    // jumping to the bottom "Session start" entry is exactly that revert.
    let history_button = Button::from_icon_name("document-open-recent-symbolic");
    history_button.set_tooltip_text(Some(
        "Edit history — travel to an earlier state from this session",
    ));
    let compare_button = ToggleButton::with_label("Compare");
    compare_button.set_tooltip_text(Some(
        "Compare all content changes made this session (current tree vs. the session start)",
    ));
    // The top-left "Reload" button: re-open the repository from disk to pick up
    // changes made outside commedit — a fresh session in place, same as
    // restarting the app.
    let reload_button = Button::from_icon_name("view-refresh-symbolic");
    reload_button.set_tooltip_text(Some(
        "Reload the repository — pick up changes made outside commedit",
    ));
    // A search box just right of Reload: typing matches commit subjects in the
    // list below by substring term (highlighting the matched characters and
    // scrolling to the first hit); Enter selects matches in turn. Focusable with
    // Ctrl+F. Wired below, once `refresh` & the selection router exist.
    let search_entry = SearchEntry::new();
    search_entry.set_placeholder_text(Some("Search commits"));
    search_entry.set_width_request(220);
    search_entry.set_tooltip_text(Some(
        "Search commit subjects (Ctrl+F) — Enter jumps to the next match",
    ));
    header.pack_start(&reload_button);
    header.pack_start(&search_entry);
    // pack_end fills right-to-left, so packing the history button first leaves
    // "Compare" to its left: [ Compare ][ ↺ ].
    header.pack_end(&history_button);
    header.pack_end(&compare_button);

    // Title with the repository folder name, e.g. "Commit editor - commedit".
    let folder = repo
        .borrow()
        .workspace
        .workspace_root()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "commedit".to_string());
    // Off-worktree, name the edited branch in the title so multiple windows are
    // distinguishable.
    let title = if worktree_bound {
        format!("Commit editor - {folder}")
    } else {
        let branch = repo
            .borrow()
            .target_branch_name()
            .unwrap_or("?")
            .to_string();
        format!("Commit editor - {folder} [{branch}]")
    };
    let window = ApplicationWindow::builder()
        .application(app)
        .title(title)
        .default_width(win_state.width)
        .default_height(win_state.height)
        .child(&root)
        .build();
    window.set_titlebar(Some(&header));
    if win_state.maximized {
        window.maximize();
    }

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
        graph: graph.clone(),
        trashed: trashed.clone(),
        pending_trash_op: pending_trash_op.clone(),
        wc_entries: wc_entries.clone(),
        selected_change: selected_change.clone(),
        selected_changes: selected_changes.clone(),
        pane_mode: pane_mode.clone(),
        conflict_view: conflict_view.clone(),
    };
    let drag_state = DragState {
        drag_origin: drag_origin.clone(),
        drag_row: drag_row.clone(),
        drag_from: drag_from.clone(),
        drag_set: drag_set.clone(),
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

    // Instant, line-local re-highlight of one buffer line — the responsive path
    // for an in-place keystroke the firewall lets through (`EditPlan::Allow`),
    // which otherwise only repaints via the 60ms debounced full pass and so
    // trails the typing. Diff pane only; the debounced pass still fixes anything
    // needing cross-line state.
    let highlight_line: Rc<dyn Fn(i32)> = {
        let file_buffer = file_buffer.clone();
        let current_file = current_file.clone();
        let syntax_set = syntax_set.clone();
        let theme = theme.clone();
        let pane_mode = pane_mode.clone();
        Rc::new(move |li: i32| {
            if pane_mode.borrow().is_conflict() {
                return;
            }
            let path = current_file.borrow().clone();
            highlight_diff_line(&file_buffer, li, path.as_deref(), &syntax_set, &theme);
        })
    };

    // The conflict pane's "expand hidden lines" action, late-bound (it needs the
    // conflict renderer defined below). The expand-click gesture invokes it by
    // buffer line, mirroring the diff renderer.
    let conflict_expand_cell: ConflictExpand = Rc::new(RefCell::new(None));
    // Push `new_text` (built by `build_diff_buffer_text`) into the buffer and
    // refresh the derived state + highlighting. `splice` chooses how the text
    // lands: a full `set_text` (used for a fresh load, where resetting the scroll
    // to the top is wanted) or an in-place `splice` (used when widening context,
    // where the scroll must stay put). The expand / revert cues are persistent
    // gutter renderers (`diff_cues`), so no per-line widgets are added or removed.
    let apply_diff_text: ApplyDiffText = {
        let combined_files = combined_files.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let editing = editing.clone();
        let highlight = highlight.clone();
        let diff_read_only = diff_read_only.clone();
        Rc::new(
            move |text: String, _all_hunks, files: Vec<CombinedFile>, splice: bool| {
                editing.set(true);
                // Update the placement *before* the text lands: `set_text`/`splice`
                // fires `connect_changed`, which rebuilds the gutter cue cells from
                // `combined_files`, so it must already match the text about to show.
                file_view.set_editable(!diff_read_only.get() && files.iter().any(|f| f.editable));
                *combined_files.borrow_mut() = files;
                if splice {
                    splice_buffer_text(&file_buffer, &text);
                } else {
                    // A fresh render is a new editing context, not an undoable edit:
                    // mark it irreversible so it clears the undo history rather than
                    // letting a later Ctrl+Z revert the load itself.
                    file_buffer.begin_irreversible_action();
                    file_buffer.set_text(&text);
                    file_buffer.end_irreversible_action();
                }
                // Highlight in this same main-loop turn, before GTK paints, so the
                // diff appears once fully colored instead of flashing plain first and
                // then re-highlighting via the debounced `changed` handler (which is
                // suppressed while `editing` is set).
                highlight();
                editing.set(false);
            },
        )
    };
    // Full render: every file's diff in one buffer, files separated by
    // `diff --git` lines. `set_text`s the buffer (scroll resets to the top).
    let render_diff_view: Renderer = {
        let changes = changes.clone();
        let orig_changes = orig_changes.clone();
        let expansions = expansions.clone();
        let apply_diff_text = apply_diff_text.clone();
        Rc::new(move || {
            let vis = visible_changes(&changes.borrow(), &orig_changes.borrow());
            let (text, hunks, files) = build_diff_buffer_text(&vis, &expansions.borrow());
            apply_diff_text(text, hunks, files, false);
        })
    };
    // In-place re-render after widening a hunk's context: splices only the
    // changed span so GTK keeps the scroll exactly where it is. Expansion only
    // adds/changes lines at or below the clicked header — everything above is a
    // common prefix — so the header stays put and the new context grows below it.
    let rerender_diff_spliced: Renderer = {
        let changes = changes.clone();
        let orig_changes = orig_changes.clone();
        let expansions = expansions.clone();
        let apply_diff_text = apply_diff_text.clone();
        Rc::new(move || {
            let vis = visible_changes(&changes.borrow(), &orig_changes.borrow());
            let (text, hunks, files) = build_diff_buffer_text(&vis, &expansions.borrow());
            apply_diff_text(text, hunks, files, true);
        })
    };

    // Rebuild the file dropdown from the *visible* changes (a revert can drop a
    // file). Built here so both the initial load and a revert reuse it; callers
    // guard with `nav_sync` when a stale selection would otherwise scroll.
    let rebuild_file_dropdown: Rc<dyn Fn()> = {
        let changes = changes.clone();
        let orig_changes = orig_changes.clone();
        let file_dropdown = file_dropdown.clone();
        Rc::new(move || {
            let vis = visible_changes(&changes.borrow(), &orig_changes.borrow());
            let labels: Vec<String> = vis.iter().map(change_label).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            file_dropdown.set_model(Some(&StringList::new(&refs)));
        })
    };

    // Apply a diff cue (expand context, revert hunk, revert file) for `path` and
    // re-render in place. The spliced re-render edits only the changed span, so GTK
    // keeps the scroll where it sat. A revert mutates the *render baseline*
    // (`changes`) so the dropped change survives later re-renders; Save/Split then
    // see it as a divergence from the pristine `orig_changes`. Guarded with
    // `nav_sync` so the settle doesn't flip the file dropdown. Shared by the two
    // gutter cue columns (`diff_cues`).
    let apply_diff_cue: Rc<dyn Fn(DiffCue, String)> = {
        let expansions = expansions.clone();
        let changes = changes.clone();
        let orig_changes = orig_changes.clone();
        let rerender_diff_spliced = rerender_diff_spliced.clone();
        let rebuild_file_dropdown = rebuild_file_dropdown.clone();
        let file_dropdown = file_dropdown.clone();
        let file_view = file_view.clone();
        let file_buffer = file_buffer.clone();
        let nav_sync = nav_sync.clone();
        Rc::new(move |cue: DiffCue, path: String| {
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
            // A revert that drops a file's whole change removes it from the view
            // (`visible_changes`); rebuild the dropdown to match and re-point it at
            // the file now at the viewport top. Only when the visible set shrank —
            // an expand or partial hunk-revert leaves every file in place.
            let vis_len = visible_changes(&changes.borrow(), &orig_changes.borrow()).len();
            if file_dropdown.model().map_or(0, |m| m.n_items()) as usize != vis_len {
                rebuild_file_dropdown();
                let top = file_view
                    .vadjustment()
                    .map(|v| {
                        let (iter, _) = file_view.line_at_y(v.value() as i32);
                        diff_file_index_at_line(&file_buffer, iter.line() as usize)
                    })
                    .unwrap_or(0);
                file_dropdown.set_selected(top as u32);
            }
            nav_sync.set(false);
        })
    };
    // Bind the two gutter columns' click handlers. A column means different things
    // per pane mode, so each dispatches on it. col_old: diff → widen this hunk's
    // context. col_new: diff → drop this hunk's or file's change; conflict → resolve
    // this block to the side its marker names. Each maps the clicked line back to
    // its hunk/file/block from the live buffer, then defers the mutation to an idle
    // so it runs outside the gutter's click handling.
    col_old.set_on_activate({
        let pane_mode = pane_mode.clone();
        let combined_files = combined_files.clone();
        let file_buffer = file_buffer.clone();
        let apply_diff_cue = apply_diff_cue.clone();
        let conflict_expand_cell = conflict_expand_cell.clone();
        Rc::new(move |line: u32| {
            if pane_mode.borrow().is_conflict() {
                // Conflict view: the `↕` button on an elision placeholder reveals
                // that hidden run (`conflict_expand`, late-bound below).
                if let Some(expand) = conflict_expand_cell.borrow().clone() {
                    glib::idle_add_local_once(move || expand(line as usize));
                }
                return;
            }
            let text = buffer_text(&file_buffer);
            let Some((first, last, path)) =
                diff_cues::hunk_target(&text, &combined_files.borrow(), line as usize)
            else {
                return;
            };
            let apply = apply_diff_cue.clone();
            glib::idle_add_local_once(move || apply(DiffCue::Expand(first, last), path));
        })
    });
    col_new.set_on_activate({
        let pane_mode = pane_mode.clone();
        let combined_files = combined_files.clone();
        let file_buffer = file_buffer.clone();
        let apply_diff_cue = apply_diff_cue.clone();
        let editing = editing.clone();
        let highlight = highlight.clone();
        Rc::new(move |line: u32| {
            // Conflict view: a click on a marker line's "keep" button resolves that
            // block to the side the marker names. `resolve_conflict_at` is reused
            // unchanged.
            if pane_mode.borrow().is_conflict() {
                let text = buffer_text(&file_buffer);
                let Some(side) = conflict_side_at_line(&text, line as usize) else {
                    return;
                };
                let file_buffer = file_buffer.clone();
                let editing = editing.clone();
                let highlight = highlight.clone();
                glib::idle_add_local_once(move || {
                    resolve_conflict_at(&file_buffer, &editing, line as usize, side, &*highlight);
                });
                return;
            }
            // Diff view: revert this hunk, or — on a `diff --git` line — the file.
            let text = buffer_text(&file_buffer);
            let files = combined_files.borrow();
            let line = line as usize;
            let resolved = if let Some((first, last, path)) =
                diff_cues::hunk_target(&text, &files, line)
            {
                Some((DiffCue::RevertHunk(first, last), path))
            } else {
                diff_cues::file_target(&text, &files, line).map(|path| (DiffCue::RevertFile, path))
            };
            drop(files);
            if let Some((cue, path)) = resolved {
                let apply = apply_diff_cue.clone();
                glib::idle_add_local_once(move || apply(cue, path));
            }
        })
    });

    // Every diff/conflict affordance now lives in the gutter (`GutterColumn`),
    // which owns its own click handling, pointer cursor and tooltips — so the text
    // view needs no in-text click gesture or hover-cursor override any more.

    // Jump the (already-rendered) combined diff to the file at dropdown `idx`,
    // pinning its `diff --git` header to the top of the viewport. The whole change
    // is rendered once by `render_diff_view`; the dropdown is just a navigation
    // aid. Skips the scroll when `nav_sync` is set — i.e. when this selection was
    // itself driven by the scroll→dropdown sync, so the two don't fight.
    // Set the diff view's tab width to the value the repo's editor-config files
    // declare for `path` (falling back to the default when none do). The diff
    // buffer holds all of a commit's files but the view renders one tab width at a
    // time, so this is re-applied whenever the file at the top of the view changes
    // (both navigation entry points below funnel through `scroll_to_file` /
    // `scroll_to_conflict_file`).
    let apply_tab_width: ApplyTabWidth = {
        let file_view = file_view.clone();
        let tab_resolver = tab_resolver.clone();
        Rc::new(move |path: Option<&str>| {
            let width = path
                .and_then(|p| tab_resolver.tab_width(p))
                .unwrap_or(DEFAULT_TAB_WIDTH);
            // Each diff line carries the unified-diff marker (`+`/`-`/space) inline
            // as column 0, so GtkSourceView's `set_tab_width` — which measures tab
            // stops from the line start — lands every stop one cell too far left and
            // eats a column of indentation. Instead install a PangoTabArray whose
            // stops are shifted right by exactly one character cell, so a tab in the
            // code aligns as it would in a normal editor (just offset past the
            // marker). Two explicit stops suffice: Pango extrapolates further stops
            // by repeating the gap between the last two (`get_tab_pos`), yielding an
            // infinite grid at one cell + k tab-widths for k = 1, 2, 3, ….
            //
            // Work in Pango units (1/PANGO_SCALE px), not pixels: a pixel-rounded
            // interval over-rounds by a fraction that Pango then repeats, drifting
            // ~0.4px per tab and visibly across a deep indent. Measuring the space
            // advance with `size()` (sub-pixel exact) and keeping the interval as
            // exact arithmetic lands the stops on the same sub-pixel grid the
            // spaces use, so Pango's single per-glyph display rounding matches.
            let cell = file_view.create_pango_layout(Some(" ")).size().0;
            if cell <= 0 {
                // Font metrics not ready (widget not yet styled); fall back to the
                // built-in unshifted stops rather than installing a degenerate array.
                file_view.set_tab_width(width);
                return;
            }
            let unit = cell * width as i32;
            let mut tabs = gtk::pango::TabArray::new(2, false);
            tabs.set_tab(0, gtk::pango::TabAlign::Left, cell + unit);
            tabs.set_tab(1, gtk::pango::TabAlign::Left, cell + 2 * unit);
            file_view.set_tabs(&tabs);
        })
    };

    let scroll_to_file: Rc<dyn Fn(usize)> = {
        let combined_files = combined_files.clone();
        let current_file = current_file.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let nav_sync = nav_sync.clone();
        let apply_tab_width = apply_tab_width.clone();
        Rc::new(move |idx: usize| {
            let file = combined_files.borrow().get(idx).cloned();
            let Some(file) = file else { return };
            apply_tab_width(Some(&file.path));
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
        let changes = changes.clone();
        let orig_changes = orig_changes.clone();
        let pane_mode = pane_mode.clone();
        Rc::new(move || {
            let has_edits = !pane_mode.borrow().is_conflict()
                && matches!(
                    collect_file_edits(
                        &buffer_text(&file_buffer),
                        &changes.borrow(),
                        &orig_changes.borrow(),
                    ),
                    Ok(edits) if !edits.is_empty()
                );
            split_button.set_sensitive(has_edits);
        })
    };

    // Light the Save button only when there is something to save — mirroring the
    // `save` closure's per-mode notion of "dirty": pending file edits in any
    // editable pane, plus (for a single selected commit) a changed message or
    // identity/date. In conflict mode Save resolves the current conflict, so it is
    // always actionable. Wired to every input that can change that verdict (the
    // diff, message and identity buffers below) and re-run after each (re)load, so
    // it resets to insensitive once a fresh, unedited pane is shown.
    let update_save_sensitivity: Rc<dyn Fn()> = {
        let save_button = save_button.clone();
        let pane_mode = pane_mode.clone();
        let viewing_wc = viewing_wc.clone();
        let selected_change = selected_change.clone();
        let selected_changes = selected_changes.clone();
        let multi_identity_baseline = multi_identity_baseline.clone();
        let identity_fields = identity_fields.clone();
        let original_identity = original_identity.clone();
        let commits = commits.clone();
        let message_buffer = message_buffer.clone();
        let file_buffer = file_buffer.clone();
        let changes = changes.clone();
        let orig_changes = orig_changes.clone();
        Rc::new(move || {
            // The diff buffer carries pending file-content edits — the same edits
            // Save/Split would apply. Shared by every editable pane below.
            let has_file_edits = || {
                matches!(
                    collect_file_edits(
                        &buffer_text(&file_buffer),
                        &changes.borrow(),
                        &orig_changes.borrow(),
                    ),
                    Ok(edits) if !edits.is_empty()
                )
            };
            let dirty = if pane_mode.borrow().is_conflict() {
                // Save resolves the current conflicted file — always actionable.
                true
            } else if viewing_wc.get() {
                // Working-copy entry: pending diff edits can be saved back in place,
                // and a typed commit message turns the uncommitted changes into a
                // commit on HEAD. Identity edits alone don't count — they only ride
                // along with a commit, which needs a message.
                has_file_edits() || !buffer_text(&message_buffer).trim().is_empty()
            } else if selected_changes.borrow().len() > 1 {
                // Batch view: any identity field set and differing from its baseline.
                let baseline = multi_identity_baseline.borrow();
                (0..4).any(|i| {
                    let cur = identity_fields[i].text();
                    !cur.trim().is_empty() && cur.as_str() != baseline[i].as_str()
                })
            } else if let Some(change_id) = selected_change.borrow().clone() {
                // Single commit: message, diff or identity changed vs. the loaded state.
                let message_dirty = commits
                    .borrow()
                    .iter()
                    .find(|c| c.change_id_hex() == change_id)
                    .is_some_and(|c| buffer_text(&message_buffer) != c.description);
                let new_identity = read_identity(&identity_fields);
                let identity_dirty = original_identity.borrow().as_ref() != Some(&new_identity);
                message_dirty || identity_dirty || has_file_edits()
            } else {
                false
            };
            save_button.set_sensitive(dirty);
        })
    };

    file_buffer.connect_changed({
        let collapse = collapse.clone();
        let highlight = highlight.clone();
        let highlight_line = highlight_line.clone();
        let highlight_gen = highlight_gen.clone();
        let editing = editing.clone();
        let update_split_sensitivity = update_split_sensitivity.clone();
        let update_save_sensitivity = update_save_sensitivity.clone();
        move |buffer| {
            // Track Split/Save sensitivity on every change, including programmatic
            // renders (a load leaves an unedited diff -> insensitive).
            update_split_sensitivity();
            update_save_sensitivity();
            // A full programmatic render highlights itself synchronously; don't
            // also schedule a redundant (and flash-inducing) debounced pass.
            if editing.get() {
                return;
            }
            // Repaint the edited line now so its colors and trailing-whitespace
            // flag track the keystroke; the debounced full pass below then fixes
            // cross-line state (multi-line syntax, the intra-line word diff).
            highlight_line(buffer.iter_at_offset(buffer.cursor_position()).line());
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

    // The message and identity inputs feed the same Save verdict as the diff, so a
    // keystroke in any of them re-evaluates it. (A programmatic repopulation on
    // load also fires these, but the final diff render settles the verdict against
    // the now-current baselines.)
    message_buffer.connect_changed({
        let update_save_sensitivity = update_save_sensitivity.clone();
        move |_| update_save_sensitivity()
    });
    for field in identity_fields.iter() {
        field.connect_changed({
            let update_save_sensitivity = update_save_sensitivity.clone();
            move |_| update_save_sensitivity()
        });
    }

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
            let cue = CONFLICT_ELISION_LINE;
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
                    let snip = render_conflict_snippets(&fv.full_text, &fv.exp, cue);
                    for l in snip.text.split('\n') {
                        out.push(l.to_string());
                    }
                    fv.gaps = snip.gaps.iter().map(|g| (g.above, g.below)).collect();
                    fv.pieces = snip.pieces;
                }
            }
            // The markers stay as plain git-style markers; the resolve action is a
            // gutter button (`conflict_cue_cells`), so the buffer text is the
            // conflict snippets verbatim.
            let text = out.join("\n");
            editing.set(true);
            if splice {
                splice_buffer_text(&file_buffer, &text);
            } else {
                // A fresh render starts a new editing context (see the diff
                // pane): clear the undo history rather than letting Ctrl+Z revert
                // the rendered snippets themselves.
                file_buffer.begin_irreversible_action();
                file_buffer.set_text(&text);
                file_buffer.end_irreversible_action();
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
            let cue = CONFLICT_ELISION_LINE;
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
                    // Markers carry no inline cue text (the resolve action is a
                    // gutter button), so the section lines reconstruct verbatim.
                    fv.full_text = reconstruct_conflict_file(section, &fv.pieces, cue);
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
        let apply_tab_width = apply_tab_width.clone();
        Rc::new(move |idx: usize| {
            let path = conflict_view.borrow().get(idx).map(|fv| fv.path.clone());
            let Some(path) = path else { return };
            apply_tab_width(Some(&path));
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
            match plan_edit(
                &buffer_text(buffer),
                caret,
                EditGesture::Insert(text.to_string()),
            ) {
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
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            // Undo/redo replay their changes as ordinary buffer insert/delete ops.
            // Left to the view's built-in bindings they'd fire those signals with
            // the `editing` guard clear, so the structured-edit firewall (and the
            // conflict-layout guard) below would re-plan or block them and corrupt
            // the operation. Drive them ourselves under the guard — which the
            // firewall honours as "our own edit" — then re-highlight, since the
            // guard also suppresses the debounced `changed` re-highlight. The
            // capture-phase `Stop` pre-empts the view's own Ctrl+Z / Ctrl+Y so the
            // undo isn't then applied a second time.
            if ctrl {
                let is_undo = matches!(keyval, gdk::Key::z | gdk::Key::Z) && !shift;
                let is_redo = matches!(keyval, gdk::Key::y | gdk::Key::Y)
                    || (matches!(keyval, gdk::Key::z | gdk::Key::Z) && shift);
                if is_undo || is_redo {
                    editing.set(true);
                    if is_undo {
                        file_buffer.undo();
                    } else {
                        file_buffer.redo();
                    }
                    editing.set(false);
                    highlight();
                    return glib::Propagation::Stop;
                }
            }
            // In conflict mode, structural diff gestures don't apply — let the view
            // handle Enter/Backspace/Delete as ordinary text editing.
            if pane_mode.borrow().is_conflict() {
                return glib::Propagation::Proceed;
            }
            // Cut / copy operate on the diff's content, not its raw prefixed text:
            // strip the one-char `+`/`-`/space marker per line so a later paste
            // (which re-prefixes via the firewall) round-trips cleanly without
            // doubling prefixes, and an external paste gets real code.
            if ctrl && matches!(keyval, gdk::Key::c | gdk::Key::C) && !shift {
                if let Some((s, e)) = file_buffer.selection_bounds() {
                    let raw = file_buffer.text(&s, &e, false);
                    let stripped = strip_selection_prefixes(raw.as_str(), s.line_offset() == 0);
                    file_view.clipboard().set_text(&stripped);
                    return glib::Propagation::Stop;
                }
                return glib::Propagation::Proceed;
            }
            if ctrl && matches!(keyval, gdk::Key::x | gdk::Key::X) {
                let Some((s, e)) = file_buffer.selection_bounds() else {
                    return glib::Propagation::Proceed;
                };
                // Route the delete through the same firewall Backspace/Delete use
                // (with a selection `plan_delete` ignores direction).
                return match plan_edit(
                    &buffer_text(&file_buffer),
                    buffer_selection(&file_buffer),
                    EditGesture::Delete,
                ) {
                    // A safe single-line `+` cut: the selection is mid-content
                    // (no prefix), so GTK's own cut copies and deletes it.
                    EditPlan::Allow => glib::Propagation::Proceed,
                    EditPlan::Block => {
                        show_status(READ_ONLY_HINT);
                        glib::Propagation::Stop
                    }
                    EditPlan::Edit(edit) => {
                        let raw = file_buffer.text(&s, &e, false);
                        let stripped = strip_selection_prefixes(raw.as_str(), s.line_offset() == 0);
                        file_view.clipboard().set_text(&stripped);
                        apply_patch_edit(&file_buffer, &editing, &edit, &*highlight);
                        glib::Propagation::Stop
                    }
                };
            }
            let gesture = match keyval {
                gdk::Key::Return | gdk::Key::KP_Enter => EditGesture::Newline,
                gdk::Key::BackSpace => EditGesture::Backspace,
                gdk::Key::Delete | gdk::Key::KP_Delete => EditGesture::Delete,
                gdk::Key::d | gdk::Key::D if ctrl => EditGesture::DeleteLine,
                _ => return glib::Propagation::Proceed,
            };
            match plan_edit(
                &buffer_text(&file_buffer),
                buffer_selection(&file_buffer),
                gesture,
            ) {
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

    // Load the selected working-copy entry's (editable) diff into the file pane.
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

    // Refresh the right pane from the history list's current selection. One commit
    // is the usual fully-editable pane; several is a read-only batch view (combined
    // diff, common-or-differing identity that Save writes to all, no message edit);
    // none is inert. In conflict mode it shows the anchor's conflicted files, never
    // the multi-commit view. Called on every `selected-rows-changed`, and once by
    // `refresh` after it re-selects rows (guarded by `selection_sync`).
    let update_selection_pane: Rc<dyn Fn()> = {
        let list = list.clone();
        let commits = commits.clone();
        let message_buffer = message_buffer.clone();
        let message_view = message_view.clone();
        let selected_change = selected_change.clone();
        let selected_changes = selected_changes.clone();
        let selection_sync = selection_sync.clone();
        let load_changes = load_changes.clone();
        let load_conflict_files = load_conflict_files.clone();
        let pane_mode = pane_mode.clone();
        let identity_fields = identity_fields.clone();
        let original_identity = original_identity.clone();
        let viewing_wc = viewing_wc.clone();
        let wc_list = wc_list.clone();
        let repo = repo.clone();
        let apply_changes = apply_changes.clone();
        let file_buffer = file_buffer.clone();
        let editing = editing.clone();
        let diff_read_only = diff_read_only.clone();
        let multi_identity_baseline = multi_identity_baseline.clone();
        let update_save_sensitivity = update_save_sensitivity.clone();
        let save_button = save_button.clone();
        Rc::new(move || {
            if selection_sync.get() {
                return;
            }
            // The selected commits in display order (newest first).
            let mut indices: Vec<usize> = list
                .selected_rows()
                .iter()
                .filter_map(|r| {
                    let i = r.index();
                    (i >= 0).then_some(i as usize)
                })
                .collect();
            indices.sort_unstable();
            let infos: Vec<CommitInfo> = {
                let cs = commits.borrow();
                indices.iter().filter_map(|&i| cs.get(i).cloned()).collect()
            };
            *selected_changes.borrow_mut() = infos.iter().map(|c| c.change_id_hex()).collect();
            *selected_change.borrow_mut() = infos.first().map(|c| c.change_id_hex());

            // Leaving the read-only working-copy view (mutually exclusive selection).
            viewing_wc.set(false);
            wc_list.unselect_all();

            // Conflict mode is per-commit: show the anchor's conflicted files
            // regardless of how many rows are selected; never the multi view.
            if pane_mode.borrow().is_conflict() {
                if let Some(info) = infos.first() {
                    message_buffer.set_text(&info.description);
                    load_conflict_files(info);
                }
                return;
            }
            // Back on a history selection: restore the ordinary diff-save hint that
            // the working-copy view swapped out (conflict mode handled above).
            save_button.set_tooltip_text(Some(SAVE_HINT_DIFF));

            match infos.as_slice() {
                [] => {
                    // Nothing selected: a neutral, inert pane.
                    message_view.set_editable(false);
                    message_buffer.set_text("");
                    clear_identity_differs(&identity_fields);
                    for f in identity_fields.iter() {
                        f.set_text("");
                        f.set_sensitive(false);
                    }
                    *original_identity.borrow_mut() = None;
                    diff_read_only.set(false);
                    apply_changes(Vec::new());
                }
                [info] => {
                    // Single commit: the usual fully-editable pane.
                    message_view.set_editable(true);
                    clear_identity_differs(&identity_fields);
                    for f in identity_fields.iter() {
                        f.set_sensitive(true);
                    }
                    message_buffer.set_text(&info.description);
                    set_identity_fields(&identity_fields, info);
                    *original_identity.borrow_mut() = Some(read_identity(&identity_fields));
                    diff_read_only.set(false);
                    load_changes(info);
                }
                infos => {
                    // Several commits: a read-only batch view.
                    message_view.set_editable(false);
                    set_note(
                        &message_buffer,
                        "Multiple commits selected — message not editable.",
                    );
                    // Populate the fields first: their `changed` signals re-enter
                    // `update_save_sensitivity`, which reads the baseline — so store
                    // it only once the (now-settled) fields are in place.
                    let baseline = set_identity_fields_common(&identity_fields, infos);
                    *multi_identity_baseline.borrow_mut() = baseline;
                    for f in identity_fields.iter() {
                        f.set_sensitive(true);
                    }
                    *original_identity.borrow_mut() = None;
                    // The combined diff over the selection, applied oldest first.
                    let ids: Vec<_> = infos.iter().rev().map(|c| c.id.clone()).collect();
                    let combined = {
                        let r = repo.borrow();
                        combined_changes(&r.repo, &ids)
                    };
                    diff_read_only.set(true);
                    match combined {
                        Ok(Some(ch)) => apply_changes(ch),
                        Ok(None) => {
                            apply_changes(Vec::new());
                            editing.set(true);
                            set_note(
                                &file_buffer,
                                "The combined diff of the selected commits is conflicting — \
                                 not representable as a single diff.",
                            );
                            editing.set(false);
                        }
                        Err(err) => {
                            apply_changes(Vec::new());
                            editing.set(true);
                            set_note(
                                &file_buffer,
                                &format!("Failed to build the combined diff: {err}"),
                            );
                            editing.set(false);
                        }
                    }
                }
            }
            // Settle the Save verdict against the freshly loaded baselines: the
            // diff render above already fires it, but an unchanged (e.g. empty)
            // buffer may not, so re-run it explicitly here.
            update_save_sensitivity();
        })
    };

    // Selecting commit(s) updates the right pane. This fires for programmatic and
    // keyboard selection changes; mouse clicks are handled by the gesture below
    // (which drives selection under `selection_sync`, so this is a no-op for them).
    list.connect_selected_rows_changed({
        let update_selection_pane = update_selection_pane.clone();
        move |_| update_selection_pane()
    });

    // Own the click→selection mapping. GtkListBox's built-in multiple-selection
    // *toggles* on a plain click (so clicks accumulate); we want the conventional
    // plain = select only this row, Ctrl = toggle it, Shift = range from the anchor.
    // A capture-phase handler does the selection and claims the press, so
    // GtkListBox's own (bubble-phase) selection never runs. It claims on *release*,
    // not press, so a drag still begins on motion beforehand (see `dragdrop`).
    let select_click = gtk::GestureClick::new();
    select_click.set_button(gdk::BUTTON_PRIMARY);
    select_click.set_propagation_phase(PropagationPhase::Capture);
    select_click.connect_released({
        let list = list.clone();
        let update_selection_pane = update_selection_pane.clone();
        let selection_sync = selection_sync.clone();
        let selection_anchor = selection_anchor.clone();
        move |gesture, _n, _x, y| {
            let Some(row) = list.row_at_y(y as i32) else {
                return;
            };
            let i = row.index();
            let state = gesture.current_event_state();
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            // Drive the selection ourselves under `selection_sync`, then render once.
            selection_sync.set(true);
            if shift {
                // Extend from the anchor (or the current selection's nearest row) to
                // the clicked row, selecting everything between.
                let anchor = selection_anchor
                    .get()
                    .or_else(|| list.selected_rows().iter().map(|r| r.index()).min())
                    .unwrap_or(i);
                list.unselect_all();
                for j in i.min(anchor)..=i.max(anchor) {
                    if let Some(r) = list.row_at_index(j) {
                        list.select_row(Some(&r));
                    }
                }
            } else if ctrl {
                // Toggle just this row, leaving the rest of the selection.
                if row.is_selected() {
                    list.unselect_row(&row);
                } else {
                    list.select_row(Some(&row));
                }
                selection_anchor.set(Some(i));
            } else {
                // Plain click: select only this row.
                list.unselect_all();
                list.select_row(Some(&row));
                selection_anchor.set(Some(i));
            }
            selection_sync.set(false);
            update_selection_pane();
            // Focus the clicked row so up/down keep navigating the history list.
            // Claiming the press (below) suppresses GtkListBox's own click handling,
            // which would otherwise grab focus here; without this, focus lingers in
            // whatever pane held it (typically the diff view).
            row.grab_focus();
            // Claim the press so GtkListBox's own selection handling doesn't also run.
            gesture.set_state(gtk::EventSequenceState::Claimed);
        }
    });
    list.add_controller(select_click);

    // Selecting a working-copy entry shows its editable diff and opens the
    // message/identity fields to craft a commit from the uncommitted changes:
    // Save with an empty message edits the entry in place, Save with a message
    // commits it on HEAD (see the `save` closure). The identity is prefilled with
    // the git default. The selected entry is tracked by its stable change id.
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
        let selected_changes = selected_changes.clone();
        let selection_sync = selection_sync.clone();
        let diff_read_only = diff_read_only.clone();
        let repo = repo.clone();
        let wc_identity_baseline = wc_identity_baseline.clone();
        let save_button = save_button.clone();
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
            // Mutually exclusive with the history selection. Drop the multi-set and
            // its "(differs)" styling, and clear it under `selection_sync` so the
            // pane router doesn't fire and clobber the working-copy view below.
            selected_changes.borrow_mut().clear();
            clear_identity_differs(&identity_fields);
            selection_sync.set(true);
            list.unselect_all();
            selection_sync.set(false);
            // Craft a commit from the uncommitted changes: the message starts empty
            // (typing one turns Save from "save the diff in place" into "commit on
            // HEAD"), and the identity fields are prefilled with the git default so
            // the author/committer can be overridden — recorded as the baseline the
            // save compares against to tell an override from the untouched default.
            message_buffer.set_text("");
            message_view.set_editable(true);
            let baseline =
                set_identity_fields_from(&identity_fields, &repo.borrow().default_identity());
            *wc_identity_baseline.borrow_mut() = baseline;
            for f in identity_fields.iter() {
                f.set_sensitive(true);
            }
            save_button.set_tooltip_text(Some(SAVE_HINT_WORKCOPY));
            // The working-copy diff is editable; clear any multi-select read-only.
            diff_read_only.set(false);
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

    // The history rows' subject revert button calls back here with the clicked
    // row's display index. The real handler needs `refresh` / `enter_conflict_mode`
    // (built below), so it can't exist when `refresh` is built — break the cycle
    // with a slot filled in once everything is constructed, behind a stable wrapper
    // the rows capture now and that reads the slot at click time.
    let on_revert_slot: Rc<RefCell<Option<RevertCallback>>> = Rc::new(RefCell::new(None));
    let on_revert: RevertCallback = {
        let slot = on_revert_slot.clone();
        Rc::new(move |idx| {
            if let Some(handler) = slot.borrow().as_ref() {
                handler(idx);
            }
        })
    };

    // The history rows' merge-out button uses the same slot-behind-a-stable-wrapper
    // shape as the revert button (it needs `refresh` / `enter_conflict_mode`, built
    // below): the rows capture `on_merge_out` now, the real handler is filled in once
    // everything is constructed.
    let on_merge_out_slot: Rc<RefCell<Option<MergeOutCallback>>> = Rc::new(RefCell::new(None));
    let on_merge_out: MergeOutCallback = {
        let slot = on_merge_out_slot.clone();
        Rc::new(move |idx| {
            if let Some(handler) = slot.borrow().as_ref() {
                handler(idx);
            }
        })
    };

    // The trash rows' restore button has the same slot-behind-a-stable-wrapper
    // shape as the revert button: the rows (and `dragdrop`'s trash repopulation)
    // capture `on_restore` now, the real handler — which needs `refresh` etc. —
    // is filled in once everything is built.
    let on_restore_slot: Rc<RefCell<Option<RestoreToWorktreeCallback>>> =
        Rc::new(RefCell::new(None));
    let on_restore: RestoreToWorktreeCallback = {
        let slot = on_restore_slot.clone();
        Rc::new(move |idx| {
            if let Some(handler) = slot.borrow().as_ref() {
                handler(idx);
            }
        })
    };

    // The history rows' commit-style lint badge has the same slot-behind-a-stable-
    // wrapper shape: the rows capture `on_lint` now, the real handler (which needs
    // `refresh` etc.) is filled in once everything is built.
    let on_lint_slot: Rc<RefCell<Option<LintFixCallback>>> = Rc::new(RefCell::new(None));
    let on_lint: LintFixCallback = {
        let slot = on_lint_slot.clone();
        Rc::new(move |idx| {
            if let Some(handler) = slot.borrow().as_ref() {
                handler(idx);
            }
        })
    };

    // Reload history from the engine, preserving the selected commit by its
    // (rewrite-stable) change id.
    let refresh: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let commits = commits.clone();
        let graph = graph.clone();
        let list = list.clone();
        let selected_change = selected_change.clone();
        let selected_changes = selected_changes.clone();
        let selection_sync = selection_sync.clone();
        let update_selection_pane = update_selection_pane.clone();
        let identities = identities.clone();
        let history_limit = history_limit.clone();
        let history_has_more = history_has_more.clone();
        let refresh_wc = refresh_wc.clone();
        let on_revert = on_revert.clone();
        let on_merge_out = on_merge_out.clone();
        let on_lint = on_lint.clone();
        let search_query = search_query.clone();
        let search_matches = search_matches.clone();
        let search_cursor = search_cursor.clone();
        Rc::new(move || {
            let (loaded, has_more) = {
                let r = repo.borrow();
                match r.head_commit_id() {
                    Some(head) => {
                        history_limited(&r.repo, &head, 0, history_limit.get()).unwrap_or_default()
                    }
                    None => (Vec::new(), false),
                }
            };
            history_has_more.set(has_more);
            *commits.borrow_mut() = loaded;
            {
                let root = repo.borrow().root_commit_id();
                *graph.borrow_mut() = compute_graph(&commits.borrow(), &root);
            }
            {
                let cs = commits.borrow();
                let refs = repo.borrow().commit_refs();
                // Learn this repo's de-facto commit-message conventions from its own
                // history, so the per-row lint badge flags drift from *its* norm.
                let subjects: Vec<&str> = cs.iter().map(|c| c.subject.as_str()).collect();
                let style = msglint::RepoStyle::learn(&subjects);
                populate_list(
                    &list,
                    &cs,
                    &HashSet::new(),
                    &refs,
                    &graph,
                    Some(&on_revert),
                    Some(&on_merge_out),
                    Some(&on_lint),
                    Some(&style),
                );
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
            // Re-select the previous selection by (rewrite-stable) change id: the
            // full multi-set when several were selected, otherwise the single anchor
            // (single-commit ops update only the anchor). Programmatic selection is
            // guarded so the pane router runs once, at the end, rather than per row.
            let targets: Vec<String> = {
                let multi = selected_changes.borrow();
                if multi.len() > 1 {
                    multi.clone()
                } else {
                    selected_change.borrow().clone().into_iter().collect()
                }
            };
            selection_sync.set(true);
            list.unselect_all();
            for change in &targets {
                let idx = commits
                    .borrow()
                    .iter()
                    .position(|c| c.change_id_hex() == *change);
                if let Some(idx) = idx {
                    if let Some(row) = list.row_at_index(idx as i32) {
                        list.select_row(Some(&row));
                    }
                }
            }
            selection_sync.set(false);
            update_selection_pane();
            // populate_list reset the row labels to plain text; re-apply an active
            // search so its highlights survive the rebuild. The selection was just
            // restored by change id, so the Enter cursor is stale — reset it.
            {
                let query = search_query.borrow();
                if !query.is_empty() {
                    *search_matches.borrow_mut() =
                        rows::apply_search_highlight(&list, &commits.borrow(), &query);
                    search_cursor.set(None);
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
    // quick-resolve affordances are the gutter "keep" buttons (see
    // `conflict_cue_cells`).
    let enter_conflict_mode =
        conflict::build_enter_conflict_mode(&widgets, &data, refresh_conflict.clone());

    // Fill the revert-button slot now that `refresh` / `enter_conflict_mode` exist.
    // The handler drops a revert of the clicked commit directly on top of it (its
    // descendants rebase onto the revert). Deferred to idle because it rebuilds the
    // very row whose button fired — the same widget-tree-mutation hazard as
    // `dragdrop::run_post_drag`.
    *on_revert_slot.borrow_mut() = Some({
        let repo = repo.clone();
        let commits = commits.clone();
        let graph = graph.clone();
        let selected_change = selected_change.clone();
        let selected_changes = selected_changes.clone();
        let pane_mode = pane_mode.clone();
        let refresh = refresh.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        let show_status = show_status.clone();
        Rc::new(move |idx: i32| {
            let repo = repo.clone();
            let commits = commits.clone();
            let graph = graph.clone();
            let selected_change = selected_change.clone();
            let selected_changes = selected_changes.clone();
            let pane_mode = pane_mode.clone();
            let refresh = refresh.clone();
            let enter_conflict_mode = enter_conflict_mode.clone();
            let show_status = show_status.clone();
            glib::idle_add_local_once(move || {
                if pane_mode.borrow().is_conflict() {
                    show_status("Resolve the pending conflict before reverting");
                    return;
                }
                // Resolve the clicked commit and the slot to splice its revert into.
                let (target, change, new_children) = {
                    let commits = commits.borrow();
                    let Some(commit) = commits.get(idx as usize) else {
                        return;
                    };
                    let target = commit.id.clone();
                    let change = commit.change_id_hex();
                    // Parent the revert on the clicked commit; its children are the
                    // commit's current branch children, which rebase onto the
                    // revert. The clicked commit (display index `idx`) is the parent
                    // of the lane edge crossing the gap just above it
                    // (`boundaries[idx - 1]`); at the tip (idx 0) there are no
                    // children and the revert becomes the new HEAD.
                    let new_children = if idx == 0 {
                        Vec::new()
                    } else {
                        graph
                            .borrow()
                            .boundaries
                            .get(idx as usize - 1)
                            .and_then(|edges| edges.iter().find(|e| e.parent == target))
                            .map(|e| e.children.clone())
                            .unwrap_or_default()
                    };
                    (target, change, new_children)
                };
                let outcome = repo.borrow_mut().revert_commit(
                    &target,
                    vec![target.clone()],
                    new_children,
                    None,
                );
                match outcome {
                    Ok(SaveOutcome::Clean) => {
                        // Re-select just the clicked commit; the revert sits above it.
                        *selected_changes.borrow_mut() = vec![change.clone()];
                        *selected_change.borrow_mut() = Some(change);
                        refresh();
                    }
                    Ok(SaveOutcome::Conflicts { commits }) => enter_conflict_mode(commits),
                    Err(err) => show_status(&format!("Revert failed: {err}")),
                }
            });
        }) as RevertCallback
    });

    // Fill the merge-out-button slot, now that `refresh` / `enter_conflict_mode`
    // exist. The handler introduces a merge directly above the clicked commit — the
    // commit becomes a side branch the merge folds back, its descendants rebasing
    // onto the merge. Deferred to idle (it rebuilds the very row whose button fired)
    // and computes the splice slot exactly like the revert handler.
    *on_merge_out_slot.borrow_mut() = Some({
        let repo = repo.clone();
        let commits = commits.clone();
        let graph = graph.clone();
        let selected_change = selected_change.clone();
        let selected_changes = selected_changes.clone();
        let pane_mode = pane_mode.clone();
        let refresh = refresh.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        let show_status = show_status.clone();
        Rc::new(move |idx: i32| {
            let repo = repo.clone();
            let commits = commits.clone();
            let graph = graph.clone();
            let selected_change = selected_change.clone();
            let selected_changes = selected_changes.clone();
            let pane_mode = pane_mode.clone();
            let refresh = refresh.clone();
            let enter_conflict_mode = enter_conflict_mode.clone();
            let show_status = show_status.clone();
            glib::idle_add_local_once(move || {
                if pane_mode.borrow().is_conflict() {
                    show_status("Resolve the pending conflict before introducing a merge");
                    return;
                }
                // Resolve the clicked commit and the slot the merge splices into.
                let (target, change, new_children) = {
                    let commits = commits.borrow();
                    let Some(commit) = commits.get(idx as usize) else {
                        return;
                    };
                    let target = commit.id.clone();
                    let change = commit.change_id_hex();
                    // The new merge takes the gap just above the clicked commit; its
                    // children are the commit's current branch children (which rebase
                    // onto the merge), the lane edge crossing that gap
                    // (`boundaries[idx - 1]`). At the tip (idx 0) there are none and
                    // the merge becomes the new HEAD.
                    let new_children = if idx == 0 {
                        Vec::new()
                    } else {
                        graph
                            .borrow()
                            .boundaries
                            .get(idx as usize - 1)
                            .and_then(|edges| edges.iter().find(|e| e.parent == target))
                            .map(|e| e.children.clone())
                            .unwrap_or_default()
                    };
                    (target, change, new_children)
                };
                let outcome = repo.borrow_mut().merge_out_commit(&target, new_children);
                match outcome {
                    Ok(SaveOutcome::Clean) => {
                        // Re-select just the clicked commit; the new merge sits just
                        // above it, one click away to reword its pro-forma message.
                        *selected_changes.borrow_mut() = vec![change.clone()];
                        *selected_change.borrow_mut() = Some(change);
                        refresh();
                    }
                    Ok(SaveOutcome::Conflicts { commits }) => enter_conflict_mode(commits),
                    Err(err) => show_status(&format!("Merge-out failed: {err}")),
                }
            });
        }) as MergeOutCallback
    });

    // Fill the trash restore-button slot, now that `refresh` / `enter_conflict_mode`
    // exist. The handler writes the clicked trashed commit's changes to the working
    // tree as uncommitted edits and drops it from the trash. Deferred to idle — like
    // the revert button and `dragdrop::run_post_drag` — because it rebuilds the very
    // trash row whose button fired.
    *on_restore_slot.borrow_mut() = Some({
        let repo = repo.clone();
        let trashed = trashed.clone();
        let pending_trash_op = pending_trash_op.clone();
        let trash_list = trash_list.clone();
        let trash_scroll = trash_scroll.clone();
        let refresh = refresh.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        let show_status = show_status.clone();
        let pane_mode = pane_mode.clone();
        let on_restore = on_restore.clone();
        Rc::new(move |idx: i32| {
            let repo = repo.clone();
            let trashed = trashed.clone();
            let pending_trash_op = pending_trash_op.clone();
            let trash_list = trash_list.clone();
            let trash_scroll = trash_scroll.clone();
            let refresh = refresh.clone();
            let enter_conflict_mode = enter_conflict_mode.clone();
            let show_status = show_status.clone();
            let pane_mode = pane_mode.clone();
            let on_restore = on_restore.clone();
            glib::idle_add_local_once(move || {
                if pane_mode.borrow().is_conflict() {
                    show_status(
                        "Resolve the pending conflict before restoring to the working tree",
                    );
                    return;
                }
                let Some(info) = trashed.borrow().get(idx as usize).cloned() else {
                    return;
                };
                let outcome = repo.borrow_mut().restore_to_working_copy(&info.id);
                match outcome {
                    Ok(SaveOutcome::Clean) => {
                        // Its changes are now uncommitted; drop it from the trash.
                        let change_hex = info.change_id_hex();
                        trashed
                            .borrow_mut()
                            .retain(|c| c.change_id_hex() != change_hex);
                        // refresh() rebuilds history + the working-copy rows (the new
                        // uncommitted entry); repopulate the trash to drop its row.
                        refresh();
                        populate_trash(
                            &trash_list,
                            &trash_scroll,
                            &trashed.borrow(),
                            worktree_bound.then_some(&on_restore),
                        );
                        show_status("Restored the commit's changes to the working tree");
                    }
                    Ok(SaveOutcome::Conflicts { commits }) => {
                        // The changes overlap existing uncommitted edits: hold the
                        // trash removal until the overlap resolves clean (dropped on
                        // abort), and resolve it like any conflict.
                        *pending_trash_op.borrow_mut() =
                            Some(PendingTrashOp::Restore(Box::new(info.clone())));
                        enter_conflict_mode(commits);
                    }
                    Err(err) => show_status(&format!("Restore to working tree failed: {err}")),
                }
            });
        }) as RestoreToWorktreeCallback
    });

    // Fill the lint-badge slot, now that `refresh` / `enter_conflict_mode` exist. The
    // handler tidies the clicked commit's summary to match the repo's de-facto style:
    // it auto-fixes the mechanical issues (case, trailing period) in place, and when
    // only judgment issues remain (a missing prefix, an over-long summary) it selects
    // the commit and focuses the message editor for a manual fix instead. Deferred to
    // idle (it rebuilds the very row whose badge fired), like the revert handler.
    *on_lint_slot.borrow_mut() = Some({
        let repo = repo.clone();
        let commits = commits.clone();
        let selected_change = selected_change.clone();
        let selected_changes = selected_changes.clone();
        let pane_mode = pane_mode.clone();
        let refresh = refresh.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        let show_status = show_status.clone();
        let message_view = message_view.clone();
        Rc::new(move |idx: i32| {
            let repo = repo.clone();
            let commits = commits.clone();
            let selected_change = selected_change.clone();
            let selected_changes = selected_changes.clone();
            let pane_mode = pane_mode.clone();
            let refresh = refresh.clone();
            let enter_conflict_mode = enter_conflict_mode.clone();
            let show_status = show_status.clone();
            let message_view = message_view.clone();
            glib::idle_add_local_once(move || {
                if pane_mode.borrow().is_conflict() {
                    show_status("Resolve the pending conflict before editing a message");
                    return;
                }
                // Resolve the clicked commit and re-learn the repo's style (commits
                // may have changed since the badge was painted).
                let resolved = {
                    let cs = commits.borrow();
                    let Some(commit) = cs.get(idx as usize) else {
                        return;
                    };
                    let subjects: Vec<&str> = cs.iter().map(|c| c.subject.as_str()).collect();
                    let style = msglint::RepoStyle::learn(&subjects);
                    (
                        commit.id.clone(),
                        commit.change_id_hex(),
                        commit.description.clone(),
                        msglint::autofix_subject(&commit.subject, &style),
                    )
                };
                let (commit_id, change, description, fixed) = resolved;
                // Re-select the clicked commit by its (rewrite-stable) change id
                // either way, so the pane follows it.
                *selected_changes.borrow_mut() = vec![change.clone()];
                *selected_change.borrow_mut() = Some(change);
                let Some(fixed_subject) = fixed else {
                    // Only judgment issues remain (or it's already clean): show it for
                    // a manual edit rather than guessing.
                    refresh();
                    message_view.grab_focus();
                    show_status("Edit the summary to match this repo's commit style");
                    return;
                };
                let new_message = msglint::replace_subject(&description, &fixed_subject);
                let outcome = repo.borrow_mut().rewrite_message(&commit_id, &new_message);
                match outcome {
                    Ok(SaveOutcome::Clean) => {
                        show_status("Tidied the commit summary to match the repo's style");
                        refresh();
                    }
                    Ok(SaveOutcome::Conflicts { commits }) => enter_conflict_mode(commits),
                    Err(err) => show_status(&format!("Couldn't fix the summary: {err}")),
                }
            });
        }) as LintFixCallback
    });

    // The late-bound callbacks the peeled modules invoke. Assembled here, after
    // its members exist, and handed to `dragdrop`/`conflict` by reference.
    let callbacks = Callbacks {
        refresh: refresh.clone(),
        show_status: show_status.clone(),
        enter_conflict_mode: enter_conflict_mode.clone(),
        exit_conflict_mode: exit_conflict_mode.clone(),
        on_restore: on_restore.clone(),
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
        on_restore.clone(),
    );

    // Render the session "Compare" into its (read-only) full-window buffer: the
    // content delta between the current tree and the one the session started
    // with. Recomputed each time the view is shown — and after "Revert all" — so
    // it always reflects the live tree. A message/identity-only edit changes no
    // tree, so it produces an empty diff; after a revert it empties too.
    let render_compare: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let compare_buffer = compare_buffer.clone();
        let syntax_set = syntax_set.clone();
        let theme = theme.clone();
        let show_status = show_status.clone();
        Rc::new(move || {
            let changes = match repo.borrow_mut().session_changes() {
                Ok(changes) => changes,
                Err(err) => {
                    show_status(&format!("Compare failed: {err}"));
                    return;
                }
            };
            if changes.is_empty() {
                compare_buffer.set_text("No content changes since the session started.");
                return;
            }
            // Default context, no expand cues: the compare view is read-only, so
            // the diff pane's hunk-expansion wiring deliberately doesn't apply.
            let combined = render_commit_diff(&changes, &HashMap::new());
            compare_buffer.set_text(&combined.text);
            let first = combined.files.first().map(|f| f.path.as_str());
            highlight_diff(&compare_buffer, first, &syntax_set, &theme);
        })
    };

    // "Compare" toggle: swap the whole window between the editor and the
    // read-only session diff. Computing the diff snapshots the working copy
    // (which can move `@`), so refresh the now-hidden editor to keep its
    // history/`@`-row consistent.
    compare_button.connect_toggled({
        let content_stack = content_stack.clone();
        let render_compare = render_compare.clone();
        let refresh = refresh.clone();
        move |btn| {
            if btn.is_active() {
                render_compare();
                content_stack.set_visible_child_name("compare");
                refresh();
            } else {
                content_stack.set_visible_child_name("edit");
            }
        }
    });

    // "Edit history" (top-right header button): open a dropdown of this session's
    // snapshots — every recorded edit, newest first, plus the session-start floor
    // — and let the user travel the repository back (or forward) to any of them.
    // The bottom "Session start" entry is the old "Revert all". Clicking an entry
    // calls `jump_to_op`, then runs the same post-rewrite reset the revert handler
    // used (drop conflict mode, empty the trash, reload + reselect); hovering an
    // entry highlights the history row(s) that operation touched.
    history_button.connect_clicked({
        let repo = repo.clone();
        let exit_conflict_mode = exit_conflict_mode.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let list = list.clone();
        let commits = commits.clone();
        let trashed = trashed.clone();
        let pending_trash_op = pending_trash_op.clone();
        let trash_list = trash_list.clone();
        let trash_scroll = trash_scroll.clone();
        let on_restore = on_restore.clone();
        let compare_button = compare_button.clone();
        let render_compare = render_compare.clone();
        move |btn| {
            // Snapshot the dropdown's data: (jump target, label, affected change-ids),
            // newest first, then the session-start floor (target 0). `cursor` marks
            // the current state; entries ahead of it are redo targets.
            let mut entries: Vec<(usize, String, Vec<String>)> = Vec::new();
            let cursor;
            let floor_subtitle;
            {
                let r = repo.borrow();
                cursor = r.op_cursor();
                for (i, entry) in r.session_ops().iter().enumerate().rev() {
                    entries.push((i + 1, entry.label().to_string(), entry.affected().to_vec()));
                }
                floor_subtitle = r
                    .session_start_head_hex()
                    .map(|h| h[..h.len().min(12)].to_string());
                entries.push((0, "Session start".to_string(), Vec::new()));
            }
            let entries = Rc::new(entries);

            let list_box = ListBox::new();
            list_box.set_selection_mode(gtk::SelectionMode::None);
            for (target, label, affected) in entries.iter() {
                let subtitle = if *target == 0 {
                    floor_subtitle.as_deref()
                } else {
                    None
                };
                let row = history_row(label, subtitle, *target == cursor, *target > cursor);
                // Hover the entry → highlight the commit row(s) it touched.
                let motion = gtk::EventControllerMotion::new();
                motion.connect_enter({
                    let list = list.clone();
                    let commits = commits.clone();
                    let affected = affected.clone();
                    move |_, _, _| {
                        clear_highlight(&list);
                        highlight_affected(&list, &commits.borrow(), &affected);
                    }
                });
                motion.connect_leave({
                    let list = list.clone();
                    move |_| clear_highlight(&list)
                });
                row.add_controller(motion);
                list_box.append(&row);
            }

            let scroll = ScrolledWindow::builder()
                .propagate_natural_height(true)
                .propagate_natural_width(true)
                .min_content_width(260)
                .max_content_height(360)
                .hscrollbar_policy(PolicyType::Never)
                .child(&list_box)
                .build();
            let popover = Popover::new();
            popover.set_parent(btn);
            popover.set_autohide(true);
            popover.set_child(Some(&scroll));

            list_box.connect_row_activated({
                let popover = popover.clone();
                let entries = entries.clone();
                let repo = repo.clone();
                let exit_conflict_mode = exit_conflict_mode.clone();
                let enter_conflict_mode = enter_conflict_mode.clone();
                let refresh = refresh.clone();
                let show_status = show_status.clone();
                let list = list.clone();
                let trashed = trashed.clone();
                let pending_trash_op = pending_trash_op.clone();
                let trash_list = trash_list.clone();
                let trash_scroll = trash_scroll.clone();
                let on_restore = on_restore.clone();
                let compare_button = compare_button.clone();
                let render_compare = render_compare.clone();
                move |_, row| {
                    let Some((target, label, _)) = entries.get(row.index() as usize) else {
                        return;
                    };
                    let (target, label) = (*target, label.clone());
                    popover.popdown();
                    // Bind the outcome before the match so `repo`'s borrow is
                    // dropped before the reset closures re-borrow it.
                    let outcome = repo.borrow_mut().jump_to_op(target);
                    let outcome = match outcome {
                        Ok(o) => o,
                        Err(err) => {
                            show_status(&format!("Time-travel failed: {err}"));
                            return;
                        }
                    };
                    // A recorded op should always re-export clean; handle conflicts
                    // defensively by entering the resolution flow.
                    if let SaveOutcome::Conflicts { commits } = outcome {
                        enter_conflict_mode(commits);
                        return;
                    }
                    // Drop conflict mode if we were resolving (idempotent otherwise).
                    exit_conflict_mode();
                    // The jump undoes the session's drops, so empty the trash bin
                    // and drop any trash change a held-back rewrite was waiting on.
                    pending_trash_op.borrow_mut().take();
                    trashed.borrow_mut().clear();
                    populate_trash(
                        &trash_list,
                        &trash_scroll,
                        &trashed.borrow(),
                        worktree_bound.then_some(&on_restore),
                    );
                    // `refresh` re-selects the prior selection by change id and
                    // re-renders the pane; if it's gone after the jump, select the tip.
                    refresh();
                    if list.selected_rows().is_empty() {
                        if let Some(row) = list.row_at_index(0) {
                            list.select_row(Some(&row));
                        }
                    }
                    if compare_button.is_active() {
                        render_compare();
                    }
                    show_status(&format!("Travelled to: {label}"));
                }
            });

            // Drop any lingering hover highlight and detach the popover on close.
            popover.connect_closed({
                let list = list.clone();
                move |p| {
                    clear_highlight(&list);
                    p.unparent();
                }
            });
            popover.popup();
        }
    });

    // "Reload" (top-left header button): re-open the repository from disk so
    // edits made outside commedit (a `git commit`, branch switch, …) show up —
    // a fresh session in place, same as restarting the app. Session state
    // (edit history, trash, a pending conflict, the split working-copy chain)
    // is dropped, exactly as a restart would; `Repo::open` re-snapshots the
    // working copy and collapses the chain to git's single-pile view.
    reload_button.connect_clicked({
        let repo = repo.clone();
        let repo_path = repo_path.clone();
        let branch = branch.clone();
        let exit_conflict_mode = exit_conflict_mode.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let list = list.clone();
        let trashed = trashed.clone();
        let pending_trash_op = pending_trash_op.clone();
        let trash_list = trash_list.clone();
        let trash_scroll = trash_scroll.clone();
        let on_restore = on_restore.clone();
        let history_limit = history_limit.clone();
        let compare_button = compare_button.clone();
        let render_compare = render_compare.clone();
        move |_| {
            // Open the new session before dropping the old one, so a failed
            // reload leaves the current state untouched (concurrent sessions
            // are supported, so the brief overlap is fine). Cached like the
            // initial open; the old session flushes its index as it drops.
            let reopened = match Repo::open_branch(
                &repo_path,
                commedit_engine::index_cache::IndexCache::Default,
                branch.as_deref(),
            ) {
                Ok(r) => r,
                Err(err) => {
                    show_status(&format!("Reload failed: {err:#}"));
                    return;
                }
            };
            *repo.borrow_mut() = reopened;
            // The same post-rewrite reset a time-travel jump runs: the fresh
            // session has no conflict in flight and an empty trash.
            exit_conflict_mode();
            pending_trash_op.borrow_mut().take();
            trashed.borrow_mut().clear();
            populate_trash(
                &trash_list,
                &trash_scroll,
                &trashed.borrow(),
                worktree_bound.then_some(&on_restore),
            );
            history_limit.set(HISTORY_PAGE);
            // `refresh` re-selects the prior selection by change id and re-renders
            // the pane; if it's gone after the reload, select the tip.
            refresh();
            if list.selected_rows().is_empty() {
                if let Some(row) = list.row_at_index(0) {
                    list.select_row(Some(&row));
                }
            }
            if compare_button.is_active() {
                render_compare();
            }
            show_status("Repository reloaded");
        }
    });

    // Drag-and-drop to reorder commits. While dragging, a placeholder row opens a
    // gap at the hover position (the surrounding commits slide to make room) and
    // the dragged row is dimmed; dropping rebases the commit into that slot via
    // the engine and reloads. The reorder is applied immediately — there is no
    // separate Save step for it.

    dragdrop::wire(&widgets, &data, &drag_state, &callbacks);
    populate_trash(
        &trash_list,
        &trash_scroll,
        &trashed.borrow(),
        worktree_bound.then_some(&on_restore),
    );

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
        let selected_changes = selected_changes.clone();
        let multi_identity_baseline = multi_identity_baseline.clone();
        let wc_identity_baseline = wc_identity_baseline.clone();
        let list = list.clone();
        Rc::new(move || {
            // In conflict mode, "Save" means "resolve the current conflicted file".
            if pane_mode.borrow().is_conflict() {
                resolve_current();
                return;
            }
            // Viewing a working-copy entry. The commit message gates what Save does:
            // with no message, the edited diff is written back to the working copy
            // in place (it stays uncommitted); with a message, the uncommitted
            // changes are crystallized into a real commit on top of HEAD.
            if viewing_wc.get() {
                let saved_file = current_file.borrow().clone();
                let saved_cursor = file_buffer.cursor_position();
                // Flush any pending diff edits into the working copy first — both
                // paths want the on-disk tree to match the shown diff, whether it's
                // re-rendered (no message) or committed (message). Editing in place
                // moves no tip, so a per-file loop is fine.
                let edits = match collect_file_edits(
                    &buffer_text(&file_buffer),
                    &changes.borrow(),
                    &orig_changes.borrow(),
                ) {
                    Ok(edits) => edits,
                    Err(msg) => {
                        show_status(&msg);
                        return;
                    }
                };
                let change = selected_wc_change.borrow().clone();
                for edit in &edits {
                    if let Err(err) = repo.borrow_mut().edit_working_copy_file(
                        change.as_deref(),
                        &edit.path,
                        edit.content.as_deref(),
                    ) {
                        show_status(&format!("Working-copy edit failed: {err}"));
                        return;
                    }
                }
                let message = buffer_text(&message_buffer);
                let message = message.trim();
                if message.is_empty() {
                    // No message: leave the changes uncommitted, just reload the diff
                    // and rows where the user was.
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
                // A message was given: commit exactly the displayed diff — the
                // selected entry's slice — leaving every other "uncommitted changes"
                // entry untouched (changes the user reverted in the buffer but didn't
                // Split off were just dropped from the entry above, so they're gone).
                // Pass the identity only when the user overrode the prefilled git
                // default; otherwise let the engine stamp git config + a fresh "now".
                let baseline = wc_identity_baseline.borrow().clone();
                let current: [String; 4] =
                    std::array::from_fn(|i| identity_fields[i].text().to_string());
                let identity = (current != baseline).then(|| read_identity(&identity_fields));
                let outcome = repo.borrow_mut().commit_working_copy_entry(
                    change.as_deref(),
                    message,
                    identity.as_ref(),
                );
                match outcome {
                    Ok(SaveOutcome::Clean) => {
                        // The working copy is now clean (refresh hides its row) and
                        // the new commit is the tip. Drop the working-copy selection
                        // and select the tip so its just-committed message is shown,
                        // ready to refine in place.
                        selected_wc_change.borrow_mut().take();
                        selected_change.borrow_mut().take();
                        selected_changes.borrow_mut().clear();
                        refresh();
                        if let Some(row) = list.row_at_index(0) {
                            list.select_row(Some(&row));
                        }
                    }
                    Ok(SaveOutcome::Conflicts { commits }) => enter_conflict_mode(commits),
                    Err(err) => show_status(&format!("Commit failed: {err}")),
                }
                return;
            }
            // Several commits selected: the read-only batch view, whose only editable
            // part is the identity. Write each field the user changed (vs. the value
            // it was populated with) to every selected commit in one atomic
            // transaction; fields left untouched — including the "(differs)" blanks —
            // keep each commit's own value. No message/file edits in this mode.
            {
                let selected = selected_changes.borrow().clone();
                if selected.len() > 1 {
                    let baseline = multi_identity_baseline.borrow().clone();
                    let current: [String; 4] =
                        std::array::from_fn(|i| identity_fields[i].text().to_string());
                    let overrides: [Option<String>; 4] = std::array::from_fn(|i| {
                        (!current[i].trim().is_empty() && current[i] != baseline[i])
                            .then(|| current[i].clone())
                    });
                    if overrides.iter().all(Option::is_none) {
                        show_status("No identity changes to apply to the selected commits.");
                        return;
                    }
                    let edits: Vec<BatchEdit> = {
                        let cs = commits.borrow();
                        selected
                            .iter()
                            .filter_map(|ch| cs.iter().find(|c| c.change_id_hex() == *ch))
                            .map(|info| BatchEdit {
                                target: info.id.clone(),
                                message: None,
                                identity: Some(identity_for_commit(info, &overrides)),
                            })
                            .collect()
                    };
                    let outcome = repo.borrow_mut().rewrite_batch(edits);
                    match outcome {
                        Ok(SaveOutcome::Clean) => refresh(),
                        Ok(SaveOutcome::Conflicts { commits }) => enter_conflict_mode(commits),
                        Err(err) => show_status(&format!("Identity save failed: {err}")),
                    }
                    return;
                }
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
            let edits = match collect_file_edits(
                &buffer_text(&file_buffer),
                &changes.borrow(),
                &orig_changes.borrow(),
            ) {
                Ok(edits) => edits,
                Err(msg) => {
                    show_status(&msg);
                    return;
                }
            };
            if !edits.is_empty() {
                let outcome = repo.borrow_mut().rewrite_files_edits(&commit_id, &edits);
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
                let outcome = repo
                    .borrow_mut()
                    .rewrite_identity(&commit_id, &new_identity);
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
                    file_dropdown
                        .set_selected(diff_file_index_at_line(&file_buffer, top_line) as u32);
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
            let edits = match collect_file_edits(
                &buffer_text(&file_buffer),
                &changes.borrow(),
                &orig_changes.borrow(),
            ) {
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
                if let Err(err) = repo
                    .borrow_mut()
                    .split_working_copy_edits(change.as_deref(), &edits)
                {
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
            let outcome = repo.borrow_mut().split_commit_edits(&commit_id, &edits);
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

    // Search the history by commit subject. Typing matches every commit by
    // substring term (highlighting the matched characters and scrolling to the
    // first hit) without touching the selection; Enter then selects matches in turn.
    search_entry.connect_search_changed({
        let list = list.clone();
        let history_scroll = history_scroll.clone();
        let commits = commits.clone();
        let search_query = search_query.clone();
        let search_matches = search_matches.clone();
        let search_cursor = search_cursor.clone();
        move |entry| {
            let query = entry.text().to_string();
            let matches = rows::apply_search_highlight(&list, &commits.borrow(), &query);
            let first = matches.first().copied();
            *search_query.borrow_mut() = query;
            *search_matches.borrow_mut() = matches;
            // A new query restarts Enter navigation from the first match.
            search_cursor.set(None);
            if let Some(idx) = first {
                rows::scroll_row_into_view(&history_scroll, &list, idx);
            }
        }
    });
    // Enter selects the first match, then each subsequent press advances to the
    // next one, wrapping around. Focus stays in the entry so repeated Enter cycles.
    search_entry.connect_activate({
        let list = list.clone();
        let history_scroll = history_scroll.clone();
        let search_matches = search_matches.clone();
        let search_cursor = search_cursor.clone();
        let selection_sync = selection_sync.clone();
        let update_selection_pane = update_selection_pane.clone();
        move |_| {
            let idx = {
                let matches = search_matches.borrow();
                if matches.is_empty() {
                    return;
                }
                let next = match search_cursor.get() {
                    None => 0,
                    Some(c) => (c + 1) % matches.len(),
                };
                search_cursor.set(Some(next));
                matches[next]
            };
            // Drive the list selection like `refresh` does — guarded so the pane
            // router runs once at the end rather than on each `select_row`.
            selection_sync.set(true);
            list.unselect_all();
            if let Some(row) = list.row_at_index(idx as i32) {
                list.select_row(Some(&row));
            }
            selection_sync.set(false);
            update_selection_pane();
            rows::scroll_row_into_view(&history_scroll, &list, idx);
        }
    });

    // Ctrl+S triggers the same save.
    let save_shortcut = {
        let save = save.clone();
        let action = CallbackAction::new(move |_, _| {
            save();
            glib::Propagation::Stop
        });
        let trigger = ShortcutTrigger::parse_string("<Control>s").expect("valid shortcut trigger");
        Shortcut::new(Some(trigger), Some(action))
    };
    // Ctrl+Q closes the window.
    let quit_shortcut = {
        let window = window.clone();
        let action = CallbackAction::new(move |_, _| {
            window.close();
            glib::Propagation::Stop
        });
        let trigger = ShortcutTrigger::parse_string("<Control>q").expect("valid shortcut trigger");
        Shortcut::new(Some(trigger), Some(action))
    };
    // Ctrl+F focuses the header search box.
    let find_shortcut = {
        let search_entry = search_entry.clone();
        let action = CallbackAction::new(move |_, _| {
            search_entry.grab_focus();
            glib::Propagation::Stop
        });
        let trigger = ShortcutTrigger::parse_string("<Control>f").expect("valid shortcut trigger");
        Shortcut::new(Some(trigger), Some(action))
    };
    let shortcuts = ShortcutController::new();
    shortcuts.add_shortcut(save_shortcut);
    shortcuts.add_shortcut(quit_shortcut);
    shortcuts.add_shortcut(find_shortcut);
    window.add_controller(shortcuts);

    // Remember the window geometry across sessions: on close, persist the size
    // (`default_size` reports the un-maximized size to restore to), the maximized
    // state, and the two divider positions (commit-list width, message-pane
    // height). Position is deliberately not stored — GTK4/Wayland can't restore it.
    {
        let paned = paned.clone();
        let right_paned = right_paned.clone();
        let repo = repo.clone();
        window.connect_close_request(move |window| {
            let (width, height) = window.default_size();
            window_state::WindowState {
                width,
                height,
                maximized: window.is_maximized(),
                list_width: paned.position(),
                message_height: right_paned.position(),
            }
            .save();
            // Persist the session's jj index to the cache so the next launch primes
            // from it (see `commedit_engine::index_cache`). Synchronous — a GTK app
            // can't rely on `Repo`'s `Drop` running at process exit.
            repo.borrow_mut().flush_index_cache();
            glib::Propagation::Proceed
        });
    }

    // Initial population and selection.
    refresh();
    if list.selected_rows().is_empty() {
        if let Some(row) = list.row_at_index(0) {
            list.select_row(Some(&row));
        }
    }

    window.present();
}

/// Build one row for the "Edit history" dropdown: the op `label`, prefixed with a
/// `●` and bolded when it is the current state, and dimmed when it is a redo
/// target (a state ahead of the cursor — still clickable). `subtitle`, when given,
/// is shown dimmed below the label (the session-start short hash).
fn history_row(label: &str, subtitle: Option<&str>, current: bool, future: bool) -> ListBoxRow {
    let vbox = GtkBox::new(Orientation::Vertical, 0);
    vbox.set_margin_top(3);
    vbox.set_margin_bottom(3);
    vbox.set_margin_start(8);
    vbox.set_margin_end(12);

    let hbox = GtkBox::new(Orientation::Horizontal, 6);
    let marker = Label::new(Some(if current { "\u{25CF}" } else { " " }));
    marker.set_width_chars(1);
    hbox.append(&marker);
    let text = Label::new(None);
    text.set_xalign(0.0);
    if current {
        text.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(label)));
    } else {
        text.set_text(label);
    }
    hbox.append(&text);
    vbox.append(&hbox);

    if let Some(sub) = subtitle {
        let s = Label::new(Some(sub));
        s.set_xalign(0.0);
        s.add_css_class("dim-label");
        s.set_margin_start(18);
        vbox.append(&s);
    }

    let row = ListBoxRow::new();
    row.set_child(Some(&vbox));
    if future {
        row.add_css_class("dim-label");
    }
    row
}

/// Build the per-file [`FileEdit`]s a Save / Split should apply, by comparing each
/// file's *intended* new content against the commit's pristine content.
///
/// Three inputs, three roles: `combined` is the live diff buffer — authoritative
/// for manual content edits; `render` is the render baseline (`changes`) — the full
/// set including files a revert dropped from the view (which therefore have no
/// buffer section), where its `new_text` is the intended content (`None` = drop the
/// file); `orig` is the pristine load — the "before" each file diverges from. We
/// iterate `render` (not the buffer) so a reverted-away file still yields its edit.
/// A file becomes a [`FileEdit::write`] (content edited, or a removed file
/// restored), a [`FileEdit::delete`] (an added file's change reverted), or nothing.
fn collect_file_edits(
    combined: &str,
    render: &[FileChange],
    orig: &[FileChange],
) -> Result<Vec<FileEdit>, String> {
    let sections: HashMap<String, String> = split_combined_patch(combined).into_iter().collect();
    let mut edits = Vec::new();
    for r in render {
        let Some(orig_change) = orig.iter().find(|c| c.path == r.path) else {
            continue;
        };
        // Binary / conflicted-base files have no editable text on either side.
        if orig_change.is_binary || orig_change.conflicted_base {
            continue;
        }
        // The content the file should end up with. A render-baseline `new_text` of
        // `None` is a dropped file (its addition reverted, or still removed). A file
        // still shown in the buffer reconstructs from its edited patch (capturing
        // manual edits); one a revert hid from the view has no section, so its
        // reverted `new_text` is the intended content directly.
        let target: Option<String> = if r.new_text.is_none() {
            None
        } else if let Some(patch) = sections.get(&r.path) {
            let old = r.old_text.as_deref().unwrap_or("");
            let mut content = apply_patch(old, patch)
                .map_err(|err| format!("Cannot apply edited patch for {}: {err}", r.path))?;
            // Match apply_patch's trailing-newline normalization to the intended
            // content's own style so an untouched file yields no spurious edit.
            let want = r.new_text.as_deref().unwrap_or("");
            if !want.is_empty() && !want.ends_with('\n') && content.ends_with('\n') {
                content.pop();
            }
            Some(content)
        } else {
            r.new_text.clone()
        };
        if target.as_deref() != orig_change.new_text.as_deref() {
            edits.push(match target {
                Some(content) => FileEdit::write(r.path.clone(), content),
                None => FileEdit::delete(r.path.clone()),
            });
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

/// Show a standalone, GNOME-style error page in place of the editor — used when
/// the repository can't be opened. Laid out like an `AdwStatusPage` (centred
/// icon, heading, dimmed detail) but with plain GTK4 so we pull in no extra
/// dependency. `title` is the one-line heading; `detail` the explanation below it.
fn present_error(app: &Application, title: &str, detail: &str) {
    let icon = gtk::Image::from_icon_name("dialog-error-symbolic");
    icon.set_pixel_size(96);
    icon.add_css_class("error");

    let heading = Label::builder().label(title).wrap(true).build();
    heading.add_css_class("title-2");

    let body = Label::builder()
        .label(detail)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .max_width_chars(50)
        .build();
    body.add_css_class("dim-label");

    let content = GtkBox::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .margin_start(36)
        .margin_end(36)
        .margin_top(36)
        .margin_bottom(36)
        .build();
    content.append(&icon);
    content.append(&heading);
    content.append(&body);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("commedit")
        .default_width(460)
        .default_height(340)
        .child(&content)
        .build();
    window.set_titlebar(Some(&HeaderBar::new()));
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

    fn removed(path: &str, old: &str) -> FileChange {
        FileChange {
            path: path.to_string(),
            kind: ChangeKind::Removed,
            old_text: Some(old.to_string()),
            new_text: None,
            is_binary: false,
            conflicted_base: false,
        }
    }

    /// Buffer line index of the first `@@` header in a built diff.
    fn first_hunk_line(text: &str) -> usize {
        text.split('\n').position(|l| l.starts_with("@@")).unwrap()
    }

    #[test]
    fn diff_cue_cells_mark_expand_and_revert_on_one_header() {
        // A 12-line file with one mid edit leaves hidden context both ways: the @@
        // header gets both an expand cell and a revert cell, and the click target
        // resolves to that file's hunk.
        let old: String = (1..=12).map(|n| format!("l{n}\n")).collect();
        let new = old.replace("l6\n", "L6\n");
        let changes = vec![modified("f", &old, &new)];
        let (text, _h, files) = build_diff_buffer_text(&changes, &HashMap::new());
        let (expand, revert) = diff_cues::diff_cue_cells(&text, &files, &changes, false);
        let li = first_hunk_line(&text);
        assert!(expand[li].is_some(), "expandable header");
        assert!(revert[li].is_some(), "revertable hunk");
        assert!(
            matches!(diff_cues::hunk_target(&text, &files, li), Some((_, _, ref p)) if p == "f")
        );
    }

    #[test]
    fn diff_cue_cells_revert_without_expand_on_a_full_hunk() {
        // A 3-line file with one change has no hidden context: no expand cell, but
        // still a revert cell, on the @@ header.
        let changes = vec![modified("f", "a\nb\nc\n", "a\nB\nc\n")];
        let (text, _h, files) = build_diff_buffer_text(&changes, &HashMap::new());
        let (expand, revert) = diff_cues::diff_cue_cells(&text, &files, &changes, false);
        let li = first_hunk_line(&text);
        assert!(expand[li].is_none(), "no hidden context");
        assert!(revert[li].is_some());
    }

    #[test]
    fn diff_cue_cells_revert_file_rides_the_diff_git_line() {
        let changes = vec![modified("f", "a\nb\n", "a\nB\n")];
        let (text, _h, files) = build_diff_buffer_text(&changes, &HashMap::new());
        let (_expand, revert) = diff_cues::diff_cue_cells(&text, &files, &changes, false);
        let sep = files[0].start_line;
        assert!(text
            .split('\n')
            .nth(sep)
            .unwrap()
            .starts_with("diff --git "));
        assert!(revert[sep].is_some(), "revert-file cell on the separator");
        assert_eq!(
            diff_cues::file_target(&text, &files, sep).as_deref(),
            Some("f")
        );
    }

    #[test]
    fn diff_cue_cells_read_only_suppresses_reverts() {
        let changes = vec![modified("f", "a\nb\n", "a\nB\n")];
        let (text, _h, files) = build_diff_buffer_text(&changes, &HashMap::new());
        let (_e, revert) = diff_cues::diff_cue_cells(&text, &files, &changes, true);
        assert!(
            revert.iter().all(Option::is_none),
            "no revert cues when read-only"
        );
    }

    #[test]
    fn added_file_gets_a_file_revert_cue_but_no_hunk_revert_cue() {
        // An added file's whole change can be dropped (revert file -> delete), but
        // a *hunk* revert needs an old side, so it's not offered.
        let changes = vec![added("new.txt", "x\ny\n")];
        let (text, _h, files) = build_diff_buffer_text(&changes, &HashMap::new());
        let (_e, revert) = diff_cues::diff_cue_cells(&text, &files, &changes, false);
        let sep = files[0].start_line;
        assert!(revert[sep].is_some(), "revert-file on the separator");
        for (i, l) in text.split('\n').enumerate() {
            if l.starts_with("@@") {
                assert!(revert[i].is_none(), "no hunk revert for an added file");
            }
        }
    }

    #[test]
    fn removed_file_gets_a_file_revert_cue() {
        // A removed file has no hunks (it renders as a notice) but still a change
        // to undo, so the revert-file cell rides its `diff --git` separator.
        let changes = vec![removed("gone.txt", "a\nb\n")];
        let (text, _h, files) = build_diff_buffer_text(&changes, &HashMap::new());
        let (_e, revert) = diff_cues::diff_cue_cells(&text, &files, &changes, false);
        let sep = files[0].start_line;
        assert!(revert[sep].is_some());
        assert_eq!(
            diff_cues::file_target(&text, &files, sep).as_deref(),
            Some("gone.txt")
        );
    }

    /// Apply a `RevertFile` to `change` the way the click handler does (the render
    /// baseline's `new_text` drops to the old side), then render and collect the
    /// edits a Save would apply against the pristine `orig`.
    fn revert_file_and_collect(orig: &[FileChange]) -> Vec<FileEdit> {
        let mut render = orig.to_vec();
        for c in render.iter_mut() {
            c.new_text = c.old_text.clone();
        }
        let (text, _h, _f) = build_diff_buffer_text(&render, &HashMap::new());
        collect_file_edits(&text, &render, orig).expect("collect")
    }

    #[test]
    fn reverting_an_added_file_collects_a_delete() {
        let edits = revert_file_and_collect(&[added("new.txt", "x\ny\n")]);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, "new.txt");
        assert!(
            edits[0].content.is_none(),
            "an added file's revert deletes it"
        );
    }

    #[test]
    fn reverting_a_removed_file_collects_a_recreate() {
        let edits = revert_file_and_collect(&[removed("gone.txt", "a\nb\n")]);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, "gone.txt");
        assert_eq!(edits[0].content.as_deref(), Some("a\nb\n"));
    }

    #[test]
    fn reverting_a_modified_file_collects_the_old_content() {
        let edits = revert_file_and_collect(&[modified("f", "a\nb\n", "a\nB\n")]);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].content.as_deref(), Some("a\nb\n"));
    }

    #[test]
    fn an_untouched_diff_collects_no_edits() {
        // Render the pristine diff and collect against the same baseline: nothing
        // diverges, so a plain reselect (no edits) saves nothing.
        let orig = vec![
            modified("f", "a\nb\n", "a\nB\n"),
            added("new.txt", "x\n"),
            removed("gone.txt", "z\n"),
        ];
        let (text, _h, _f) = build_diff_buffer_text(&orig, &HashMap::new());
        let edits = collect_file_edits(&text, &orig, &orig).expect("collect");
        assert!(edits.is_empty(), "got {edits:?}");
    }

    #[test]
    fn a_reverted_file_dropped_from_the_view_still_collects_its_edit() {
        // Revert the modified file: it has no net change left, so visible_changes
        // drops it from the buffer (no section). collect_file_edits iterates the
        // full render baseline, so it still emits the revert from `new_text`.
        let orig = vec![modified("f", "a\nb\n", "a\nB\n"), added("n.txt", "x\n")];
        let mut render = orig.clone();
        render[0].new_text = render[0].old_text.clone(); // revert f

        let vis = visible_changes(&render, &orig);
        assert!(
            vis.iter().all(|c| c.path != "f"),
            "f is hidden from the view"
        );
        let (text, _h, _f) = build_diff_buffer_text(&vis, &HashMap::new());
        assert!(!text.contains("a/f b/f"), "f has no buffer section");

        let edits = collect_file_edits(&text, &render, &orig).expect("collect");
        let f = edits
            .iter()
            .find(|e| e.path == "f")
            .expect("f edit present");
        assert_eq!(f.content.as_deref(), Some("a\nb\n"));
    }

    #[test]
    fn visible_changes_keeps_a_mode_only_no_change_file() {
        // A file that already had no textual change at load (e.g. a mode-only
        // change) isn't a revert, so it stays visible.
        let orig = vec![modified("m", "same\n", "same\n")];
        let vis = visible_changes(&orig, &orig);
        assert_eq!(vis.len(), 1);
    }
}
