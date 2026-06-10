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
/// applied on a clean resolution and discarded on abort. `Drop` adds the commit
/// to the trash (a history→trash drop that conflicted); `Restore` removes it (a
/// trash→history restore that conflicted).
pub(crate) enum PendingTrashOp {
    Drop(CommitInfo),
    Restore(CommitInfo),
}

/// Which side(s) of a conflict block a quick-resolve action keeps.
#[derive(Clone, Copy)]
pub(crate) enum Side {
    Ours,
    Theirs,
    Both,
}

/// Inline, clickable quick-resolve cues appended to a conflict block's marker
/// lines — the same idiom as the diff view's "expand context" cue. Clicking the
/// marker line keeps the indicated side(s) and drops the markers: "use ours"
/// after `<<<<<<<`, "use theirs" after `>>>>>>>`, "use both" after `=======`.
pub(crate) const CUE_OURS: &str = " ◀ ➜ use ours ▶";
pub(crate) const CUE_BOTH: &str = " ◀ ➜ use both ▶";
pub(crate) const CUE_THEIRS: &str = " ◀ ➜ use theirs ▶";
/// The end-caps that make a cue read as a banner/tag-shaped button. Painted as a
/// full-height triangle in the button colour against the line background, their
/// flat (vertical) side sits flush against the solid-fill body between them, so
/// they align in height and touch the block, giving pointed ends. The left cap
/// also marks where the clickable button begins.
pub(crate) const CUE_CAP_L: char = '◀';
pub(crate) const CUE_CAP_R: char = '▶';

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
pub(crate) const ABORT_HINT: &str =
    "Discard the entire rewrite and roll the repository back to the state it had \
     before you saved, leaving git untouched.";
/// Hover hint for the diff view's Split button (enabled only with pending diff edits).
pub(crate) const SPLIT_HINT: &str =
    "Split this commit in two: rewrite it to your edited diff, and add a new commit \
     after it holding the changes you took out — so the two together reproduce the \
     original commit and its descendants stay unchanged.";

/// Inline cues that *drop* changes from the diff — the mirror of "expand
/// context". `revert hunk` sits on each `@@` header (next to the expand cue),
/// `revert file` on each `diff --git` separator. Clicking one rewrites the diff
/// so those changes vanish, leaving a pending edit; the user then Saves (drops
/// them) or Splits (peels them into a separate commit). Shown only for modified
/// text files (see `build_diff_buffer_text`).
pub(crate) const REVERT_HUNK_LABEL: &str = "⤺ revert hunk";
pub(crate) const REVERT_FILE_LABEL: &str = "⤺ revert file";

/// Which inline cue a click/hover landed on in the (non-conflict) diff view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffCue {
    /// The "expand context" pill on an expandable `@@` header — widen this hunk's
    /// context (group range to grow on each side).
    Expand(usize, usize),
    /// The "revert hunk" pill — drop this hunk's changes (its group range).
    RevertHunk(usize, usize),
    /// The "revert file" pill on a `diff --git` line — drop the whole file's changes.
    RevertFile,
}

/// Label of the conflict pane's elision cue — the pill standing in for a hidden
/// run of unconflicted lines between snippets. Clicking it reveals more context.
pub(crate) const CONFLICT_CUE_LABEL: &str = "↕ expand hidden lines";

/// The standalone notice shown for a structural (non-text-resolvable) conflicted
/// file in the combined conflict view.
pub(crate) const CONFLICT_STRUCTURAL_NOTICE: &str =
    "⚠ structural conflict — can't be resolved as text here; use “Abort rewrite”";

/// Status-line hint shown when the patch firewall blocks an edit that would break
/// the unified-diff structure.
pub(crate) const READ_ONLY_HINT: &str = "Edit blocked — this change would break the patch structure.";
/// Status-line hint shown when an edit would touch the conflict view's structural
/// layout lines (file headers, elision cues, notices).
pub(crate) const CONFLICT_LAYOUT_HINT: &str =
    "Edit blocked — this line is part of the conflict view layout. Edit within a snippet.";

/// Grouped bundles of the `build_ui` state handed to the peeled modules
/// (`dragdrop`, `conflict`). Every field is an `Rc` or a GTK widget handle (itself
/// refcounted), so the derived `Clone` is cheap. The bundles are assembled in
/// `build_ui` by cloning its existing locals — no state is duplicated, both point
/// at the same `Rc` — and handed to the modules as borrowed bundles, which clone
/// out the individual handles their closures capture.

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
    pub(crate) pane_mode: Rc<RefCell<PaneMode>>,
    pub(crate) conflict_view: Rc<RefCell<Vec<ConflictFileView>>>,
}

/// The transient drag-only cells.
#[derive(Clone)]
pub(crate) struct DragState {
    pub(crate) drag_origin: Rc<Cell<DragOrigin>>,
    pub(crate) drag_row: Rc<RefCell<Option<ListBoxRow>>>,
    pub(crate) drag_from: Rc<Cell<Option<usize>>>,
    pub(crate) drop_gap: Rc<Cell<Option<usize>>>,
    pub(crate) drop_onto: Rc<Cell<Option<usize>>>,
    pub(crate) post_drag: Rc<RefCell<Option<Box<dyn FnOnce()>>>>,
}

/// The late-bound cross-module callbacks.
#[derive(Clone)]
pub(crate) struct Callbacks {
    pub(crate) refresh: Rc<dyn Fn()>,
    pub(crate) show_status: Rc<dyn Fn(&str)>,
    pub(crate) enter_conflict_mode: Rc<dyn Fn(Vec<ConflictedCommit>)>,
    pub(crate) exit_conflict_mode: Rc<dyn Fn()>,
}
