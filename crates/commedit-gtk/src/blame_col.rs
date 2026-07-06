//! The diff pane's optional **blame** gutter column.
//!
//! [`BlameColumn`] is a `sourceview5::GutterRenderer` at gutter index 0 — the far
//! left, *before* the old|new line-number columns in [`crate::diff_cues`] — that
//! draws, per context / removed line, the short hash of the commit that last
//! touched it in the *old* (pre-image) file — the engine's
//! [`commedit_engine::blame::FileBlame`], computed only while the user expands the
//! column via the persistent sidebar handle left of the gutter (`main.rs`'s
//! `blame_strip`). Hovering a cell invokes a late-bound
//! callback with the originating commit's `change_id` hex, so the caller can
//! highlight that commit's row in the history list; the cell reads as a
//! hyperlink (pointer cursor + underline) and clicking it fires a second
//! late-bound callback so the caller can open that origin commit.
//!
//! The line→origin mapping ([`blame_cells`] via the pure [`diff_old_refs`]) is
//! GTK-free and inline-tested; only the rendering and hover plumbing touch GTK.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use commedit_engine::blame::FileBlame;
use commedit_engine::diff::CombinedFile;
use commedit_engine::history::CommitInfo;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use sourceview5::prelude::*;
use sourceview5::subclass::prelude::*;
use sourceview5::{GutterLines, GutterRendererAlignmentMode};

use crate::linenums::DiffLineNo;

/// Horizontal padding around the hash text, in pixels (matches the number
/// column's [`crate::diff_cues`] padding).
const XPAD: i32 = 4;

/// A hover callback, invoked with the hovered cell's `change_id` hex (or `None`
/// when the pointer leaves a blamed line).
pub(crate) type HoverFn = Rc<dyn Fn(Option<&str>)>;

/// An activation (click) callback, invoked with the clicked cell's `change_id`
/// hex so the caller can open that origin commit.
pub(crate) type ActivateFn = Rc<dyn Fn(&str)>;

/// One blame cell: what to draw, plus the data the hover/highlight needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlameCell {
    /// The 8-char commit short hash drawn in the gutter.
    pub short: String,
    /// The originating commit's stable `change_id` hex, the key the history list
    /// highlights on.
    pub change_id_hex: String,
    /// Author / date / subject, shown as the cell's tooltip.
    pub tooltip: String,
}

impl BlameCell {
    /// Build a cell from a blamed origin commit. The short hash matches the
    /// history rows' 8-char `<tt>` id (`rows.rs`).
    fn from_info(info: &CommitInfo) -> Self {
        Self {
            short: info.id_hex().chars().take(8).collect(),
            change_id_hex: info.change_id_hex(),
            tooltip: format!(
                "{}\n{} · {}",
                info.subject, info.author_name, info.author_time
            ),
        }
    }
}

/// The dim gray used for the hashes at rest, matching the line numbers' tone
/// (`#6e7781`); a touch darker when hovered so the linked cell reads as live.
fn dim_color() -> gdk::RGBA {
    gdk::RGBA::new(0.431, 0.467, 0.506, 1.0)
}
fn hover_color() -> gdk::RGBA {
    gdk::RGBA::new(0.231, 0.251, 0.283, 1.0)
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct BlameColumn {
        /// Per-buffer-line cells; `None` where the line carries no blame (a `+`
        /// line, header, or separator). Refreshed wholesale on every buffer
        /// change while blame is on.
        pub(super) cells: RefCell<Vec<Option<BlameCell>>>,
        /// Line currently under the pointer (for the prelight), or -1.
        pub(super) hover: Cell<i32>,
        /// Desired column width, sized to the widest hash (0 when empty, so the
        /// column collapses while blame is off).
        pub(super) width_px: Cell<i32>,
        /// Hover callback, late-bound by the caller once the list it highlights
        /// exists.
        pub(super) on_hover: RefCell<Option<HoverFn>>,
        /// Activation callback, late-bound like `on_hover`; fired with a clicked
        /// hash's `change_id` hex so the caller can open its origin commit.
        pub(super) on_activate: RefCell<Option<ActivateFn>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BlameColumn {
        const NAME: &'static str = "CommeditBlameColumn";
        type Type = super::BlameColumn;
        type ParentType = sourceview5::GutterRenderer;
    }

    impl ObjectImpl for BlameColumn {}

    impl WidgetImpl for BlameColumn {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                let w = self.width_px.get();
                (w, w, -1, -1)
            } else {
                (0, 0, -1, -1)
            }
        }
    }

    impl GutterRendererImpl for BlameColumn {
        fn query_activatable(&self, iter: &gtk::TextIter, _area: &gdk::Rectangle) -> bool {
            matches!(self.cells.borrow().get(iter.line() as usize), Some(Some(_)))
        }

        fn activate(
            &self,
            iter: &gtk::TextIter,
            _area: &gdk::Rectangle,
            _button: u32,
            _state: gdk::ModifierType,
            _n_presses: i32,
        ) {
            // Take a copy of the change id and drop the borrow *before* firing:
            // the callback re-selects the row, which refreshes and repaints this
            // column via `set_content` (a `cells` mut-borrow).
            let change = self
                .cells
                .borrow()
                .get(iter.line() as usize)
                .cloned()
                .flatten()
                .map(|c| c.change_id_hex);
            if let Some(change) = change {
                if let Some(cb) = self.on_activate.borrow().clone() {
                    cb(&change);
                }
            }
        }

        fn snapshot_line(&self, snapshot: &gtk::Snapshot, lines: &GutterLines, line: u32) {
            let cells = self.cells.borrow();
            let Some(Some(cell)) = cells.get(line as usize) else {
                return;
            };
            let obj = self.obj();
            // Shares the snapshot across all lines (not pre-translated per line),
            // so position explicitly: the line's own y, right-aligned within the
            // width and vertically centred against the line height.
            let layout = obj.view().create_pango_layout(Some(&cell.short));
            let (lw, lh) = layout.pixel_size();
            let avail = obj.width();
            let (line_y, cell_h) = lines.line_yrange(line, GutterRendererAlignmentMode::Cell);
            let x = (avail - lw - XPAD).max(0) as f32;
            let y = line_y as f32 + ((cell_h - lh) / 2).max(0) as f32;
            let hovered = self.hover.get() == line as i32;
            let color = if hovered { hover_color() } else { dim_color() };
            // Hovered cells read as clickable: underline the hash (a hyperlink
            // affordance, paired with the pointer cursor from `setup_hover`).
            if hovered {
                let attrs = gtk::pango::AttrList::new();
                attrs.insert(gtk::pango::AttrInt::new_underline(
                    gtk::pango::Underline::Single,
                ));
                layout.set_attributes(Some(&attrs));
            }
            snapshot.save();
            snapshot.translate(&gtk::graphene::Point::new(x, y));
            snapshot.append_layout(&layout, &color);
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    pub struct BlameColumn(ObjectSubclass<imp::BlameColumn>)
        @extends sourceview5::GutterRenderer, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl BlameColumn {
    pub(crate) fn new() -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().hover.set(-1);
        obj.setup_hover();
        obj
    }

    /// Late-bind the hover callback. Called once the history list it highlights
    /// exists.
    pub(crate) fn set_on_hover(&self, f: HoverFn) {
        *self.imp().on_hover.borrow_mut() = Some(f);
    }

    /// Late-bind the activation callback, fired with a clicked hash's `change_id`
    /// hex. Called once the history list it opens into exists.
    pub(crate) fn set_on_activate(&self, f: ActivateFn) {
        *self.imp().on_activate.borrow_mut() = Some(f);
    }

    /// Replace the per-line cells, resize to fit the widest hash (zero — a
    /// collapsed column — when empty), and repaint.
    pub(crate) fn set_content(&self, cells: &[Option<BlameCell>]) {
        let imp = self.imp();
        let view = self.view();
        let text_w = cells
            .iter()
            .flatten()
            .map(|c| view.create_pango_layout(Some(&c.short)).pixel_size().0)
            .max()
            .unwrap_or(0);
        let width = if text_w == 0 { 0 } else { text_w + 2 * XPAD };
        *imp.cells.borrow_mut() = cells.to_vec();
        imp.width_px.set(width);
        self.queue_resize();
        self.queue_draw();
    }

    /// The buffer line under widget-relative `y` (the gutter scrolls with the
    /// text). The view's top margin sits above buffer line 0, so subtract it to
    /// reach buffer coordinates — mirrors [`crate::diff_cues`]'s hit-test.
    fn line_at_widget_y(&self, y: f64) -> Option<i32> {
        let view = self.view();
        let vadj = view.vadjustment()?;
        let buf_y = vadj.value() as i32 + y as i32 - view.top_margin();
        let (iter, _) = view.line_at_y(buf_y);
        Some(iter.line())
    }

    /// The cell on buffer `line`, if any.
    fn cell_at(&self, line: i32) -> Option<BlameCell> {
        if line < 0 {
            return None;
        }
        self.imp()
            .cells
            .borrow()
            .get(line as usize)
            .cloned()
            .flatten()
    }

    /// Install hover handling: a tooltip per blamed line, a prelight, and the
    /// late-bound `on_hover` callback fired with the line's `change_id` (or
    /// `None` off a blamed line) so the caller can highlight the commit row.
    fn setup_hover(&self) {
        self.set_has_tooltip(true);
        self.connect_query_tooltip(|this, _x, y, _keyboard, tooltip| {
            let Some(line) = this.line_at_widget_y(y as f64) else {
                return false;
            };
            let Some(cell) = this.cell_at(line) else {
                return false;
            };
            tooltip.set_text(Some(&cell.tooltip));
            true
        });

        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion({
            let this = self.clone();
            move |_, _x, y| {
                let line = this.line_at_widget_y(y).unwrap_or(-1);
                let cell = this.cell_at(line);
                let hover_line = if cell.is_some() { line } else { -1 };
                if hover_line != this.imp().hover.get() {
                    this.imp().hover.set(hover_line);
                    this.set_cursor_from_name(Some(if hover_line >= 0 {
                        "pointer"
                    } else {
                        "default"
                    }));
                    this.queue_draw();
                    if let Some(cb) = this.imp().on_hover.borrow().clone() {
                        cb(cell.as_ref().map(|c| c.change_id_hex.as_str()));
                    }
                }
            }
        });
        motion.connect_leave({
            let this = self.clone();
            move |_| {
                if this.imp().hover.get() != -1 {
                    this.imp().hover.set(-1);
                    this.set_cursor_from_name(Some("default"));
                    this.queue_draw();
                    if let Some(cb) = this.imp().on_hover.borrow().clone() {
                        cb(None);
                    }
                }
            }
        });
        self.add_controller(motion);
    }
}

// --- Pure cell building (GTK-free, inline-tested). ---

/// Per blamed origin commit, the cell to draw on each buffer line of the diff
/// `text`. `+` lines, headers and separators get `None`; a context / `-` line
/// gets the cell for its file's old-side line, looked up in `blame` (keyed by
/// path). Stable under interactive edits: it re-derives from the live text +
/// `nums` each call, and the cached `blame` is keyed on the unchanging old side.
pub(crate) fn blame_cells(
    text: &str,
    files: &[CombinedFile],
    nums: &[DiffLineNo],
    blame: &HashMap<String, FileBlame>,
) -> Vec<Option<BlameCell>> {
    diff_old_refs(text, files, nums)
        .into_iter()
        .map(|r| {
            let (fi, old_line) = r?;
            let path = &files.get(fi)?.path;
            let fb = blame.get(path)?;
            let origin_idx = (*fb.lines.get(old_line)?)?;
            Some(BlameCell::from_info(fb.origins.get(origin_idx)?))
        })
        .collect()
}

/// For each buffer line of `text`, the `(file index in `files`, 0-based old-file
/// line)` it refers to on the old side, or `None` for `+` lines / headers /
/// separators / lines before the first file. A line belongs to the file of the
/// most recent `diff --git` separator — counted in the *live* text and matched to
/// `files` by order, the robust-to-edits scheme [`crate::diff_cues`] uses (a
/// context-line split shifts line positions but never adds/removes a separator).
fn diff_old_refs(
    text: &str,
    files: &[CombinedFile],
    nums: &[DiffLineNo],
) -> Vec<Option<(usize, usize)>> {
    let mut out = Vec::new();
    let mut cur_file: Option<usize> = None;
    let mut next = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        if line.starts_with("diff --git ") {
            cur_file = (next < files.len()).then_some(next);
            next += 1;
            out.push(None);
            continue;
        }
        let r = match (cur_file, nums.get(i).and_then(|n| n.old)) {
            (Some(fi), Some(old1)) if old1 >= 1 => Some((fi, (old1 - 1) as usize)),
            _ => None,
        };
        out.push(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linenums::diff_line_numbers;

    fn file(path: &str) -> CombinedFile {
        CombinedFile {
            path: path.to_string(),
            // start_line is unused by the blame mapping (it counts live separators).
            start_line: 0,
            editable: true,
            hunks: Vec::new(),
        }
    }

    #[test]
    fn old_refs_map_context_and_removed_lines_per_file() {
        // Two files, matched to their `diff --git` separators by order. Context and
        // `-` lines reference their old-file line (0-based); `+` lines, headers and
        // separators reference nothing.
        let text = "\
diff --git a/x b/x
@@ -1,2 +1,2 @@
 keep
-was
+now
diff --git a/y b/y
@@ -5,1 +5,1 @@
 why";
        let files = [file("x"), file("y")];
        let nums = diff_line_numbers(text);
        assert_eq!(
            diff_old_refs(text, &files, &nums),
            vec![
                None,         // diff --git x
                None,         // @@
                Some((0, 0)), // " keep"  -> file 0, old line 1
                Some((0, 1)), // "-was"   -> file 0, old line 2
                None,         // "+now"
                None,         // diff --git y
                None,         // @@
                Some((1, 4)), // " why"   -> file 1, old line 5
            ],
        );
    }

    #[test]
    fn blame_cells_resolve_only_where_blame_has_the_line() {
        // A file blamed for its one old line; a context line whose old line has no
        // blame entry stays None. (No CommitInfo needed beyond what the engine
        // builds — exercised end-to-end in the engine's blame tests.)
        let text = "\
diff --git a/x b/x
@@ -1,1 +1,1 @@
 keep";
        let files = [file("x")];
        let nums = diff_line_numbers(text);
        // Empty blame map -> no cells resolve.
        let cells = blame_cells(text, &files, &nums, &HashMap::new());
        assert_eq!(cells, vec![None, None, None]);
    }
}
