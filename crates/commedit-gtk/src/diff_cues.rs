//! The file pane's gutter column renderer and the diff-view cue geometry.
//!
//! [`GutterColumn`] is a `sourceview5::GutterRenderer` subclass that draws **one
//! gutter column**, showing per line *either* a right-aligned line number *or* a
//! centered, clickable cue glyph. On an *unnumbered* line (a `@@` header, a
//! `diff --git` separator, a conflict marker) the two never coincide, so a cue
//! there draws always. A cue on a *numbered* line — the ✄ split button on an
//! eligible context line — is **hover-reveal**: at rest the number shows, and
//! only the hovered line trades it for the button. The file gutter holds two
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

/// The split-hunk glyph. `\u{2704}` (✄, white scissors) rather than `\u{2702}`
/// (✂), which many fonts render as a colour emoji that wouldn't take the accent
/// tint the other cue glyphs get.
const SPLIT_GLYPH: &str = "\u{2704}";

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

/// Accent for the split-hunk cue (purple), distinct from expand-blue and
/// revert-amber.
fn split_color() -> gdk::RGBA {
    gdk::RGBA::parse("#8250df").expect("valid colour")
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
        /// Per-buffer-line cues; `None` where the line carries no button. A cue on
        /// an unnumbered line (a `@@`/`diff --git` header) always wins; a cue on a
        /// numbered line (the split button on a context line) is hover-reveal —
        /// only the hovered line draws it, the rest show their number.
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
            // This column's own number for the line, if any.
            let value = self.numbers.borrow().get(line as usize).and_then(|entry| {
                match self.column.get() {
                    NumColumn::Old => entry.old,
                    NumColumn::New => entry.new,
                }
            });
            // A cue on an unnumbered line (a `@@`/`diff --git` header) draws always;
            // a cue on a numbered line (a context-line split button) is hover-reveal
            // — at rest the number shows, only the hovered line trades it in.
            if let Some(Some(cue)) = self.cues.borrow().get(line as usize) {
                if value.is_none() || self.hover.get() == line as i32 {
                    self.snapshot_cue(snapshot, lines, line, cue);
                    return;
                }
            }
            if let Some(value) = value {
                self.snapshot_number(snapshot, lines, line, value);
            }
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
    /// so its top aligns with the viewport top), or `None`. The view's top margin
    /// sits above buffer line 0, so subtract it to reach buffer coordinates —
    /// `line_at_y` expects them. Skipping it makes the hit-test lead the pointer by
    /// the margin, so the cue prelights (and its cursor) a few pixels *above* the
    /// drawn button while its bottom edge reads as dead.
    fn line_at_widget_y(&self, y: f64) -> Option<i32> {
        let view = self.view();
        let vadj = view.vadjustment()?;
        let buf_y = vadj.value() as i32 + y as i32 - view.top_margin();
        let (iter, _) = view.line_at_y(buf_y);
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

/// Which sides `(counts_old, counts_new)` a diff line contributes to, classified
/// exactly like [`crate::linenums::diff_line_numbers`] so counts derived here stay
/// consistent with the numbers it reports. A context line counts to both, `+`/`-`
/// to one, and `@@`/meta/`\ No newline` lines to neither.
fn line_sides(l: &str) -> (bool, bool) {
    if l.starts_with("@@")
        || l.starts_with("diff ")
        || l.starts_with("--- ")
        || l.starts_with("+++ ")
        || l.starts_with("index ")
        || l.starts_with('\\')
    {
        (false, false)
    } else if l.starts_with('+') {
        (false, true)
    } else if l.starts_with('-') {
        (true, false)
    } else if l.starts_with(' ') {
        (true, true)
    } else {
        (false, false)
    }
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

/// Split the rendered hunk containing context `line` into two hunks at that line.
/// Returns the new buffer text and the new combined-files, or `None` when `line`
/// isn't an eligible split point (not a context line strictly between two change
/// groups within its hunk). Ephemeral: undone by the next full engine re-render.
///
/// `line` is a 0-based buffer line index (into `text.split('\n')`). The split
/// context line becomes the new second hunk's leading context; net one line is
/// added (the inserted `@@` header). Positions are re-derived from the live
/// `text` so an interactive edit that shifted lines can't misplace the cut.
///
/// Backs the gutter ✄ split button (see [`diff_cue_cells`]), which re-validates
/// here on click and no-ops on `None`.
pub(crate) fn split_hunk_at(
    text: &str,
    files: &[CombinedFile],
    line: usize,
) -> Option<(String, Vec<CombinedFile>)> {
    let lines: Vec<&str> = text.split('\n').collect();
    // The split point must land on a context line — never a `@@`/`diff --git`/meta
    // line, a `+`/`-` change line, or out of range.
    if line >= lines.len() || !lines[line].starts_with(' ') {
        return None;
    }
    // The enclosing hunk is the last `@@` header before `line`; `k` counts them.
    let k = lines[..line].iter().filter(|l| is_hunk_header(l)).count();
    if k == 0 {
        return None;
    }
    // Its live buffer position (not the possibly-stale `HunkInfo::header_line`).
    let orig_header_line = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| is_hunk_header(l))
        .nth(k - 1)
        .map(|(i, _)| i)?;
    // Map that flattened hunk index back to its owning file + slot so the metadata
    // can be rewritten in place.
    let mut owner = None;
    let mut flat_i = 0usize;
    'outer: for (fi, f) in files.iter().enumerate() {
        for hi in 0..f.hunks.len() {
            if flat_i == k - 1 {
                owner = Some((fi, hi));
                break 'outer;
            }
            flat_i += 1;
        }
    }
    let (file_idx, hunk_idx) = owner?;
    let orig = files[file_idx].hunks[hunk_idx].clone();

    // The hunk's content spans from just past its header to the next `@@`/file
    // separator, or EOF.
    let hunk_end = (orig_header_line + 1..lines.len())
        .find(|&i| is_hunk_header(lines[i]) || is_file_sep(lines[i]))
        .unwrap_or(lines.len());
    if line <= orig_header_line || line >= hunk_end {
        return None;
    }

    // Walk the content into change groups (maximal runs of `+`/`-`, separated by
    // context; `\ No newline`/meta lines are neutral and don't break a run).
    let mut group_ends: Vec<usize> = Vec::new();
    let mut cur_start: Option<usize> = None;
    let mut cur_last = 0usize;
    for (i, l) in lines
        .iter()
        .enumerate()
        .take(hunk_end)
        .skip(orig_header_line + 1)
    {
        let (o, n) = line_sides(l);
        if o && n {
            // Context line: close any open group.
            if cur_start.take().is_some() {
                group_ends.push(cur_last);
            }
        } else if o != n {
            // Change line.
            cur_start.get_or_insert(i);
            cur_last = i;
        }
        // Neutral line: leaves the current group open.
    }
    if cur_start.take().is_some() {
        group_ends.push(cur_last);
    }
    // Require a whole change group above the cut and at least one below it.
    let groups_before = group_ends.iter().filter(|&&e| e < line).count();
    if groups_before == 0 || groups_before >= group_ends.len() {
        return None;
    }

    // Derive both headers' start numbers and counts from the live line numbering,
    // so they stay correct after interactive edits. `line` is context, so it
    // carries both an old and a new number (the second hunk's starts).
    let dln = crate::linenums::diff_line_numbers(text);
    let ao = dln.get(line).and_then(|d| d.old)?;
    let an = dln.get(line).and_then(|d| d.new)?;
    // First hunk: counts up to (not including) the cut. Its start is the cut's
    // number walked back over those lines (equivalently the original header start).
    let (mut c1o, mut c1n) = (0u32, 0u32);
    for l in &lines[orig_header_line + 1..line] {
        let (o, n) = line_sides(l);
        c1o += o as u32;
        c1n += n as u32;
    }
    let a = ao.checked_sub(c1o)?;
    let c = an.checked_sub(c1n)?;
    // Second hunk: counts from the cut to the hunk end (the cut's context counts).
    let (mut c2o, mut c2n) = (0u32, 0u32);
    for l in &lines[line..hunk_end] {
        let (o, n) = line_sides(l);
        c2o += o as u32;
        c2n += n as u32;
    }

    // Preserve the original header's section-heading suffix (everything past the
    // closing `@@`, including its leading space) on the first hunk only.
    let after_first = &lines[orig_header_line][2..];
    let suffix = after_first
        .find("@@")
        .map(|i| &after_first[i + 2..])
        .unwrap_or("");
    let h1 = format!("@@ -{a},{c1o} +{c},{c1n} @@{suffix}");
    let h2 = format!("@@ -{ao},{c2o} +{an},{c2n} @@");

    // New text: rewrite the header to the first hunk's, insert the second hunk's
    // header just before the cut. Split/join on '\n' preserves trailing-newline.
    let mut out: Vec<String> = text.split('\n').map(str::to_string).collect();
    out[orig_header_line] = h1;
    out.insert(line, h2);
    let new_text = out.join("\n");

    // New metadata: everything below the inserted line shifts down by one, then the
    // split hunk is replaced by the two halves.
    let mut new_files = files.to_vec();
    for f in new_files.iter_mut().skip(file_idx + 1) {
        f.start_line += 1;
        for h in &mut f.hunks {
            h.header_line += 1;
        }
    }
    let owner_file = &mut new_files[file_idx];
    for h in owner_file.hunks.iter_mut().skip(hunk_idx + 1) {
        h.header_line += 1;
    }
    let hunk1 = HunkInfo {
        header_line: orig_header_line,
        first_group: orig.first_group,
        last_group: orig.first_group + groups_before - 1,
        can_expand_up: orig.can_expand_up,
        can_expand_down: false,
    };
    let hunk2 = HunkInfo {
        header_line: line,
        first_group: orig.first_group + groups_before,
        last_group: orig.last_group,
        can_expand_up: false,
        can_expand_down: orig.can_expand_down,
    };
    owner_file
        .hunks
        .splice(hunk_idx..hunk_idx + 1, [hunk1, hunk2]);

    Some((new_text, new_files))
}

/// The two per-line gutter columns for the diff buffer `text`: expandable `@@`
/// headers get an `↕`/`↑`/`↓` cell (column 0), revertable hunks and files a `⤺`
/// cell (column 1). Column 0 additionally carries a ✄ split cue on every context
/// line eligible for [`split_hunk_at`] (one strictly between two change groups of
/// its hunk) — hover-revealed by the renderer since those lines are numbered.
/// Paired in document order with `hunks`/`files` (robust to interactive line
/// shifts); `changes` supplies revert eligibility and `read_only` suppresses the
/// (edit-implying) revert and split cues.
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
    // Split-eligible context lines, tracked in one pass: once a change has been
    // seen in the current hunk, buffer any following context lines as `pending`; a
    // later change line confirms them (they separate two groups) and flushes them
    // to eligible. A hunk/file boundary or EOF drops whatever is still pending —
    // that's leading/trailing context, which can't split.
    let mut seen_change = false;
    let mut pending: Vec<usize> = Vec::new();
    for (i, l) in text.split('\n').enumerate() {
        if is_hunk_header(l) {
            pending.clear();
            seen_change = false;
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
            pending.clear();
            seen_change = false;
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
        } else if !read_only {
            // A content line inside a hunk (a split enables an edit/move, so gated
            // like the revert cues on `!read_only`).
            match line_sides(l) {
                // Context line: a split candidate once a change precedes it.
                (true, true) if seen_change => pending.push(i),
                (true, true) => {}
                // Change line: the pending context now separates two groups.
                (o, n) if o != n => {
                    for p in pending.drain(..) {
                        expand[p] = Some(GutterCue {
                            glyph: SPLIT_GLYPH.to_string(),
                            tooltip: "Split hunk".to_string(),
                            color: split_color(),
                        });
                    }
                    seen_change = true;
                }
                // Neutral (`\ No newline`, meta): leaves the current run intact.
                _ => {}
            }
        }
    }
    (expand, revert)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `HunkInfo` with both expand flags on, so the split's flag handling
    /// (up stays on hunk1, down on hunk2) is exercised.
    fn hunk(header_line: usize, first_group: usize, last_group: usize) -> HunkInfo {
        HunkInfo {
            header_line,
            first_group,
            last_group,
            can_expand_up: true,
            can_expand_down: true,
        }
    }

    fn file(path: &str, start_line: usize, hunks: Vec<HunkInfo>) -> CombinedFile {
        CombinedFile {
            path: path.to_string(),
            start_line,
            editable: true,
            hunks,
        }
    }

    /// The `@@` header lines of a rendered diff, in order.
    fn headers(text: &str) -> Vec<&str> {
        text.split('\n').filter(|l| is_hunk_header(l)).collect()
    }

    #[test]
    fn split_two_group_hunk_at_separating_context() {
        // Groups {-a,+A} and {-d,+D}, split at the second context line " c".
        let text = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,4 +1,4 @@
-a
+A
 b
 c
-d
+D";
        let files = vec![file("f", 0, vec![hunk(3, 0, 1)])];
        // " c" is buffer line 7.
        let (new_text, new_files) = split_hunk_at(text, &files, 7).unwrap();

        assert_eq!(
            headers(&new_text),
            vec!["@@ -1,2 +1,2 @@", "@@ -3,2 +3,2 @@"]
        );
        // The inserted header sits just before the (now shifted) split line.
        let lines: Vec<&str> = new_text.split('\n').collect();
        assert_eq!(lines[7], "@@ -3,2 +3,2 @@");
        assert_eq!(lines[8], " c");

        assert_eq!(new_files.len(), 1);
        let h = &new_files[0].hunks;
        assert_eq!(h.len(), 2);
        assert_eq!(
            (h[0].header_line, h[0].first_group, h[0].last_group),
            (3, 0, 0)
        );
        assert_eq!(
            (h[1].header_line, h[1].first_group, h[1].last_group),
            (7, 1, 1)
        );
        // Expand flags: outer edges keep the original's, the inner edges close off.
        assert_eq!((h[0].can_expand_up, h[0].can_expand_down), (true, false));
        assert_eq!((h[1].can_expand_up, h[1].can_expand_down), (false, true));
        assert_eq!(new_files[0].start_line, 0);
    }

    #[test]
    fn split_three_group_hunk_at_first_context() {
        // Groups {-a,+A}, {-c,+C}, {-e,+E}; split at the first inter-group context
        // " b" (line 6) -> hunk1 covers 1 group, hunk2 covers 2.
        let text = "\
diff --git a/g b/g
--- a/g
+++ b/g
@@ -1,5 +1,5 @@
-a
+A
 b
-c
+C
 d
-e
+E";
        let files = vec![file("g", 0, vec![hunk(3, 0, 2)])];
        let (new_text, new_files) = split_hunk_at(text, &files, 6).unwrap();

        assert_eq!(
            headers(&new_text),
            vec!["@@ -1,1 +1,1 @@", "@@ -2,4 +2,4 @@"]
        );
        let h = &new_files[0].hunks;
        assert_eq!(h.len(), 2);
        assert_eq!(
            (h[0].header_line, h[0].first_group, h[0].last_group),
            (3, 0, 0)
        );
        assert_eq!(
            (h[1].header_line, h[1].first_group, h[1].last_group),
            (6, 1, 2)
        );
    }

    #[test]
    fn split_second_file_shifts_only_below() {
        // Three files; split file b's first hunk. File a is untouched, file b's
        // later hunk and file c (header + start_line) shift down by the inserted
        // line.
        let text = "\
diff --git a/a b/a
--- a/a
+++ b/a
@@ -1,2 +1,2 @@
-p
+P
 q
diff --git a/b b/b
--- a/b
+++ b/b
@@ -1,3 +1,3 @@
-m
+M
 n
-o
+O
@@ -20,2 +20,2 @@
-r
+R
 s
diff --git a/c b/c
--- a/c
+++ b/c
@@ -1,2 +1,2 @@
-z
+Z
 w";
        let files = vec![
            file("a", 0, vec![hunk(3, 0, 0)]),
            file("b", 7, vec![hunk(10, 0, 1), hunk(16, 2, 2)]),
            file("c", 20, vec![hunk(23, 0, 0)]),
        ];
        // " n" in file b's first hunk is buffer line 13.
        let (new_text, new_files) = split_hunk_at(text, &files, 13).unwrap();

        assert_eq!(
            headers(&new_text),
            vec![
                "@@ -1,2 +1,2 @@",   // file a, untouched
                "@@ -1,1 +1,1 @@",   // file b hunk1 (split)
                "@@ -2,2 +2,2 @@",   // file b hunk2 (split)
                "@@ -20,2 +20,2 @@", // file b's original second hunk
                "@@ -1,2 +1,2 @@",   // file c, untouched
            ],
        );

        // File a: entirely untouched.
        assert_eq!(new_files[0].start_line, 0);
        assert_eq!(new_files[0].hunks[0].header_line, 3);

        // File b: unchanged separator; three hunks now, the original second one
        // bumped by the inserted line.
        assert_eq!(new_files[1].start_line, 7);
        let hb = &new_files[1].hunks;
        assert_eq!(hb.len(), 3);
        assert_eq!(
            (hb[0].header_line, hb[0].first_group, hb[0].last_group),
            (10, 0, 0)
        );
        assert_eq!(
            (hb[1].header_line, hb[1].first_group, hb[1].last_group),
            (13, 1, 1)
        );
        assert_eq!(
            (hb[2].header_line, hb[2].first_group, hb[2].last_group),
            (17, 2, 2)
        );

        // File c: separator and hunk both shift down by one.
        assert_eq!(new_files[2].start_line, 21);
        assert_eq!(new_files[2].hunks[0].header_line, 24);
    }

    #[test]
    fn ineligible_split_points_return_none() {
        // A single-group hunk with leading and trailing context.
        let text = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,3 +1,3 @@
 head
-x
+X
 tail";
        let files = vec![file("f", 0, vec![hunk(3, 0, 0)])];

        // On the `@@` header line.
        assert!(split_hunk_at(text, &files, 3).is_none());
        // On a change line ('-' then '+').
        assert!(split_hunk_at(text, &files, 5).is_none());
        assert!(split_hunk_at(text, &files, 6).is_none());
        // Leading context (before any change group).
        assert!(split_hunk_at(text, &files, 4).is_none());
        // Trailing context (after the only change group).
        assert!(split_hunk_at(text, &files, 7).is_none());
        // Out of range.
        assert!(split_hunk_at(text, &files, 999).is_none());
    }

    #[test]
    fn preserves_section_heading_suffix_on_first_hunk() {
        // The original header carries a `@@ … fn thing()` suffix; only hunk1 keeps
        // it. Also checks trailing-newline preservation.
        let text = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,4 +1,4 @@ fn thing()
-a
+A
 b
 c
-d
+D
";
        let files = vec![file("f", 0, vec![hunk(3, 0, 1)])];
        let (new_text, _) = split_hunk_at(text, &files, 7).unwrap();
        assert_eq!(
            headers(&new_text),
            vec!["@@ -1,2 +1,2 @@ fn thing()", "@@ -3,2 +3,2 @@"],
        );
        // The trailing newline survives the round-trip.
        assert!(new_text.ends_with("+D\n"));
    }

    #[test]
    fn neutral_no_newline_line_does_not_split_a_group() {
        // A `\ No newline at end of file` between the two change lines is neutral:
        // they stay one group, so the only real split is at the inter-group context.
        let text = "\
diff --git a/f b/f
--- a/f
+++ b/f
@@ -1,4 +1,3 @@
-a
\\ No newline at end of file
+A
 b
 c
-d";
        let files = vec![file("f", 0, vec![hunk(3, 0, 1)])];
        // " c" at buffer line 8 sits between the {-a,+A} run and {-d} run.
        let (new_text, new_files) = split_hunk_at(text, &files, 8).unwrap();
        // hunk1 = a,b old / A,b new; hunk2 = c,d old / c new. The `\` line is not
        // counted on either side (and doesn't break the {-a,+A} run).
        assert_eq!(
            headers(&new_text),
            vec!["@@ -1,2 +1,2 @@", "@@ -3,2 +3,1 @@"]
        );
        assert_eq!(new_files[0].hunks.len(), 2);
    }
}
