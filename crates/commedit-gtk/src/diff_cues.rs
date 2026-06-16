//! Clickable gutter "cue" buttons for the diff and conflict views.
//!
//! [`ActivatableGutterRenderer`] is a `sourceview5::GutterRenderer` subclass that
//! draws a single glyph per qualifying line and reports clicks on it — the gutter
//! counterpart of the old inline text "pills". It is deliberately generic: it
//! knows only a per-line `(glyph, tooltip)` cell vector and an `on_activate(line)`
//! callback, so the same type drives the diff view's expand / revert columns and
//! the conflict view's resolve column. Per-line tooltips and a pointer cursor /
//! prelight on hover come for free because the renderer is itself a `gtk::Widget`.
//!
//! The action a click performs is the caller's concern: `on_activate` is handed
//! the buffer line, and the caller maps it back to a hunk / file / conflict block.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use commedit_engine::diff::{CombinedFile, FileChange, HunkInfo};
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use sourceview5::prelude::*;
use sourceview5::subclass::prelude::*;
use sourceview5::{GutterLines, GutterRendererAlignmentMode};

/// The revert glyph (matches the old "⤺ revert" pill).
const REVERT_GLYPH: &str = "\u{293a}";

/// A gutter click handler, invoked with the activated buffer line.
pub(crate) type ActivateFn = Rc<dyn Fn(u32)>;

/// One active gutter cell: the glyph to draw and its hover tooltip.
#[derive(Clone, Debug, Default)]
pub(crate) struct GutterCue {
    pub glyph: String,
    pub tooltip: String,
}

/// Horizontal padding around the glyph, in pixels.
const XPAD: i32 = 6;

mod imp {
    use super::*;

    pub struct ActivatableGutterRenderer {
        /// Per-buffer-line cells; `None` where the line carries no cue. Indexed by
        /// line, refreshed wholesale on every buffer change.
        pub(super) cells: RefCell<Vec<Option<GutterCue>>>,
        /// The glyph colour.
        pub(super) color: Cell<gdk::RGBA>,
        /// Line currently under the pointer (for the prelight), or -1.
        pub(super) hover: Cell<i32>,
        /// Desired column width, driven by the widest active glyph.
        pub(super) width_px: Cell<i32>,
        /// Click handler, late-bound by the caller (it needs render state defined
        /// after this renderer is built and inserted).
        pub(super) on_activate: RefCell<Option<ActivateFn>>,
    }

    impl Default for ActivatableGutterRenderer {
        fn default() -> Self {
            // `color` is set by `new()`; RGBA has no `Default`, so seed it here.
            Self {
                cells: RefCell::new(Vec::new()),
                color: Cell::new(gdk::RGBA::BLACK),
                hover: Cell::new(-1),
                width_px: Cell::new(0),
                on_activate: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ActivatableGutterRenderer {
        const NAME: &'static str = "CommeditActivatableGutterRenderer";
        type Type = super::ActivatableGutterRenderer;
        type ParentType = sourceview5::GutterRenderer;
    }

    impl ObjectImpl for ActivatableGutterRenderer {}

    impl WidgetImpl for ActivatableGutterRenderer {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                let w = self.width_px.get();
                (w, w, -1, -1)
            } else {
                (0, 0, -1, -1)
            }
        }
    }

    impl GutterRendererImpl for ActivatableGutterRenderer {
        fn query_activatable(&self, iter: &gtk::TextIter, _area: &gdk::Rectangle) -> bool {
            self.cells
                .borrow()
                .get(iter.line() as usize)
                .map(Option::is_some)
                .unwrap_or(false)
        }

        fn activate(
            &self,
            iter: &gtk::TextIter,
            _area: &gdk::Rectangle,
            _button: u32,
            _state: gdk::ModifierType,
            _n_presses: i32,
        ) {
            let cb = self.on_activate.borrow().clone();
            if let Some(cb) = cb {
                cb(iter.line() as u32);
            }
        }

        fn snapshot_line(&self, snapshot: &gtk::Snapshot, lines: &GutterLines, line: u32) {
            let cells = self.cells.borrow();
            let Some(Some(cue)) = cells.get(line as usize) else {
                return;
            };
            let obj = self.obj();
            let avail = obj.width();
            let (line_y, cell_h) = lines.line_yrange(line, GutterRendererAlignmentMode::Cell);

            // Prelight: a faint wash of the glyph colour behind the hovered cell, so
            // it reads as a button responding to the pointer.
            if self.hover.get() == line as i32 {
                let mut bg = self.color.get();
                bg.set_alpha(0.18);
                snapshot.append_color(
                    &bg,
                    &gtk::graphene::Rect::new(0.0, line_y as f32, avail as f32, cell_h as f32),
                );
            }

            let layout = obj.view().create_pango_layout(Some(&cue.glyph));
            let (lw, lh) = layout.pixel_size();
            let x = ((avail - lw) / 2).max(0) as f32;
            let y = line_y as f32 + ((cell_h - lh) / 2).max(0) as f32;
            snapshot.save();
            snapshot.translate(&gtk::graphene::Point::new(x, y));
            snapshot.append_layout(&layout, &self.color.get());
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    pub struct ActivatableGutterRenderer(ObjectSubclass<imp::ActivatableGutterRenderer>)
        @extends sourceview5::GutterRenderer, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ActivatableGutterRenderer {
    pub(crate) fn new(color: gtk::gdk::RGBA) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().color.set(color);
        obj.imp().hover.set(-1);
        obj.setup_hover();
        obj
    }

    /// Late-bind the click handler. Called once the render state it needs exists.
    pub(crate) fn set_on_activate(&self, f: ActivateFn) {
        *self.imp().on_activate.borrow_mut() = Some(f);
    }

    /// Replace the per-line cells, resize the column to fit the widest active glyph
    /// (zero width — a collapsed column — when nothing is active), and repaint.
    pub(crate) fn set_cells(&self, cells: &[Option<GutterCue>]) {
        let imp = self.imp();
        let glyph_w = cells
            .iter()
            .flatten()
            .map(|c| self.create_pango_layout(Some(&c.glyph)).pixel_size().0)
            .max()
            .unwrap_or(0);
        let width = if glyph_w == 0 { 0 } else { glyph_w + 2 * XPAD };
        *imp.cells.borrow_mut() = cells.to_vec();
        imp.width_px.set(width);
        self.queue_resize();
        self.queue_draw();
    }

    /// Lay out a Pango string through the parent view's monospace font.
    fn create_pango_layout(&self, text: Option<&str>) -> gtk::pango::Layout {
        self.view().create_pango_layout(text)
    }

    /// The buffer line under widget-relative `y` (the gutter scrolls with the text,
    /// so its top aligns with the viewport top), or `None`.
    fn line_at_widget_y(&self, y: f64) -> Option<i32> {
        let view = self.view();
        let vadj = view.vadjustment()?;
        let (iter, _) = view.line_at_y(vadj.value() as i32 + y as i32);
        Some(iter.line())
    }

    /// Whether buffer `line` carries an active cell.
    fn is_active(&self, line: i32) -> bool {
        line >= 0
            && self
                .imp()
                .cells
                .borrow()
                .get(line as usize)
                .map(Option::is_some)
                .unwrap_or(false)
    }

    /// Install hover handling: a prelight + pointer cursor over an active cell, and
    /// a per-line tooltip read from the cell.
    fn setup_hover(&self) {
        self.set_has_tooltip(true);
        self.connect_query_tooltip(|this, _x, y, _keyboard, tooltip| {
            let Some(line) = this.line_at_widget_y(y as f64) else {
                return false;
            };
            let cells = this.imp().cells.borrow();
            let Some(Some(cue)) = cells.get(line as usize) else {
                return false;
            };
            tooltip.set_text(Some(&cue.tooltip));
            true
        });

        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion({
            let this = self.clone();
            move |_, _x, y| {
                let line = this.line_at_widget_y(y).filter(|&l| this.is_active(l));
                let new = line.unwrap_or(-1);
                if new != this.imp().hover.get() {
                    this.imp().hover.set(new);
                    this.set_cursor_from_name(Some(if new >= 0 { "pointer" } else { "default" }));
                    this.queue_draw();
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
                }
            }
        });
        self.add_controller(motion);
    }
}

// --- Diff-view cue geometry (pure; shared by the cell builder and the click
// resolution so the drawn buttons and what a click does always agree). ---

/// The change groups + owning file for the diff's `@@` headers, in document order.
fn flatten_hunks(files: &[CombinedFile]) -> Vec<(&CombinedFile, &HunkInfo)> {
    files
        .iter()
        .flat_map(|f| f.hunks.iter().map(move |h| (f, h)))
        .collect()
}

/// How many lines matching `pred` precede buffer `line`, *if* `line` itself
/// matches — i.e. its index among same-kind lines. Re-derived from the live text
/// so it survives interactive edits that shift line positions.
fn order_of(text: &str, line: usize, pred: impl Fn(&str) -> bool) -> Option<usize> {
    let mut k = 0;
    for (i, l) in text.split('\n').enumerate() {
        if i == line {
            return pred(l).then_some(k);
        }
        if pred(l) {
            k += 1;
        }
    }
    None
}

fn is_hunk_header(l: &str) -> bool {
    l.starts_with("@@")
}

fn is_file_sep(l: &str) -> bool {
    l.starts_with("diff --git ")
}

/// A *hunk* revert is a partial content reversal — meaningful only for a modified
/// file, where both sides exist as text and the hunk's `-`/`+` groups can drop
/// back to the old side.
fn revert_hunk_ok(change: &FileChange) -> bool {
    change.old_text.is_some() && change.new_text.is_some()
}

/// A *file* revert drops a file's whole change (modify→unmodify, add→delete,
/// remove→restore). Eligible wherever the two sides differ as text (not a
/// mode-only change) and the file is editable as text (not binary / conflicted).
fn revert_file_ok(change: &FileChange) -> bool {
    !change.is_binary
        && !change.conflicted_base
        && change.old_text.as_deref() != change.new_text.as_deref()
}

/// The (first_group, last_group, path) for the `@@` header at buffer `line`, or
/// `None` if `line` is not a hunk header. Used to resolve an expand / revert-hunk
/// click; expand and revert-hunk share the same group range and file.
pub(crate) fn hunk_target(
    text: &str,
    files: &[CombinedFile],
    line: usize,
) -> Option<(usize, usize, String)> {
    let k = order_of(text, line, is_hunk_header)?;
    let flat = flatten_hunks(files);
    let (f, h) = flat.get(k)?;
    Some((h.first_group, h.last_group, f.path.clone()))
}

/// The file path whose `diff --git` separator is at buffer `line`, or `None`.
/// Used to resolve a revert-file click.
pub(crate) fn file_target(text: &str, files: &[CombinedFile], line: usize) -> Option<String> {
    let k = order_of(text, line, is_file_sep)?;
    files.get(k).map(|f| f.path.clone())
}

/// The two per-line gutter columns for the diff buffer `text`: expandable `@@`
/// headers get an `↕`/`↑`/`↓` cell (column 0), revertable hunks and files a `⤺`
/// cell (column 1). Paired in document order with `hunks`/`files` (robust to
/// interactive line shifts); `changes` supplies revert eligibility and
/// `read_only` suppresses the (edit-implying) revert cues.
pub(crate) fn diff_cue_cells(
    text: &str,
    files: &[CombinedFile],
    changes: &[FileChange],
    read_only: bool,
) -> (Vec<Option<GutterCue>>, Vec<Option<GutterCue>>) {
    let n = text.split('\n').count();
    let mut expand = vec![None; n];
    let mut revert = vec![None; n];
    let change_for = |path: &str| changes.iter().find(|c| c.path == path);

    let flat = flatten_hunks(files);
    let mut hunk_k = 0;
    let mut file_k = 0;
    for (i, l) in text.split('\n').enumerate() {
        if is_hunk_header(l) {
            if let Some((f, h)) = flat.get(hunk_k) {
                let glyph = match (h.can_expand_up, h.can_expand_down) {
                    (true, true) => Some("\u{2195}"),  // ↕
                    (true, false) => Some("\u{2191}"), // ↑
                    (false, true) => Some("\u{2193}"), // ↓
                    (false, false) => None,
                };
                if let Some(g) = glyph {
                    expand[i] = Some(GutterCue {
                        glyph: g.to_string(),
                        tooltip: "Expand context".to_string(),
                    });
                }
                if !read_only && f.editable && change_for(&f.path).is_some_and(revert_hunk_ok) {
                    revert[i] = Some(GutterCue {
                        glyph: REVERT_GLYPH.to_string(),
                        tooltip: "Revert hunk".to_string(),
                    });
                }
            }
            hunk_k += 1;
        } else if is_file_sep(l) {
            if let Some(f) = files.get(file_k) {
                if !read_only && change_for(&f.path).is_some_and(revert_file_ok) {
                    revert[i] = Some(GutterCue {
                        glyph: REVERT_GLYPH.to_string(),
                        tooltip: "Revert file".to_string(),
                    });
                }
            }
            file_k += 1;
        }
    }
    (expand, revert)
}
