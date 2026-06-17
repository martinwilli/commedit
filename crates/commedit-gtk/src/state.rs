//! Shared UI types and constants used across the GTK modules.
//!
//! These are the enums, the late-bound-callback alias, and the string constants
//! that several of the peeled modules (`dragdrop`, `conflict`, `highlight`, …) and
//! `build_ui` all reach for. Keeping them in one place lets each module depend on
//! the vocabulary without depending on each other.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use commedit_engine::conflict::ConflictedCommit;
use commedit_engine::diff::{ConflictPiece, ContextExpansion};
use commedit_engine::graph::GraphLayout;
use commedit_engine::history::CommitInfo;
use commedit_engine::repo::Repo;
use commedit_engine::workcopy::WorkingCopyEntry;
use gtk::{Box as GtkBox, Button, Label, ListBox, ListBoxRow, ScrolledWindow};

pub(crate) const APP_ID: &str = "net.willi.commedit";

/// How many history rows to load per page. The list starts with one page and
/// grows by another whenever the user scrolls near the bottom (see the
/// `history_scroll` edge handler), so opening a deep repo stays cheap.
pub(crate) const HISTORY_PAGE: usize = 64;

/// A reference-counted, re-entrant "render the current diff" callback. Boxed so
/// the embedded expand-context buttons can hold and invoke it after they widen a
/// hunk (the renderer rebuilds the buffer and the buttons themselves).
pub(crate) type Renderer = Rc<dyn Fn()>;

/// A one-shot action staged from a drag gesture and run at idle from `drag-end`
/// (see `dragdrop::run_post_drag`); `None` between drags.
pub(crate) type PostDrag = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

/// A history row's revert button (floating at the row's right edge) calls this with
/// the row's display index to drop a revert of that commit directly on top of it
/// (wired in `build_ui`, captured by each row in `rows::add_revert_button`).
pub(crate) type RevertCallback = Rc<dyn Fn(i32)>;

/// A history row's merge-out button (beside the revert button) calls this with the
/// row's display index to introduce a merge commit directly above that commit — the
/// commit becomes a side branch the new merge folds back (wired in `build_ui`,
/// captured by each row in `rows::add_merge_out_button`).
pub(crate) type MergeOutCallback = Rc<dyn Fn(i32)>;

/// A trash row's restore button calls this with the row's display index to write
/// that dropped commit's changes to the working tree as uncommitted edits (and
/// remove it from the trash) — wired in `build_ui`, captured by each trash row in
/// `rows::add_restore_button`.
pub(crate) type RestoreToWorktreeCallback = Rc<dyn Fn(i32)>;

/// A history row's commit-style badge calls this with the row's display index when
/// the commit's summary drifts from the repo's de-facto conventions (see
/// `crate::msglint`): it auto-fixes the mechanical issues (case, trailing period)
/// or, when only judgment issues remain, selects the commit for manual editing.
/// Wired in `build_ui`, captured by each row in `rows::build_lint_badge`.
pub(crate) type LintFixCallback = Rc<dyn Fn(i32)>;

/// Which list a drag started in, so the shared drop handlers can tell a reorder
/// (history → history), a drop (history → trash), a restore (trash → history) and
/// a working-copy fold (working copy → commit) apart. The carried value is just
/// the source row index; this says where from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragOrigin {
    History,
    Trash,
    /// A working-copy entry being dragged onto a commit to fold it in (fixup).
    WorkingCopy,
}

/// Which content the diff pane is showing. In `Diff` mode it's the usual
/// editable unified diff guarded by the patch firewall. In `Conflict` mode a
/// rewrite produced conflicts that git is held back from until they're resolved:
/// the pane shows a conflicted file materialized with 2-way markers, edited
/// free-form (the firewall is bypassed), and saving resolves rather than
/// rewrites.
pub(crate) enum PaneMode {
    Diff,
    Conflict(ConflictCtx),
}

impl PaneMode {
    pub(crate) fn is_conflict(&self) -> bool {
        matches!(self, PaneMode::Conflict(_))
    }
}

/// The live state of an in-progress conflict resolution: the conflicted commits,
/// refreshed from the engine after each resolution step, oldest first.
pub(crate) struct ConflictCtx {
    pub(crate) commits: Vec<ConflictedCommit>,
}

/// Per-file state of the combined conflict-snippet buffer currently shown (one
/// per conflicted file of the selected commit). The buffer shows only each file's
/// conflict snippets — its `<<< … >>>` blocks plus context, with the long
/// unconflicted runs elided behind a cue — so on save we reconstruct each whole
/// file from the (edited) shown segments interleaved with the verbatim elided
/// runs recorded in `pieces`.
pub(crate) struct ConflictFileView {
    pub(crate) path: String,
    /// False for structural (non-text) conflicts, shown as a read-only notice.
    pub(crate) resolvable: bool,
    /// Marker length jj used, echoed back on resolve so the edit re-parses.
    pub(crate) marker_len: usize,
    /// The file's current full conflict text (source of truth): re-windowed on
    /// render, refreshed from the buffer (capturing edits) on expand/save.
    pub(crate) full_text: String,
    /// Per-file snippet context expansion (the elision cues widen it).
    pub(crate) exp: ContextExpansion,
    /// Pieces recorded at the last render, for reconstructing the full file.
    pub(crate) pieces: Vec<ConflictPiece>,
    /// The elision gaps recorded at the last render, in document order, as
    /// `(above_block, below_block)` — which blocks' context a cue click widens.
    pub(crate) gaps: Vec<(Option<usize>, Option<usize>)>,
}

impl ConflictCtx {
    /// The change ids (hex) of commits that still have conflicts — used to badge
    /// the matching history rows.
    pub(crate) fn conflicted_changes(&self) -> HashSet<String> {
        self.commits
            .iter()
            .filter(|c| !c.files.is_empty())
            .map(|c| c.change_id_hex())
            .collect()
    }
}

/// A trash-list change deferred while a dropped/restored commit's rewrite is held
/// back for conflict resolution. The rewrite isn't applied to git until the
/// conflicts clear, so the trash list must not change yet either: the op is
/// applied on a clean resolution and discarded on abort. `Drop` adds the
/// commit(s) to the trash (a history→trash drop that conflicted — one entry per
/// commit for a multi-selection drop); `Restore` removes one (a trash→history
/// restore that conflicted).
pub(crate) enum PendingTrashOp {
    Drop(Vec<CommitInfo>),
    Restore(Box<CommitInfo>),
}

/// Which side(s) of a conflict block a quick-resolve action keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Ours,
    Theirs,
    Both,
}

/// Tooltips for the action-bar buttons. The Save button means different things
/// per pane mode — committing an edit in the diff view, resolving a file in the
/// conflict view — so its tooltip is swapped when entering/leaving conflict mode.
pub(crate) const SAVE_HINT_DIFF: &str =
    "Save your edits to this commit — message, identity, or file content — \
     rewriting it in place and rebasing its descendants onto the result.";
pub(crate) const SAVE_HINT_CONFLICT: &str =
    "Resolve the conflicted file shown above. When a rewrite conflicts across \
     several files you resolve them one at a time — save each in turn; the \
     rewrite is applied to git only once the last conflict is cleared.";
/// Hover hint for the Save button while a working-copy entry is selected, where
/// Save's meaning is gated on the commit message: with the message empty it
/// writes the edited diff back to the working copy in place (leaving it
/// uncommitted); with a message it crystallizes the uncommitted changes into a
/// real commit on top of HEAD.
pub(crate) const SAVE_HINT_WORKCOPY: &str =
    "With no commit message, save your diff edits back to the working copy, still \
     uncommitted. Type a message to instead commit the uncommitted changes on top \
     of HEAD — author/committer default to your git identity unless you set them.";
pub(crate) const ABORT_HINT: &str =
    "Discard the entire rewrite and roll the repository back to the state it had \
     before you saved, leaving git untouched.";
/// Hover hint for the diff view's Split button (enabled only with pending diff edits).
pub(crate) const SPLIT_HINT: &str =
    "Split this commit in two: rewrite it to your edited diff, and add a new commit \
     after it holding the changes you took out — so the two together reproduce the \
     original commit and its descendants stay unchanged.";

/// A diff-view gutter cue action (`diff_cues`): widen a hunk's context, drop a
/// hunk's changes, or drop a whole file's changes. The first two carry the hunk's
/// inclusive change-group range. A revert rewrites the diff so those changes
/// vanish, leaving a pending edit; the user then Saves (drops them) or Splits
/// (peels them into a separate commit). Offered only for modified text files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffCue {
    /// Widen this hunk's context (group range to grow on each side).
    Expand(usize, usize),
    /// Drop this hunk's changes (its group range).
    RevertHunk(usize, usize),
    /// Drop the whole file's changes.
    RevertFile,
}

/// The conflict pane's elision placeholder — a single plain line standing in for
/// a hidden run of unconflicted lines between snippets. It carries no number and
/// is not editable; the gutter draws an `↕` "expand" button beside it (see
/// `conflict_cue_cells`), and clicking that reveals more context. Kept a single
/// constant so the buffer scanners can recognise it by exact match.
pub(crate) const CONFLICT_ELISION_LINE: &str = "\u{22ef} hidden lines \u{22ef}";

/// The standalone notice shown for a structural (non-text-resolvable) conflicted
/// file in the combined conflict view.
pub(crate) const CONFLICT_STRUCTURAL_NOTICE: &str =
    "⚠ structural conflict — can't be resolved as text here; use “Abort rewrite”";

/// Status-line hint shown when the patch firewall blocks an edit that would break
/// the unified-diff structure.
pub(crate) const READ_ONLY_HINT: &str =
    "Edit blocked — this change would break the patch structure.";
/// Status-line hint shown when an edit would touch the conflict view's structural
/// layout lines (file headers, elision cues, notices).
pub(crate) const CONFLICT_LAYOUT_HINT: &str =
    "Edit blocked — this line is part of the conflict view layout. Edit within a snippet.";

// Grouped bundles of the `build_ui` state handed to the peeled modules
// (`dragdrop`, `conflict`). Every field is an `Rc` or a GTK widget handle (itself
// refcounted), so the derived `Clone` is cheap. The bundles are assembled in
// `build_ui` by cloning its existing locals — no state is duplicated, both point
// at the same `Rc` — and handed to the modules as borrowed bundles, which clone
// out the individual handles their closures capture.

/// The GTK widget handles a peeled module captures.
#[derive(Clone)]
pub(crate) struct Widgets {
    pub(crate) list: ListBox,
    pub(crate) placeholder: ListBoxRow,
    pub(crate) trash_list: ListBox,
    pub(crate) trash_scroll: ScrolledWindow,
    pub(crate) trash_box: GtkBox,
    pub(crate) wc_list: ListBox,
    pub(crate) file_buffer: sourceview5::Buffer,
    pub(crate) file_view: sourceview5::View,
    pub(crate) save_button: Button,
    pub(crate) prev_conflict_button: Button,
    pub(crate) next_conflict_button: Button,
    pub(crate) conflict_banner: GtkBox,
    pub(crate) conflict_label: Label,
    pub(crate) abort_button: Button,
}

/// The model cells a peeled module reads/writes.
#[derive(Clone)]
pub(crate) struct Data {
    pub(crate) repo: Rc<RefCell<Repo>>,
    pub(crate) commits: Rc<RefCell<Vec<CommitInfo>>>,
    /// The history list's ancestry-graph layout, recomputed with `commits`.
    pub(crate) graph: Rc<RefCell<GraphLayout>>,
    pub(crate) trashed: Rc<RefCell<Vec<CommitInfo>>>,
    /// A trash-list change deferred until a conflicted drop/restore resolves.
    pub(crate) pending_trash_op: Rc<RefCell<Option<PendingTrashOp>>>,
    pub(crate) wc_entries: Rc<RefCell<Vec<WorkingCopyEntry>>>,
    pub(crate) selected_change: Rc<RefCell<Option<String>>>,
    /// The full multi-selection as change ids, newest-first (the anchor
    /// `selected_change` is its first entry). `dragdrop` reads it to drag the
    /// whole selection as a group; a 0/1-length set is the single-commit case.
    pub(crate) selected_changes: Rc<RefCell<Vec<String>>>,
    pub(crate) pane_mode: Rc<RefCell<PaneMode>>,
    pub(crate) conflict_view: Rc<RefCell<Vec<ConflictFileView>>>,
}

/// The transient drag-only cells.
#[derive(Clone)]
pub(crate) struct DragState {
    pub(crate) drag_origin: Rc<Cell<DragOrigin>>,
    pub(crate) drag_row: Rc<RefCell<Option<ListBoxRow>>>,
    pub(crate) drag_from: Rc<Cell<Option<usize>>>,
    /// The display indices (newest-first) of a multi-selection being dragged as a
    /// group, captured at drag start. Empty for an ordinary single-commit drag;
    /// indices stay valid for the gesture since no rewrite runs until `drag-end`.
    pub(crate) drag_set: Rc<RefCell<Vec<usize>>>,
    pub(crate) drop_gap: Rc<Cell<Option<usize>>>,
    pub(crate) drop_onto: Rc<Cell<Option<usize>>>,
    pub(crate) post_drag: PostDrag,
}

/// The late-bound cross-module callbacks.
#[derive(Clone)]
pub(crate) struct Callbacks {
    pub(crate) refresh: Rc<dyn Fn()>,
    pub(crate) show_status: Rc<dyn Fn(&str)>,
    pub(crate) enter_conflict_mode: Rc<dyn Fn(Vec<ConflictedCommit>)>,
    pub(crate) exit_conflict_mode: Rc<dyn Fn()>,
    /// Restore a trashed commit's changes to the working tree (by display index),
    /// for `dragdrop` to repopulate the trash list with its restore buttons intact.
    pub(crate) on_restore: RestoreToWorktreeCallback,
}
