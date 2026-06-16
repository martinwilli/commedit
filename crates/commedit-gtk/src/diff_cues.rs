//! The file pane's gutter column renderer and the diff-view cue geometry.
//!
//! [`GutterColumn`] is a `sourceview5::GutterRenderer` subclass that draws **one
//! gutter column**, showing per line *either* a right-aligned line number *or* a
//! centered, clickable cue glyph — the two never coincide (cue lines, such as a
//! `@@` header or a conflict marker, carry no number). The file gutter holds two
//! of them (old|new / ours|theirs); each is fed a per-line number map (it picks
//! its own slot) plus a per-line cue map, so the action buttons sit at the same
//! level as the line numbers instead of in extra columns.
//!
//! A cue carries its own colour and tooltip; per-line tooltips and a pointer
//! cursor / prelight on hover come for free because the renderer is itself a
//! `gtk::Widget`. The action a click performs is the caller's concern:
//! `on_activate` is handed the buffer line, and the caller maps it back to a
//! hunk / file / conflict block (see [`hunk_target`] / [`file_target`] and the
//! conflict-cue helpers in `conflict.rs`).

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

use crate::linenums::{DiffLineNo, NumColumn};

/// The revert glyph (matches the old "⤺ revert" pill).
const REVERT_GLYPH: &str = "\u{293a}";

/// Horizontal padding around a cue glyph, in pixels.
const CUE_XPAD: i32 = 6;
/// Horizontal padding around a line number, in pixels.
const NUM_XPAD: i32 = 4;

/// A gutter click handler, invoked with the activated buffer line.
pub(crate) type ActivateFn = Rc<dyn Fn(u32)>;

/// One active gutter cell: the glyph to draw, its hover tooltip, and its colour.
#[derive(Clone, Debug)]
pub(crate) struct GutterCue {
    pub glyph: String,
    pub tooltip: String,
    pub color: gdk::RGBA,
}

/// The dim gray used for line numbers, matching the diff's "meta" tone (`#6e7781`).
fn number_color() -> gdk::RGBA {
    gdk::RGBA::new(0.431, 0.467, 0.506, 1.0)
}

/// Accent for the expand-context cue (GitHub diff blue). Also used for the
/// conflict view's elision cue.
pub(crate) fn expand_color() -> gdk::RGBA {
    gdk::RGBA::parse("#0550ae").expect("valid colour")
}

/// Accent for the revert cue (amber).
pub(crate) fn revert_color() -> gdk::RGBA {
    gdk::RGBA::parse("#9a6700").expect("valid colour")
}

/// How much larger than the view's body font a cue glyph is drawn — a button
/// reads as a control, not as text, so it is bumped up a notch.
const CUE_SCALE: f64 = 1.08;

/// Upward nudge (px) applied to the glyph: the arrow/check ink sits low in its
/// Pango logical box (which reserves descent space), so plain centering reads as
/// slightly too low.
const CUE_GLYPH_RISE: f32 = 1.4;

/// Lay out a cue glyph through the view's font, enlarged by [`CUE_SCALE`]. Shared
/// by the width measurement and the draw so the button is sized to fit its glyph.
fn cue_layout(view: &sourceview5::View, glyph: &str) -> gtk::pango::Layout {
    let layout = view.create_pango_layout(Some(glyph));
    if let Some(mut fd) = view.pango_context().font_description() {
        let size = fd.size();
        if size > 0 {
            fd.set_size((size as f64 * CUE_SCALE) as i32);
            layout.set_font_description(Some(&fd));
        }
    }
    layout
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct GutterColumn {
        /// Per-buffer-line numbers, indexed by line; this column draws its own
        /// (old or new) slot. Refreshed wholesale on every buffer change.
        pub(super) numbers: RefCell<Vec<DiffLineNo>>,
        /// Per-buffer-line cues; `None` where the line carries no button. A cue
        /// line never also carries a number, so a cue takes precedence when drawn.
        pub(super) cues: RefCell<Vec<Option<GutterCue>>>,
        /// Which number slot this column draws.
        pub(super) column: Cell<NumColumn>,
        /// Line currently under the pointer (for the prelight), or -1.
        pub(super) hover: Cell<i32>,
        /// Desired column width, the larger of the widest number and widest glyph.
        pub(super) width_px: Cell<i32>,
        /// Click handler, late-bound by the caller (it needs render state defined
        /// after this renderer is built and inserted).
        pub(super) on_activate: RefCell<Option<ActivateFn>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GutterColumn {
        const NAME: &'static str = "CommeditGutterColumn";
        type Type = super::GutterColumn;
        type ParentType = sourceview5::GutterRenderer;
    }

    impl ObjectImpl for GutterColumn {}

    impl WidgetImpl for GutterColumn {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                let w = self.width_px.get();
                (w, w, -1, -1)
            } else {
                (0, 0, -1, -1)
            }
        }
    }

    impl GutterRendererImpl for GutterColumn {
        fn query_activatable(&self, iter: &gtk::TextIter, _area: &gdk::Rectangle) -> bool {
            self.cues
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
            // A cue line carries no number, so a present cue wins outright.
            if let Some(Some(cue)) = self.cues.borrow().get(line as usize) {
                self.snapshot_cue(snapshot, lines, line, cue);
                return;
            }
            let nums = self.numbers.borrow();
            let Some(entry) = nums.get(line as usize) else {
                return;
            };
            let value = match self.column.get() {
                NumColumn::Old => entry.old,
                NumColumn::New => entry.new,
            };
            let Some(value) = value else {
                return;
            };
            self.snapshot_number(snapshot, lines, line, value);
        }
    }

    impl GutterColumn {
        /// Draw a right-aligned, dim line number on `line`.
        fn snapshot_number(
            &self,
            snapshot: &gtk::Snapshot,
            lines: &GutterLines,
            line: u32,
            n: u32,
        ) {
            let obj = self.obj();
            let layout = obj.view().create_pango_layout(Some(&n.to_string()));
            let (lw, lh) = layout.pixel_size();
            // The snapshot is shared across all lines (not pre-translated per line),
            // so position explicitly: the line's own y within the visible area from
            // `line_yrange`, plus right-alignment within the width and vertical
            // centering against the line height.
            let avail = obj.width();
            let (line_y, cell_h) = lines.line_yrange(line, GutterRendererAlignmentMode::Cell);
            let x = (avail - lw - NUM_XPAD).max(0) as f32;
            let y = line_y as f32 + ((cell_h - lh) / 2).max(0) as f32;
            snapshot.save();
            snapshot.translate(&gtk::graphene::Point::new(x, y));
            snapshot.append_layout(&layout, &number_color());
            snapshot.restore();
        }

        /// Draw a clickable cue button on `line`: a rounded, tinted box outlined in
        /// the accent colour with the enlarged glyph centered in it, intensifying
        /// when hovered so it clearly reads as a pressable control.
        fn snapshot_cue(
            &self,
            snapshot: &gtk::Snapshot,
            lines: &GutterLines,
            line: u32,
            cue: &GutterCue,
        ) {
            let obj = self.obj();
            let view = obj.view();
            let avail = obj.width();
            let (line_y, cell_h) = lines.line_yrange(line, GutterRendererAlignmentMode::Cell);
            let hovered = self.hover.get() == line as i32;

            // The button box: inset from the cell so neighbouring rows' buttons
            // don't touch, with rounded corners.
            let inset_x = 2.0_f32;
            let inset_y = 1.5_f32;
            let bx = inset_x;
            let by = line_y as f32 + inset_y;
            let bw = (avail as f32 - 2.0 * inset_x).max(1.0);
            let bh = (cell_h as f32 - 2.0 * inset_y).max(1.0);
            let rect = gtk::graphene::Rect::new(bx, by, bw, bh);
            let rrect = gtk::gsk::RoundedRect::from_rect(rect, 4.0);

            // Fill: a tint of the accent, deepening on hover.
            let mut fill = cue.color;
            fill.set_alpha(if hovered { 0.30 } else { 0.12 });
            snapshot.push_rounded_clip(&rrect);
            snapshot.append_color(&fill, &rect);
            snapshot.pop();

            // Border: the accent itself, solid on hover, softened at rest.
            let mut border = cue.color;
            border.set_alpha(if hovered { 1.0 } else { 0.6 });
            let bwidth = if hovered { 1.5 } else { 1.0 };
            snapshot.append_border(&rrect, &[bwidth; 4], &[border; 4]);

            // Glyph: enlarged, in the accent colour, centered in the button and
            // nudged up so the arrow/check ink looks optically centred.
            let layout = cue_layout(&view, &cue.glyph);
            let (lw, lh) = layout.pixel_size();
            let x = bx + ((bw - lw as f32) / 2.0).max(0.0);
            let y = by + ((bh - lh as f32) / 2.0 - CUE_GLYPH_RISE);
            snapshot.save();
            snapshot.translate(&gtk::graphene::Point::new(x, y));
            snapshot.append_layout(&layout, &cue.color);
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    pub struct GutterColumn(ObjectSubclass<imp::GutterColumn>)
        @extends sourceview5::GutterRenderer, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl GutterColumn {
    pub(crate) fn new(column: NumColumn) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().column.set(column);
        obj.imp().hover.set(-1);
        obj.setup_hover();
        obj
    }

    /// Late-bind the click handler. Called once the render state it needs exists.
    pub(crate) fn set_on_activate(&self, f: ActivateFn) {
        *self.imp().on_activate.borrow_mut() = Some(f);
    }

    /// Replace this column's per-line numbers and cues, resize it to fit the wider
    /// of the widest number and the widest glyph (zero — a collapsed column — when
    /// it holds neither), and repaint.
    pub(crate) fn set_content(&self, numbers: &[DiffLineNo], cues: &[Option<GutterCue>]) {
        let imp = self.imp();
        let col = imp.column.get();
        let max_num = numbers
            .iter()
            .filter_map(|n| match col {
                NumColumn::Old => n.old,
                NumColumn::New => n.new,
            })
            .max()
            .unwrap_or(0);
        let num_w = if max_num == 0 {
            0
        } else {
            (max_num.ilog10() as i32 + 1) * self.digit_width() + 2 * NUM_XPAD
        };
        let glyph_w = cues
            .iter()
            .flatten()
            .map(|c| cue_layout(&self.view(), &c.glyph).pixel_size().0)
            .max()
            .unwrap_or(0);
        let cue_w = if glyph_w == 0 {
            0
        } else {
            glyph_w + 2 * CUE_XPAD
        };
        *imp.numbers.borrow_mut() = numbers.to_vec();
        *imp.cues.borrow_mut() = cues.to_vec();
        imp.width_px.set(num_w.max(cue_w));
        self.queue_resize();
        self.queue_draw();
    }

    /// Pixel width of one digit in the view's monospace font.
    fn digit_width(&self) -> i32 {
        self.view().create_pango_layout(Some("0")).pixel_size().0
    }

    /// The buffer line under widget-relative `y` (the gutter scrolls with the text,
    /// so its top aligns with the viewport top), or `None`.
    fn line_at_widget_y(&self, y: f64) -> Option<i32> {
        let view = self.view();
        let vadj = view.vadjustment()?;
        let (iter, _) = view.line_at_y(vadj.value() as i32 + y as i32);
        Some(iter.line())
    }

    /// Whether buffer `line` carries an active (clickable) cue.
    fn is_active(&self, line: i32) -> bool {
        line >= 0
            && self
                .imp()
                .cues
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
            let cues = this.imp().cues.borrow();
            let Some(Some(cue)) = cues.get(line as usize) else {
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
                        color: expand_color(),
                    });
                }
                if !read_only && f.editable && change_for(&f.path).is_some_and(revert_hunk_ok) {
                    revert[i] = Some(GutterCue {
                        glyph: REVERT_GLYPH.to_string(),
                        tooltip: "Revert hunk".to_string(),
                        color: revert_color(),
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
                        color: revert_color(),
                    });
                }
            }
            file_k += 1;
        }
    }
    (expand, revert)
}
