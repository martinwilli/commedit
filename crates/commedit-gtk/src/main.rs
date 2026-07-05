//! commedit GTK4 UI (Milestone 2): browse history, edit a commit message, and
//! edit the content of files a commit changes. Saving transparently rewrites the
//! commit and rebases descendants via the engine.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;

use commedit_engine::blame::FileBlame;
use commedit_engine::conflict::SaveOutcome;
use commedit_engine::diff::{
    apply_patch, combined_changes, commit_changes, reconstruct_conflict_file, render_commit_diff,
    render_conflict_snippets, revert_groups, split_combined_patch, CombinedFile, ContextExpansion,
    FileChange, HunkInfo,
};
use commedit_engine::graph::{compute_graph, GraphLayout};
use commedit_engine::history::{history, history_limited, CommitInfo};
use commedit_engine::patch_edit::{
    collapse_diff, deletion_is_safe, move_block_range, plan_edit, strip_selection_prefixes, Cursor,
    EditGesture, EditPlan, Selection,
};
use commedit_engine::repo::Repo;
use commedit_engine::rewrite::{BatchEdit, Identity};
use commedit_engine::tabwidth::{TabWidthResolver, DEFAULT_TAB_WIDTH};
use commedit_engine::tree::FileEdit;
use commedit_engine::workcopy::WcTarget;
use commedit_engine::CommitId;
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    gdk, Application, ApplicationWindow, Box as GtkBox, Button, CallbackAction, CheckButton,
    DropDown, Entry, EventControllerKey, EventControllerScroll, EventControllerScrollFlags, Grid,
    HeaderBar, Label, ListBox, ListBoxRow, MenuButton, Orientation, Paned, PolicyType, Popover,
    PropagationPhase, ScrolledWindow, SearchEntry, Shortcut, ShortcutController, ShortcutTrigger,
    Stack, StringList, ToggleButton,
};
use sourceview5::prelude::ViewExt;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

mod state;
use crate::state::*;
mod blame_col;
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
mod lanebranch;
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
    // The pure list of real commits (newest first). The reorder/squash/drop
    // planners and the MCP path see it byte-identically — it never holds the
    // working-copy `@` rows, which live only in `display` below.
    let commits: Rc<RefCell<Vec<CommitInfo>>> = Rc::new(RefCell::new(Vec::new()));
    // The *planning* ancestry-graph lane layout over `commits` (no `@` nodes),
    // recomputed whenever `commits` is reloaded; the planners read it.
    let graph: Rc<RefCell<GraphLayout>> = Rc::new(RefCell::new(GraphLayout::default()));
    // The interleaved rows actually drawn: real commits with each worktree's
    // uncommitted `@` spliced in above its tip (as a hollow lane node). The list's
    // rows mirror this 1:1, so a *list* index is a `display` index — translate it
    // to a commit index with `row_commit_index` / `row_commit_gap`.
    let display: Rc<RefCell<Vec<DisplayRow>>> = Rc::new(RefCell::new(Vec::new()));
    // The *rendering* graph over `display` (including the `@` nodes), fed only to
    // the row drawing areas; `hollow[i]` flags display row `i`'s node as a ring.
    let draw_graph: Rc<RefCell<GraphLayout>> = Rc::new(RefCell::new(GraphLayout::default()));
    let hollow: SharedHollow = Rc::new(RefCell::new(Vec::new()));
    // Commit index → display-row index (the reverse of `row_commit_index`), so a
    // commit can be re-selected / highlighted by index after `@` nodes shift it.
    let commit_rows: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
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
    // The git-default identity prefilled into the fields when a working-copy `@`
    // node is selected (see `update_selection_pane`); the working-copy commit save
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
    // Diff-blame state (the opt-in gutter column, `blame_col`). `blame_on` is the
    // toggle; `blame_selection` is the commit id(s) whose old side is being
    // blamed (oldest-first, as `blame_old_side` wants); `blame_data` caches the
    // computed per-file blame for the current selection (None = off / not yet
    // computed). The expensive walk runs only while toggled on.
    let blame_on: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let blame_selection: Rc<RefCell<Vec<CommitId>>> = Rc::new(RefCell::new(Vec::new()));
    let blame_data: Rc<RefCell<Option<HashMap<String, FileBlame>>>> = Rc::new(RefCell::new(None));
    // Guards the dropdown↔scroll feedback loop: set while one side programmatically
    // drives the other so the reaction doesn't bounce back.
    let nav_sync: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // One-shot: set around the `refresh()` a content-only Save triggers, so the diff
    // reload *splices* the new text into the buffer (keeping the SourceView's scroll
    // anchor) instead of `set_text`-ing it. `set_text` resets the view's internal
    // first-para mark to the top, and the adjustment is only a mirror of that mark —
    // GTK re-derives the scroll value from it on the next validation, so poking the
    // adjustment can't hold. Splicing leaves the mark untouched (see `apply_changes`).
    let splice_reload: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // Per-file hunk context expansion, keyed by path. Reset when the selected
    // commit changes (see `load_changes`).
    let expansions: Rc<RefCell<HashMap<String, ContextExpansion>>> =
        Rc::new(RefCell::new(HashMap::new()));
    // Commits dropped to the trash this session, newest drop last. They are no
    // longer on the branch but their objects survive, so they can be dragged back
    // into history to restore them (see `Repo::restore_commit`).
    let trashed: Rc<RefCell<Vec<CommitInfo>>> = Rc::new(RefCell::new(Vec::new()));
    // Each trashed commit's origin branch (change-id hex → short-name), recorded at
    // drop time so "restore to working tree" routes back to that branch's worktree.
    let trashed_origin: Rc<RefCell<HashMap<String, String>>> =
        Rc::new(RefCell::new(HashMap::new()));
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
    // The diff hunk grabbed by its `@@` line and dragged from the diff view, or
    // None between hunk drags. Carried out-of-band (the payload is only an i32
    // sentinel); read by the history list's drop handler (`DragOrigin::Hunk`).
    let drag_hunk: Rc<RefCell<Option<HunkDrag>>> = Rc::new(RefCell::new(None));
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
             border-radius: 5px; outline: 1px dashed rgb(46, 194, 126); \
             outline-offset: -1px; } \
             row.squash-sibling { background-color: rgba(245, 194, 17, 0.18); \
             border-radius: 5px; outline: 1px dashed rgb(245, 194, 17); \
             outline-offset: -1px; } \
             row.squash-drop-target { background-color: rgba(224, 27, 36, 0.38); \
             border-radius: 5px; outline: 1px solid rgb(224, 27, 36); \
             outline-offset: -1px; } \
             row.op-affected { background-color: rgba(53, 132, 228, 0.22); \
             border-radius: 5px; outline: 1px dashed rgb(53, 132, 228); \
             outline-offset: -1px; } \
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
             border-radius: 5px; outline: 1px dashed rgb(145, 65, 172); \
             outline-offset: -1px; } \
             .blame-strip { min-width: 14px; padding: 0; color: #6e7781; \
             border-right: 1px solid alpha(@theme_fg_color, 0.12); } \
             .blame-strip:hover { background-color: alpha(@theme_fg_color, 0.08); }",
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
    // The working-copy `@` rows now live *in* the history `list` as hollow lane
    // nodes (one per worktree, spliced above its tip — see `display`), so there is
    // no separate working-copy list. The selection these two cells track is the
    // currently-shown `@`: `selected_wc_change` is its stable change id (the diff
    // pane follows it), `selected_wc_branch` the worktree's branch short-name
    // (which `@` to edit/commit/fold/discard, via `Repo::wc_target_for_branch`).
    let selected_wc_change: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let selected_wc_branch: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

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
    // The file gutter: the optional blame column, then the two number columns,
    // old | new. Each number column draws *either* a line number
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
    // The optional blame column sits at the far left, *before* the line numbers
    // (`blame_col`) — the IDE "annotate" convention. It carries no content (zero
    // width — collapsed) until the user expands it via the chevron toggle; then
    // `refresh_blame_column` repopulates it from `blame_data`.
    let col_blame = blame_col::BlameColumn::new();
    line_gutter.insert(&col_blame, 0);
    line_gutter.insert(&col_old, 1);
    line_gutter.insert(&col_new, 2);
    // Hovering a blame cell highlights that commit's row in the history list, if
    // it is shown (`highlight_affected` no-ops for a change id not in the list, so
    // an origin older than the visible range simply doesn't light up). Leaving a
    // cell clears it.
    col_blame.set_on_hover({
        let list = list.clone();
        let commits = commits.clone();
        let commit_rows = commit_rows.clone();
        Rc::new(move |change: Option<&str>| {
            clear_highlight(&list);
            if let Some(change) = change {
                highlight_affected(
                    &list,
                    &commit_rows.borrow(),
                    &commits.borrow(),
                    &[change.to_string()],
                );
            }
        })
    });

    // Recompute the blame for `blame_selection` into `blame_data` — but only while
    // blame is toggled on (it's the expensive jj-lib walk). Called when the
    // selection changes (before the diff text is rendered) and when the toggle
    // flips on; clears the cache otherwise.
    let recompute_blame: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let blame_on = blame_on.clone();
        let blame_selection = blame_selection.clone();
        let blame_data = blame_data.clone();
        Rc::new(move || {
            if !blame_on.get() {
                *blame_data.borrow_mut() = None;
                return;
            }
            let ids = blame_selection.borrow().clone();
            if ids.is_empty() {
                *blame_data.borrow_mut() = None;
                return;
            }
            let blamed = repo.borrow().blame_old_side(&ids);
            *blame_data.borrow_mut() = blamed
                .ok()
                .map(|files| files.into_iter().map(|fb| (fb.path.clone(), fb)).collect());
        })
    };
    // Repaint the blame column from the live buffer text + the cached `blame_data`
    // (cheap: a per-line remap). Empty content collapses the column when blame is
    // off or uncomputed. Shared by the buffer `changed` handler and the toggle.
    let refresh_blame_column: Rc<dyn Fn()> = {
        let file_buffer = file_buffer.clone();
        let combined_files = combined_files.clone();
        let blame_data = blame_data.clone();
        let blame_on = blame_on.clone();
        let col_blame = col_blame.clone();
        Rc::new(move || {
            if !blame_on.get() {
                col_blame.set_content(&[]);
                return;
            }
            let data = blame_data.borrow();
            let Some(data) = data.as_ref() else {
                col_blame.set_content(&[]);
                return;
            };
            let text = buffer_text(&file_buffer);
            let nums = linenums::diff_line_numbers(&text);
            let cells = blame_col::blame_cells(&text, &combined_files.borrow(), &nums, data);
            col_blame.set_content(&cells);
        })
    };

    file_buffer.connect_changed({
        let pane_mode = pane_mode.clone();
        let col_old = col_old.clone();
        let col_new = col_new.clone();
        let col_blame = col_blame.clone();
        let combined_files = combined_files.clone();
        let conflict_view = conflict_view.clone();
        let changes = changes.clone();
        let diff_read_only = diff_read_only.clone();
        let refresh_blame_column = refresh_blame_column.clone();
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
                col_blame.set_content(&[]); // no blame while resolving conflicts
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
                refresh_blame_column();
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
    // The blame "sidebar": a thin, always-visible, full-height handle pinned at
    // the very left of the editor — left of the gutter, hence left of the blame
    // hashes it reveals. It carries a vertically-centred triangle (`▸` collapsed,
    // `◂` expanded) and toggles the blame column. A *view* option, kept out of the
    // Save/Split mutations in the bottom bar; collapsed by default, expanding is
    // opt-in because blame is the one expensive view (it recomputes the selection's
    // blame and widens the otherwise zero-width gutter column). `blame_on` reads as
    // "expanded". It is a *sibling* of the scroll, not a gutter renderer, so the
    // triangle stays pinned and centred instead of scrolling with the text.
    let blame_arrow = Label::new(Some("▸"));
    blame_arrow.set_valign(gtk::Align::Center);
    blame_arrow.set_vexpand(true);
    let blame_strip = ToggleButton::new();
    blame_strip.set_child(Some(&blame_arrow));
    blame_strip.add_css_class("flat");
    blame_strip.add_css_class("blame-strip");
    blame_strip.set_vexpand(true);
    blame_strip.set_tooltip_text(Some(
        "Blame: annotate context and removed lines with the commit that last touched them",
    ));
    blame_strip.connect_toggled({
        let blame_on = blame_on.clone();
        let recompute_blame = recompute_blame.clone();
        let refresh_blame_column = refresh_blame_column.clone();
        let blame_arrow = blame_arrow.clone();
        move |btn| {
            let on = btn.is_active();
            // The triangle points the way the panel moves: `▸` opens it rightward,
            // `◂` collapses it back to the strip.
            blame_arrow.set_text(if on { "◂" } else { "▸" });
            blame_on.set(on);
            recompute_blame();
            refresh_blame_column();
        }
    });
    // The horizontal Paned's ~9px resize handle overlaps the left edge of this
    // pane; its drag gesture (capture phase on the Paned ancestor) claims presses
    // over that band before they reach the strip. Inset the strip — now the
    // leftmost interactive element — clear of the handle band; the editor scroll
    // follows it, so it no longer needs its own inset.
    blame_strip.set_margin_start(12);
    let editor_row = GtkBox::new(Orientation::Horizontal, 0);
    editor_row.append(&blame_strip);
    editor_row.append(&file_scroll);
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
    files_box.append(&editor_row);
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
    // A branch dropdown between Reload and Search: it controls the *editable set* —
    // tick a local branch to fold it into the unified DAG as a real, editable lane,
    // untick it to freeze it again. Defaults to just the opened branch; the last
    // editable branch can't be unticked. The checkbox list is (re)populated each
    // time the popover opens and wired to `set_editable_branches` + `refresh`
    // further below, once they exist.
    let branch_menu = MenuButton::new();
    branch_menu.set_icon_name("view-list-symbolic");
    branch_menu.set_tooltip_text(Some(
        "Choose which branches are editable — tick to add a branch to the unified DAG",
    ));
    let branch_list = ListBox::new();
    branch_list.set_selection_mode(gtk::SelectionMode::None);
    // Wrap the list in a scrolled window: a repo can have dozens of branches, and
    // a GTK4 popover does not scroll its own child, so a bare list grows taller
    // than the monitor and the compositor silently fails to place the popover (it
    // never appears). Cap the height and let it scroll; propagate the natural
    // width so the popover is still only as wide as the longest branch name.
    let branch_scroll = ScrolledWindow::new();
    branch_scroll.set_child(Some(&branch_list));
    branch_scroll.set_policy(PolicyType::Never, PolicyType::Automatic);
    branch_scroll.set_propagate_natural_width(true);
    branch_scroll.set_propagate_natural_height(true);
    branch_scroll.set_max_content_height(400);
    let branch_popover = Popover::new();
    branch_popover.set_child(Some(&branch_scroll));
    branch_menu.set_popover(Some(&branch_popover));
    header.pack_start(&reload_button);
    header.pack_start(&branch_menu);
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
        display: display.clone(),
        draw_graph: draw_graph.clone(),
        hollow: hollow.clone(),
        commit_rows: commit_rows.clone(),
        trashed: trashed.clone(),
        trashed_origin: trashed_origin.clone(),
        pending_trash_op: pending_trash_op.clone(),
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
        drag_hunk: drag_hunk.clone(),
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
            // Capture the line height while the current layout is still valid; the
            // post-render scroll clamp below needs it to recompute `upper`.
            let line_height = file_view.iter_location(&file_buffer.start_iter()).height() as f64;
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
            // A revert shrinks the diff above/around the viewport, leaving the view
            // scrolled past the now-shorter content; GTK validates the onscreen range
            // lazily on the next frame and aborts when first-para sits beyond the
            // buffer end (gtk_text_view_validate_onscreen). Clamp the scroll into the
            // new bounds now — recomputing `upper` arithmetically, since splice leaves
            // GTK's adjustment stale until that same deferred pass (mirrors the save
            // reload's re-pin). Expansion only grows, so this is a no-op there.
            if let Some(vadj) = file_view.vadjustment() {
                let page = vadj.page_size();
                if line_height > 0.0 && page > 0.0 {
                    let top = file_view.top_margin() as f64;
                    let bottom = file_view.bottom_margin() as f64;
                    let height = file_buffer.line_count() as f64 * line_height + top + bottom;
                    let upper = height.max(page);
                    vadj.set_upper(upper);
                    if vadj.value() > upper - page {
                        vadj.set_value((upper - page).max(0.0));
                    }
                }
            }
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

    // The diff/conflict cue *buttons* all live in the gutter (`GutterColumn`), which
    // owns their click handling, cursor and tooltips. The one in-text affordance is
    // the `@@` header line as a drag handle: grab it to relocate the whole hunk into
    // another commit (or the working copy). The gesture is a `DragSource` in the
    // Capture phase so it gets first look at a press on the view — on a `@@` line it
    // offers the hunk (a companion press gesture claims the sequence so selection
    // can't steal it, see below); anywhere else `connect_prepare` returns None and
    // the press falls through to GtkTextView's own selection drag.
    // Is a widget-relative x over the left gutter (line numbers + cue buttons)
    // rather than the text? These Capture-phase handlers claim a press before the
    // gutter can, so without this they'd swallow the gutter's own expand /
    // revert-hunk button clicks — which sit on the very same `@@` lines.
    let over_gutter: Rc<dyn Fn(f64) -> bool> = {
        let file_view = file_view.clone();
        Rc::new(move |x: f64| {
            let gutter =
                sourceview5::prelude::ViewExt::gutter(&file_view, gtk::TextWindowType::Left);
            (x as i32) < gutter.width()
        })
    };

    let hunk_drag_source = gtk::DragSource::new();
    hunk_drag_source.set_actions(gdk::DragAction::MOVE);
    hunk_drag_source.set_propagation_phase(PropagationPhase::Capture);
    hunk_drag_source.connect_prepare({
        let file_view = file_view.clone();
        let file_buffer = file_buffer.clone();
        let combined_files = combined_files.clone();
        let diff_read_only = diff_read_only.clone();
        let pane_mode = pane_mode.clone();
        let viewing_wc = viewing_wc.clone();
        let selected_change = selected_change.clone();
        let selected_wc_branch = selected_wc_branch.clone();
        let selected_wc_change = selected_wc_change.clone();
        let drag_origin = drag_origin.clone();
        let drag_hunk = drag_hunk.clone();
        let over_gutter = over_gutter.clone();
        move |_source, x, y| {
            // Offered only in the editable single-commit / single-`@` diff. The
            // multi-commit combined view is read-only and its source is ambiguous,
            // and a conflict snippet buffer isn't a unified diff.
            if diff_read_only.get() || pane_mode.borrow().is_conflict() || over_gutter(x) {
                return None;
            }
            let (_bx, by) =
                file_view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
            let (iter, _) = file_view.line_at_y(by);
            let line = iter.line() as usize;
            // Not a `@@` header → return None so normal text selection proceeds.
            let (first_group, last_group, path) =
                diff_cues::hunk_target(&buffer_text(&file_buffer), &combined_files.borrow(), line)?;
            let source = if viewing_wc.get() {
                HunkSource::WorkingCopy {
                    branch: selected_wc_branch.borrow().clone().unwrap_or_default(),
                    change: selected_wc_change.borrow().clone().unwrap_or_default(),
                }
            } else {
                HunkSource::Commit(selected_change.borrow().clone()?)
            };
            *drag_hunk.borrow_mut() = Some(HunkDrag {
                source,
                path,
                first_group,
                last_group,
            });
            drag_origin.set(DragOrigin::Hunk);
            // In-process only: the drop handler reads the hunk from `drag_hunk`, not
            // this value. An i32 sentinel keeps the history list's DropTarget
            // (String | i32) willing to receive the drop.
            Some(gdk::ContentProvider::for_value(&(-1i32).to_value()))
        }
    });
    hunk_drag_source.connect_drag_end({
        let post_drag = post_drag.clone();
        move |_source, _drag, _delete| {
            // The drop staged its rewrite into `post_drag`; run it now that the
            // gesture (and GTK's DnD bookkeeping) is fully torn down — same discipline
            // as the history/trash drag sources.
            dragdrop::run_post_drag(&post_drag);
        }
    });

    // A press on a `@@` line must not let GtkTextView start extending a text
    // selection: a jittery press-and-move would otherwise claim the event sequence
    // for selection before the drag crosses its motion threshold (drags then only
    // start if the pointer holds still first). So claim the sequence here in the
    // Capture phase, denying the Bubble-phase selection gesture — and group this with
    // the drag source so the claim doesn't also deny our own drag.
    let hunk_press = gtk::GestureClick::new();
    hunk_press.set_button(gdk::BUTTON_PRIMARY);
    hunk_press.set_propagation_phase(PropagationPhase::Capture);
    hunk_press.connect_pressed({
        let file_view = file_view.clone();
        let file_buffer = file_buffer.clone();
        let combined_files = combined_files.clone();
        let diff_read_only = diff_read_only.clone();
        let pane_mode = pane_mode.clone();
        let over_gutter = over_gutter.clone();
        move |gesture, _n, x, y| {
            if diff_read_only.get() || pane_mode.borrow().is_conflict() || over_gutter(x) {
                return;
            }
            let (_bx, by) =
                file_view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
            let (iter, _) = file_view.line_at_y(by);
            if diff_cues::hunk_target(
                &buffer_text(&file_buffer),
                &combined_files.borrow(),
                iter.line() as usize,
            )
            .is_some()
            {
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
        }
    });
    file_view.add_controller(hunk_press.clone());
    file_view.add_controller(hunk_drag_source.clone());
    hunk_drag_source.group_with(&hunk_press);

    // Hover feedback for the drag handle: a grab cursor plus a subtle line highlight
    // whenever the pointer sits over a `@@` header, and only there. Tracks the last
    // highlighted line so the tag/cursor only change on a line change.
    let hunk_hover_line: Rc<Cell<i32>> = Rc::new(Cell::new(-1));
    let hunk_motion = gtk::EventControllerMotion::new();
    hunk_motion.connect_motion({
        let file_view = file_view.clone();
        let file_buffer = file_buffer.clone();
        let combined_files = combined_files.clone();
        let diff_read_only = diff_read_only.clone();
        let pane_mode = pane_mode.clone();
        let hunk_hover_line = hunk_hover_line.clone();
        let over_gutter = over_gutter.clone();
        move |_controller, x, y| {
            let over = if diff_read_only.get() || pane_mode.borrow().is_conflict() || over_gutter(x)
            {
                None
            } else {
                let (_bx, by) = file_view.window_to_buffer_coords(
                    gtk::TextWindowType::Widget,
                    x as i32,
                    y as i32,
                );
                let (iter, _) = file_view.line_at_y(by);
                let line = iter.line();
                diff_cues::hunk_target(
                    &buffer_text(&file_buffer),
                    &combined_files.borrow(),
                    line as usize,
                )
                .map(|_| line)
            };
            let now = over.unwrap_or(-1);
            if now == hunk_hover_line.get() {
                return;
            }
            hunk_hover_line.set(now);
            set_hunk_hover(&file_buffer, over);
            if over.is_some() {
                file_view.set_cursor_from_name(Some("grab"));
            } else {
                // Off a `@@` line: restore the text I-beam (what GtkTextView shows by
                // default) rather than clearing to the inherited arrow.
                file_view.set_cursor_from_name(Some("text"));
            }
        }
    });
    hunk_motion.connect_leave({
        let file_view = file_view.clone();
        let file_buffer = file_buffer.clone();
        let hunk_hover_line = hunk_hover_line.clone();
        move |_controller| {
            hunk_hover_line.set(-1);
            set_hunk_hover(&file_buffer, None);
            file_view.set_cursor(None);
        }
    });
    file_view.add_controller(hunk_motion);

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
        let viewing_wc = viewing_wc.clone();
        let selected_wc_branch = selected_wc_branch.clone();
        let repo = repo.clone();
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
            // Split peels any editable worktree's `@` chain — the launch one or a
            // sibling's. Disable it only for a branch with no worktree (no `@` to
            // split); the engine refuses that too.
            let splittable = if viewing_wc.get() {
                selected_wc_branch
                    .borrow()
                    .as_deref()
                    .and_then(|b| repo.borrow().wc_target_for_branch(b))
                    .is_some()
            } else {
                true
            };
            split_button.set_sensitive(has_edits && splittable);
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
            // Alt+Up / Alt+Down move the caret's line — or, with a selection, the
            // covered block of `+` lines — over its neighbour, *including* over
            // context and `-` lines, as a structured reorder via the planner.
            // SourceView binds its own move-lines here, but its raw delete+insert
            // is re-planned mid-flight by the firewall (doubling prefixes and
            // invalidating its iterators); the capture-phase `Stop` pre-empts it so
            // only our planned edit runs.
            let alt = state.contains(gdk::ModifierType::ALT_MASK);
            if alt
                && !ctrl
                && !shift
                && matches!(
                    keyval,
                    gdk::Key::Up | gdk::Key::Down | gdk::Key::KP_Up | gdk::Key::KP_Down
                )
            {
                let down = matches!(keyval, gdk::Key::Down | gdk::Key::KP_Down);
                let sel = buffer_selection(&file_buffer);
                let had_selection = file_buffer.selection_bounds().is_some();
                return match plan_edit(
                    &buffer_text(&file_buffer),
                    sel,
                    EditGesture::MoveLine { down },
                ) {
                    EditPlan::Block => {
                        show_status(MOVE_LINE_HINT);
                        glib::Propagation::Stop
                    }
                    EditPlan::Edit(edit) => {
                        apply_patch_edit(&file_buffer, &editing, &edit, &*highlight);
                        // Re-select the moved block so a repeat press moves the same
                        // lines (applying the edit collapsed the selection to a caret).
                        if had_selection {
                            let (a, b) = move_block_range(sel);
                            let (na, nb) = if down { (a + 1, b + 1) } else { (a - 1, b - 1) };
                            if let (Some(start), Some(mut end)) = (
                                file_buffer.iter_at_line(na as i32),
                                file_buffer.iter_at_line(nb as i32),
                            ) {
                                if !end.ends_line() {
                                    end.forward_to_line_end();
                                }
                                file_buffer.select_range(&start, &end);
                            }
                        }
                        glib::Propagation::Stop
                    }
                    EditPlan::Allow => glib::Propagation::Proceed,
                };
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
        let rerender_diff_spliced = rerender_diff_spliced.clone();
        let scroll_to_file = scroll_to_file.clone();
        let splice_reload = splice_reload.clone();
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
            // Render the whole change once; the dropdown is now a jump aid. A
            // content Save reloads with `splice_reload` set: splice the new text in
            // (keeping the scroll anchor) rather than `set_text` (which resets it to
            // the top). The caller also holds `nav_sync`, so neither branch's
            // dropdown change bounces back into a scroll.
            let splice = splice_reload.get();
            if splice {
                rerender_diff_spliced();
            } else {
                render_diff_view();
            }
            let labels: Vec<String> = changes.borrow().iter().map(change_label).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let prev = file_dropdown.selected();
            file_dropdown.set_model(Some(&StringList::new(&refs)));
            if splice {
                // The splice kept the viewport where it was, so keep the dropdown on
                // the same file instead of jumping to the first — and no scroll.
                let n = refs.len() as u32;
                let target = if prev == gtk::INVALID_LIST_POSITION || n == 0 {
                    0
                } else {
                    prev.min(n - 1)
                };
                file_dropdown.set_selected(target);
            } else {
                file_dropdown.set_selected(0);
                // Land at the first file's top (and set current_file) even if
                // set_selected(0) didn't fire a change notification.
                scroll_to_file(0);
            }
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
        let selected_wc_branch = selected_wc_branch.clone();
        Rc::new(move || {
            let loaded = {
                let r = repo.borrow();
                // The `@` entries of the selected worktree (launch chain or a
                // sibling's single `@`), keyed by branch short-name. Resolve the
                // viewed entry by its stable change id, falling back to that
                // worktree's newest `@`.
                let want_branch = selected_wc_branch.borrow().clone();
                let entries = r
                    .worktree_uncommitted()
                    .into_iter()
                    .find(|(b, _)| Some(b.as_str()) == want_branch.as_deref())
                    .map(|(_, e)| e)
                    .unwrap_or_default();
                let want = selected_wc_change.borrow().clone();
                let entry = want
                    .and_then(|ch| entries.iter().find(|e| e.info.change_id_hex() == ch))
                    .or_else(|| entries.first());
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
        let display = display.clone();
        let message_buffer = message_buffer.clone();
        let message_view = message_view.clone();
        let selected_change = selected_change.clone();
        let selected_changes = selected_changes.clone();
        let selection_sync = selection_sync.clone();
        let load_changes = load_changes.clone();
        let load_wc_changes = load_wc_changes.clone();
        let load_conflict_files = load_conflict_files.clone();
        let pane_mode = pane_mode.clone();
        let identity_fields = identity_fields.clone();
        let original_identity = original_identity.clone();
        let viewing_wc = viewing_wc.clone();
        let selected_wc_change = selected_wc_change.clone();
        let selected_wc_branch = selected_wc_branch.clone();
        let wc_identity_baseline = wc_identity_baseline.clone();
        let repo = repo.clone();
        let apply_changes = apply_changes.clone();
        let file_buffer = file_buffer.clone();
        let editing = editing.clone();
        let diff_read_only = diff_read_only.clone();
        let multi_identity_baseline = multi_identity_baseline.clone();
        let update_save_sensitivity = update_save_sensitivity.clone();
        let save_button = save_button.clone();
        let blame_selection = blame_selection.clone();
        let recompute_blame = recompute_blame.clone();
        Rc::new(move || {
            if selection_sync.get() {
                return;
            }
            // Point the blame at this selection (cleared here, set in the
            // commit-diff arms below) and recompute it *before* the diff text
            // lands, so the buffer `changed` handler paints the column from a
            // current cache. `recompute_blame` is a no-op while blame is toggled
            // off. The closure runs again per arm that shows a real-commit diff.
            *blame_selection.borrow_mut() = Vec::new();
            // Split the selected rows into real commits and working-copy `@` rows
            // (both now live in the one list). `select_click` keeps `@` rows out of
            // multi-selections, so at most one `@` is ever selected; if one is, it
            // wins (mutually exclusive with a history selection, as the old separate
            // working-copy list was).
            let (commit_infos, wc): (Vec<CommitInfo>, Option<(String, String)>) = {
                let display = display.borrow();
                let commits = commits.borrow();
                let mut selected: Vec<usize> = list
                    .selected_rows()
                    .iter()
                    .filter_map(|r| {
                        let i = r.index();
                        (i >= 0).then_some(i as usize)
                    })
                    .collect();
                selected.sort_unstable();
                let mut commit_infos = Vec::new();
                let mut wc = None;
                for di in selected {
                    match display.get(di) {
                        Some(DisplayRow::Commit(ci)) => {
                            if let Some(c) = commits.get(*ci) {
                                commit_infos.push(c.clone());
                            }
                        }
                        Some(DisplayRow::Wc { branch, entry }) if wc.is_none() => {
                            wc = Some((branch.clone(), entry.info.change_id_hex()));
                        }
                        _ => {}
                    }
                }
                (commit_infos, wc)
            };

            // A working-copy `@` is selected: show its editable diff and the
            // craft-a-commit pane (empty message + git-default identity baseline) —
            // Save with no message edits the `@` in place, with a message commits it
            // on that worktree's tip (see the `save` closure). Conflict mode never
            // selects `@` rows, so this can't collide with it.
            if let Some((branch, change)) = wc {
                viewing_wc.set(true);
                *selected_wc_branch.borrow_mut() = Some(branch);
                *selected_wc_change.borrow_mut() = Some(change);
                selected_changes.borrow_mut().clear();
                selected_change.borrow_mut().take();
                clear_identity_differs(&identity_fields);
                message_buffer.set_text("");
                message_view.set_editable(true);
                let baseline =
                    set_identity_fields_from(&identity_fields, &repo.borrow().default_identity());
                *wc_identity_baseline.borrow_mut() = baseline;
                for f in identity_fields.iter() {
                    f.set_sensitive(true);
                }
                save_button.set_tooltip_text(Some(SAVE_HINT_WORKCOPY));
                diff_read_only.set(false);
                recompute_blame(); // selection cleared above: drop any stale blame
                load_wc_changes();
                update_save_sensitivity();
                return;
            }

            let infos = commit_infos;
            *selected_changes.borrow_mut() = infos.iter().map(|c| c.change_id_hex()).collect();
            *selected_change.borrow_mut() = infos.first().map(|c| c.change_id_hex());

            // Leaving the working-copy view for a history selection (or none).
            viewing_wc.set(false);
            selected_wc_branch.borrow_mut().take();
            selected_wc_change.borrow_mut().take();

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
                    recompute_blame(); // selection cleared above: drop any stale blame
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
                    // Blame this commit's old side, before its diff text lands.
                    *blame_selection.borrow_mut() = vec![info.id.clone()];
                    recompute_blame();
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
                        Ok(Some(ch)) => {
                            // Blame the combined old side (oldest commit's parent),
                            // before the diff text lands.
                            *blame_selection.borrow_mut() = ids.clone();
                            recompute_blame();
                            apply_changes(ch);
                        }
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
        let display = display.clone();
        let update_selection_pane = update_selection_pane.clone();
        let selection_sync = selection_sync.clone();
        let selection_anchor = selection_anchor.clone();
        move |gesture, _n, _x, y| {
            let Some(row) = list.row_at_y(y as i32) else {
                return;
            };
            let i = row.index();
            let is_commit = |j: i32| {
                matches!(
                    display.borrow().get(j as usize),
                    Some(DisplayRow::Commit(_))
                )
            };
            let clicked_is_wc = !is_commit(i);
            let state = gesture.current_event_state();
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            // Drive the selection ourselves under `selection_sync`, then render once.
            selection_sync.set(true);
            if clicked_is_wc {
                // A working-copy `@` is a single, exclusive selection: it never joins
                // a multi-commit selection, and modifiers don't extend across it.
                list.unselect_all();
                list.select_row(Some(&row));
                selection_anchor.set(Some(i));
            } else if shift {
                // Extend from the anchor (or the current selection's nearest row) to
                // the clicked row, selecting the commit rows between (skipping `@`s so
                // a range stays a pure multi-commit selection).
                let anchor = selection_anchor
                    .get()
                    .or_else(|| list.selected_rows().iter().map(|r| r.index()).min())
                    .unwrap_or(i);
                list.unselect_all();
                for j in i.min(anchor)..=i.max(anchor) {
                    if is_commit(j) {
                        if let Some(r) = list.row_at_index(j) {
                            list.select_row(Some(&r));
                        }
                    }
                }
            } else if ctrl {
                // Toggle just this commit row, leaving the rest of the selection — but
                // first drop any working-copy `@` from it (the two never mix).
                for r in list.selected_rows() {
                    if !is_commit(r.index()) {
                        list.unselect_row(&r);
                    }
                }
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

    // Working-copy `@` rows now live in the history `list` as hollow lane nodes;
    // selecting one is handled by `update_selection_pane` (the unified selection
    // router), and the rows are (re)built by `refresh` from the interleaved
    // `display` list — so there is no separate working-copy list, row handler, or
    // `refresh_wc` here any more.

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
        let display = display.clone();
        let draw_graph = draw_graph.clone();
        let hollow = hollow.clone();
        let commit_rows = commit_rows.clone();
        let list = list.clone();
        let selected_change = selected_change.clone();
        let selected_changes = selected_changes.clone();
        let viewing_wc = viewing_wc.clone();
        let selected_wc_change = selected_wc_change.clone();
        let selected_wc_branch = selected_wc_branch.clone();
        let selection_sync = selection_sync.clone();
        let update_selection_pane = update_selection_pane.clone();
        let identities = identities.clone();
        let history_limit = history_limit.clone();
        let history_has_more = history_has_more.clone();
        let on_revert = on_revert.clone();
        let on_merge_out = on_merge_out.clone();
        let on_lint = on_lint.clone();
        let search_query = search_query.clone();
        let search_matches = search_matches.clone();
        let search_cursor = search_cursor.clone();
        Rc::new(move || {
            let (loaded, has_more) = {
                let r = repo.borrow();
                let limit = history_limit.get();
                match r.head_commit_id() {
                    Some(head) => {
                        // The editable set is the source of truth (the dropdown toggles
                        // it via `set_editable_branches`). Seed the history walk from
                        // every editable branch's real bookmark tip; a singleton set
                        // walks just that branch's chain, a wider set unions them into
                        // one DAG. Every commit shown is now editable — no view-only
                        // gating.
                        let editable = r.editable_branches();
                        if editable.len() <= 1 {
                            history_limited(&r.repo, &head, 0, limit).unwrap_or_default()
                        } else {
                            let branches = r.local_branches();
                            let mut heads = vec![head.clone()];
                            for name in &editable {
                                if let Some(b) = branches.iter().find(|b| &b.name == name) {
                                    if b.head != head {
                                        heads.push(b.head.clone());
                                    }
                                }
                            }
                            r.history_multi(&heads, 0, limit).unwrap_or_default()
                        }
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
            // Build the interleaved display list: each real commit, with every
            // worktree's uncommitted `@` spliced in as a hollow lane node directly
            // above its tip. `worktree_uncommitted` groups entries newest-first per
            // branch; the *oldest* entry sits on the real tip, so a group splices in
            // (newest first) just above the commit whose id is that entry's parent —
            // robust for a detached-HEAD launch too. `commits` stays pure (the
            // planners/MCP path see it byte-identically); `draw_graph` lays the
            // interleaved rows out so each `@` lands on its tip's own lane.
            {
                let uncommitted = repo.borrow().worktree_uncommitted();
                let cs = commits.borrow();
                let mut new_display: Vec<DisplayRow> = Vec::new();
                let mut draw_infos: Vec<CommitInfo> = Vec::new();
                let mut new_hollow: Vec<bool> = Vec::new();
                let mut new_commit_rows: Vec<usize> = Vec::with_capacity(cs.len());
                for (ci, c) in cs.iter().enumerate() {
                    for (branch, entries) in &uncommitted {
                        let on_this_tip = entries
                            .last()
                            .and_then(|e| e.info.parents.first())
                            .is_some_and(|p| *p == c.id);
                        if on_this_tip {
                            for entry in entries {
                                new_display.push(DisplayRow::Wc {
                                    branch: branch.clone(),
                                    entry: Box::new(entry.clone()),
                                });
                                draw_infos.push(entry.info.clone());
                                new_hollow.push(true);
                            }
                        }
                    }
                    new_commit_rows.push(new_display.len());
                    new_display.push(DisplayRow::Commit(ci));
                    draw_infos.push(c.clone());
                    new_hollow.push(false);
                }
                drop(cs);
                let root = repo.borrow().root_commit_id();
                *draw_graph.borrow_mut() = compute_graph(&draw_infos, &root);
                *display.borrow_mut() = new_display;
                *hollow.borrow_mut() = new_hollow;
                *commit_rows.borrow_mut() = new_commit_rows;
            }
            {
                let cs = commits.borrow();
                let disp = display.borrow();
                let refs = repo.borrow().commit_refs();
                // Learn this repo's de-facto commit-message conventions from its own
                // history, so the per-row lint badge flags drift from *its* norm.
                let subjects: Vec<&str> = cs.iter().map(|c| c.subject.as_str()).collect();
                let style = msglint::RepoStyle::learn(&subjects);
                populate_history(
                    &list,
                    &disp,
                    &cs,
                    &draw_graph,
                    &hollow,
                    &HashSet::new(),
                    &refs,
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
            if viewing_wc.get() {
                // Re-select the working-copy `@` row being viewed (by branch + change
                // id, mapping past the interleaved commit rows). If its `@` is gone
                // (committed / discarded / the tree went clean) drop the view.
                let di = find_wc_row(
                    &display.borrow(),
                    selected_wc_branch.borrow().as_deref(),
                    selected_wc_change.borrow().as_deref(),
                );
                match di {
                    Some(di) => {
                        if let Some(row) = list.row_at_index(di as i32) {
                            list.select_row(Some(&row));
                        }
                    }
                    None => {
                        viewing_wc.set(false);
                        selected_wc_branch.borrow_mut().take();
                        selected_wc_change.borrow_mut().take();
                    }
                }
            } else {
                // A commit (or none): re-select by change id, mapping each commit
                // index to its (shifted) display row via `commit_rows`.
                for change in &targets {
                    let di = commits
                        .borrow()
                        .iter()
                        .position(|c| c.change_id_hex() == *change)
                        .and_then(|ci| commit_rows.borrow().get(ci).copied());
                    if let Some(di) = di {
                        if let Some(row) = list.row_at_index(di as i32) {
                            list.select_row(Some(&row));
                        }
                    }
                }
            }
            selection_sync.set(false);
            update_selection_pane();
            // populate_history reset the row labels to plain text; re-apply an active
            // search so its highlights survive the rebuild. The selection was just
            // restored by change id, so the Enter cursor is stale — reset it.
            {
                let query = search_query.borrow();
                if !query.is_empty() {
                    *search_matches.borrow_mut() = rows::apply_search_highlight(
                        &list,
                        &display.borrow(),
                        &commits.borrow(),
                        &query,
                    );
                    search_cursor.set(None);
                }
            }
        })
    };

    // Select a sensible default row when nothing is selected: the newest *commit*
    // row, skipping a leading working-copy `@` node (which would open the craft-a-
    // commit pane rather than show a commit). Fires `selected-rows-changed`, so the
    // pane router runs once afterwards.
    let select_default: Rc<dyn Fn()> = {
        let list = list.clone();
        let display = display.clone();
        Rc::new(move || {
            if list.selected_rows().is_empty() {
                if let Some(di) = first_commit_row(&display.borrow()) {
                    if let Some(row) = list.row_at_index(di as i32) {
                        list.select_row(Some(&row));
                    }
                }
            }
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
        let display = display.clone();
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
            let display = display.clone();
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
                // Map the clicked list row to its commit index (the revert button
                // only sits on commit rows, but be defensive) — `idx` thereafter is
                // a commit index, the space the plan graph's `boundaries` use.
                let Some(idx) = row_commit_index(&display.borrow(), idx as usize) else {
                    return;
                };
                // Resolve the clicked commit and the slot to splice its revert into.
                let (target, change, new_children) = {
                    let commits = commits.borrow();
                    let Some(commit) = commits.get(idx) else {
                        return;
                    };
                    let target = commit.id.clone();
                    let change = commit.change_id_hex();
                    // Parent the revert on the clicked commit; its children are the
                    // commit's current branch children, which rebase onto the revert.
                    // The clicked commit (commit index `idx`) is the parent of the
                    // lane edge crossing the gap just above it (`boundaries[idx - 1]`);
                    // at the tip (idx 0) there are no children and the revert becomes
                    // the new HEAD.
                    let new_children = if idx == 0 {
                        Vec::new()
                    } else {
                        graph
                            .borrow()
                            .boundaries
                            .get(idx - 1)
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
        let display = display.clone();
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
            let display = display.clone();
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
                // Map the clicked list row to its commit index — the space the plan
                // graph's `boundaries` use (the merge-out button only sits on commit
                // rows, but be defensive).
                let Some(idx) = row_commit_index(&display.borrow(), idx as usize) else {
                    return;
                };
                // Resolve the clicked commit and the slot the merge splices into.
                let (target, change, new_children) = {
                    let commits = commits.borrow();
                    let Some(commit) = commits.get(idx) else {
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
                            .get(idx - 1)
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
        let trashed_origin = trashed_origin.clone();
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
            let trashed_origin = trashed_origin.clone();
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
                // Route the restore to the worktree of the branch this commit was
                // dropped from (recorded at drop time). No origin recorded ⇒ the
                // launch worktree (as before); an origin whose branch has no worktree
                // to land in ⇒ refuse rather than silently use the launch one.
                let origin = trashed_origin.borrow().get(&info.change_id_hex()).cloned();
                let target = match &origin {
                    Some(branch) => match repo.borrow().wc_target_for_branch(branch) {
                        Some(t) => t,
                        None => {
                            show_status(&format!(
                                "Can't restore: branch {branch} has no worktree to restore into"
                            ));
                            return;
                        }
                    },
                    None => WcTarget::Launch,
                };
                let outcome = repo
                    .borrow_mut()
                    .restore_to_working_copy_at(target, &info.id);
                match outcome {
                    Ok(SaveOutcome::Clean) => {
                        // Its changes are now uncommitted; drop it from the trash.
                        let change_hex = info.change_id_hex();
                        trashed
                            .borrow_mut()
                            .retain(|c| c.change_id_hex() != change_hex);
                        trashed_origin.borrow_mut().remove(&change_hex);
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
        let display = display.clone();
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
            let display = display.clone();
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
                // Map the clicked list row to its commit index (the lint badge only
                // sits on commit rows, but be defensive).
                let Some(idx) = row_commit_index(&display.borrow(), idx as usize) else {
                    return;
                };
                // Resolve the clicked commit and re-learn the repo's style (commits
                // may have changed since the badge was painted).
                let resolved = {
                    let cs = commits.borrow();
                    let Some(commit) = cs.get(idx) else {
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
        let select_default = select_default.clone();
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
                    let commit_rows = commit_rows.clone();
                    let affected = affected.clone();
                    move |_, _, _| {
                        clear_highlight(&list);
                        highlight_affected(
                            &list,
                            &commit_rows.borrow(),
                            &commits.borrow(),
                            &affected,
                        );
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
                let select_default = select_default.clone();
                let show_status = show_status.clone();
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
                    select_default();
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

    // The branch dropdown *is* the editable-branch set. Ticking a branch widens the
    // live `Repo`'s editable set (it joins the unified DAG as a real, rewritable
    // lane); unticking narrows it (its ref freezes again). Both go through the
    // engine's in-place `set_editable_branches`, so the session's undo history and
    // trash survive a toggle — unlike a full reopen. The set defaults to just the
    // opened branch; there is no pinned branch and no last-branch rule — unticking
    // the final branch empties the set, which simply shows no commits until a
    // branch is re-ticked. The list is (re)populated every time the popover opens
    // (branches move/appear out of band); it is a small transient list, not the
    // segfault-sensitive history list, so rebuilding its rows is safe.
    // Re-entrancy guard for the checkbox toggle: the Err-arm revert below calls
    // `set_active`, which *synchronously* re-fires `toggled`. Without this guard a
    // persistent `set_editable_branches` failure would ping-pong the checkbox
    // forever and overflow the stack (the failing call recurses through the revert).
    let branch_toggle_guard = Rc::new(Cell::new(false));
    branch_menu.connect_active_notify({
        let repo = repo.clone();
        let refresh = refresh.clone();
        let branch_list = branch_list.clone();
        let show_status = show_status.clone();
        let branch_toggle_guard = branch_toggle_guard.clone();
        move |mb| {
            if !mb.is_active() {
                return;
            }
            while let Some(child) = branch_list.first_child() {
                branch_list.remove(&child);
            }
            for b in repo.borrow().local_branches() {
                let check = CheckButton::with_label(&b.name);
                check.set_active(b.is_editable);
                // Connect *after* set_active, so seeding the initial state doesn't
                // fire a spurious toggle.
                check.connect_toggled({
                    let name = b.name.clone();
                    let repo = repo.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let branch_toggle_guard = branch_toggle_guard.clone();
                    move |c| {
                        // Ignore the re-entrant toggle the Err-arm revert provokes
                        // (see the guard's definition above).
                        if branch_toggle_guard.get() {
                            return;
                        }
                        // Compute the desired set from the live editable set ± this
                        // branch, preserving order (existing first, newcomers appended).
                        // Unticking the last branch leaves `desired` empty — that is
                        // allowed and yields an empty history (no last-branch rule).
                        let mut desired = repo.borrow().editable_branches();
                        if c.is_active() {
                            if !desired.contains(&name) {
                                desired.push(name.clone());
                            }
                        } else {
                            desired.retain(|n| n != &name);
                        }
                        // Bind in a `let` so the `borrow_mut()` temporary is released at
                        // the `;` — `refresh()` re-borrows `repo`, and a temporary held in
                        // a `match` scrutinee would still be alive in the arm, panicking
                        // with "already mutably borrowed".
                        let outcome = repo.borrow_mut().set_editable_branches(&desired);
                        match outcome {
                            Ok(()) => {
                                refresh();
                            }
                            Err(err) => {
                                // Revert the checkbox to match the unchanged set,
                                // guarding the programmatic `set_active` so the
                                // re-fired `toggled` is ignored (no recursion).
                                branch_toggle_guard.set(true);
                                c.set_active(!c.is_active());
                                branch_toggle_guard.set(false);
                                show_status(&format!("Could not change the branch set: {err}"));
                            }
                        }
                    }
                });
                branch_list.append(&check);
            }
        }
    });

    reload_button.connect_clicked({
        let repo = repo.clone();
        let repo_path = repo_path.clone();
        let branch = branch.clone();
        let exit_conflict_mode = exit_conflict_mode.clone();
        let refresh = refresh.clone();
        let select_default = select_default.clone();
        let show_status = show_status.clone();
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
            select_default();
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
        let message_buffer = message_buffer.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let nav_sync = nav_sync.clone();
        let splice_reload = splice_reload.clone();
        let selected_change = selected_change.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let identity_fields = identity_fields.clone();
        let original_identity = original_identity.clone();
        let pane_mode = pane_mode.clone();
        let resolve_current = resolve_current.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        let viewing_wc = viewing_wc.clone();
        let selected_wc_change = selected_wc_change.clone();
        let selected_wc_branch = selected_wc_branch.clone();
        let select_default = select_default.clone();
        let selected_changes = selected_changes.clone();
        let multi_identity_baseline = multi_identity_baseline.clone();
        let wc_identity_baseline = wc_identity_baseline.clone();
        Rc::new(move || {
            // In conflict mode, "Save" means "resolve the current conflicted file".
            if pane_mode.borrow().is_conflict() {
                resolve_current();
                return;
            }
            // Viewing a working-copy `@` (the selected `@` node). The commit message
            // gates what Save does: with no message, the edited diff is written back
            // to that worktree's `@` in place (it stays uncommitted); with a message,
            // the uncommitted changes are crystallized into a real commit on its tip.
            // The branch's `WcTarget` routes every action at the right worktree's `@`.
            if viewing_wc.get() {
                let change = selected_wc_change.borrow().clone();
                let branch = selected_wc_branch.borrow().clone();
                let Some(target) = branch
                    .as_deref()
                    .and_then(|b| repo.borrow().wc_target_for_branch(b))
                else {
                    show_status("That working copy is no longer editable here");
                    return;
                };
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
                for edit in &edits {
                    if let Err(err) = repo.borrow_mut().edit_working_copy_file_at(
                        target.clone(),
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
                    // No message: leave the changes uncommitted. `refresh` rebuilds
                    // the `@` rows and re-selects this one (its change id is stable
                    // across the in-place edit), reloading the diff. `splice_reload`
                    // + `nav_sync` keep it scrolled where it was and the dropdown on
                    // the same file (see the commit-save path); restore the caret to
                    // the user's spot.
                    nav_sync.set(true);
                    splice_reload.set(true);
                    refresh();
                    splice_reload.set(false);
                    nav_sync.set(false);
                    let offset = saved_cursor.min(file_buffer.char_count());
                    file_buffer.place_cursor(&file_buffer.iter_at_offset(offset));
                    return;
                }
                // A message was given: commit the displayed entry's slice on its
                // worktree's branch tip. Both the launch and a sibling worktree's `@`
                // commit exactly the selected entry (a split chain commits one piece
                // at a time, the rest staying uncommitted); a lone entry collapses to
                // committing the whole `@`. Pass the identity only when the user
                // overrode the prefilled git default; otherwise let the engine stamp
                // git config + a fresh "now".
                let baseline = wc_identity_baseline.borrow().clone();
                let current: [String; 4] =
                    std::array::from_fn(|i| identity_fields[i].text().to_string());
                let identity = (current != baseline).then(|| read_identity(&identity_fields));
                let outcome = match &target {
                    WcTarget::Launch => repo.borrow_mut().commit_working_copy_entry(
                        change.as_deref(),
                        message,
                        identity.as_ref(),
                    ),
                    _ => repo.borrow_mut().commit_working_copy_entry_at(
                        target.clone(),
                        change.as_deref(),
                        message,
                        identity.as_ref(),
                    ),
                };
                match outcome {
                    Ok(SaveOutcome::Clean) => {
                        // Committed: leave the `@` view and select the newest commit
                        // (the new tip when the launch branch is topmost), ready to
                        // refine its just-committed message in place.
                        viewing_wc.set(false);
                        selected_wc_change.borrow_mut().take();
                        selected_wc_branch.borrow_mut().take();
                        selected_change.borrow_mut().take();
                        selected_changes.borrow_mut().clear();
                        refresh();
                        select_default();
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

            // Remember focus so a Save from the diff keeps it. The scroll position
            // needs no capture: the reload below splices the new diff into the buffer
            // (`splice_reload`) instead of `set_text`-ing it, so the SourceView's
            // scroll anchor stays put on its own.
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
            // `apply_changes`. `splice_reload` makes that reload splice the new diff
            // into the buffer (which keeps the SourceView's scroll anchor) instead of
            // `set_text` (which would reset it to the top), and `nav_sync` suppresses
            // the scroll->dropdown sync while it runs. So a save leaves the diff put.
            nav_sync.set(true);
            splice_reload.set(true);
            refresh();
            splice_reload.set(false);
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
        let selected_wc_branch = selected_wc_branch.clone();
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

            // Splitting a working-copy `@`: a pure jj-side peel — no history change,
            // so only the `@` rows and this diff reload. Works on any editable
            // worktree's `@` (the launch one or a sibling's); a branch with no
            // worktree has no `@` to split. The edited entry keeps its change id, so
            // `refresh` re-selects it and reloads the diff.
            if viewing_wc.get() {
                let change = selected_wc_change.borrow().clone();
                let branch = selected_wc_branch.borrow().clone();
                let Some(target) = branch
                    .as_deref()
                    .and_then(|b| repo.borrow().wc_target_for_branch(b))
                else {
                    show_status(
                        "This branch has no worktree, so its uncommitted changes can't be split",
                    );
                    return;
                };
                if let Err(err) =
                    repo.borrow_mut()
                        .split_working_copy_edits_at(target, change.as_deref(), &edits)
                {
                    show_status(&format!("Split failed: {err}"));
                    return;
                }
                refresh();
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
        let display = display.clone();
        let search_query = search_query.clone();
        let search_matches = search_matches.clone();
        let search_cursor = search_cursor.clone();
        move |entry| {
            let query = entry.text().to_string();
            let matches =
                rows::apply_search_highlight(&list, &display.borrow(), &commits.borrow(), &query);
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
    select_default();

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
