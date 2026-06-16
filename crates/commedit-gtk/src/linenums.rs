//! Diff-aware line numbers for the diff pane's `GtkSourceView`.
//!
//! Two concerns live here, mirroring the `search.rs` / `msglint.rs` shape:
//!
//! * [`diff_line_numbers`] — the **pure**, GTK-free, inline-tested core: it walks
//!   the rendered unified-diff buffer text and, seeding running old/new counters
//!   from each `@@` hunk header, reconstructs the old-side and new-side line number
//!   for every content line. No engine data is needed beyond the text the view
//!   already shows: the `@@ -a,b +c,d @@` headers carry the base offsets.
//! * [`LineNumberRenderer`] — a `sourceview5::GutterRenderer` subclass that draws
//!   one column of those numbers (old *or* new). The diff gutter holds two of them,
//!   so a context line shows `old | new`, a `-` line `old | ·`, a `+` line `· | new`.
//!
//! GTK-only, no MCP/engine counterpart (the diff viewer is a GTK affordance).

use std::cell::{Cell, RefCell};

use commedit_engine::diff::{classify_conflict_lines, ConflictLineKind, ConflictPiece};
use gtk::glib;
use gtk::subclass::prelude::*;
use sourceview5::prelude::*;
use sourceview5::subclass::prelude::*;
use sourceview5::{GutterLines, GutterRendererAlignmentMode};

use crate::state::ConflictFileView;

/// The old-side and new-side line number a single rendered diff line carries.
/// `None` where that side does not apply (e.g. `new` on a `-` line, both on a
/// `@@`/header/blank line).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DiffLineNo {
    pub old: Option<u32>,
    pub new: Option<u32>,
}

/// Which side a [`LineNumberRenderer`] column shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum NumColumn {
    Old,
    #[default]
    New,
}

/// Compute the old/new line number for every `\n`-split line of `text` (the
/// rendered diff buffer). One entry per line, so the result indexes directly by
/// `GtkTextBuffer` line number (including a trailing empty line from a final
/// newline).
///
/// Classification is by **raw leading char** rather than the engine's
/// `classify_line`, whose catch-all buckets unknown lines as context — that would
/// wrongly number synthetic lines such as the annotations `── … ──` header. Order
/// matters: `--- `/`+++ ` headers are matched before the bare `+`/`-` content arms.
pub(crate) fn diff_line_numbers(text: &str) -> Vec<DiffLineNo> {
    let mut out = Vec::new();
    let mut old: u32 = 0;
    let mut new: u32 = 0;
    for line in text.split('\n') {
        if line.starts_with("@@") {
            if let Some((o, n)) = parse_hunk_header(line) {
                old = o;
                new = n;
            }
            out.push(DiffLineNo::default());
        } else if line.starts_with("diff ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("index ")
            || line.starts_with('\\')
        {
            out.push(DiffLineNo::default());
        } else {
            match line.as_bytes().first() {
                Some(b'+') => {
                    out.push(DiffLineNo {
                        old: None,
                        new: Some(new),
                    });
                    new += 1;
                }
                Some(b'-') => {
                    out.push(DiffLineNo {
                        old: Some(old),
                        new: None,
                    });
                    old += 1;
                }
                Some(b' ') => {
                    out.push(DiffLineNo {
                        old: Some(old),
                        new: Some(new),
                    });
                    old += 1;
                    new += 1;
                }
                _ => out.push(DiffLineNo::default()),
            }
        }
    }
    out
}

/// Parse the 1-based old/new start lines out of `@@ -a,b +c,d @@`. Tolerates a
/// missing count (`@@ -a +c @@`) and any trailing section heading or appended pill.
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    let rest = line.strip_prefix("@@")?.trim_start();
    let mut parts = rest.split_whitespace();
    let old = parts
        .next()?
        .strip_prefix('-')?
        .split(',')
        .next()?
        .parse()
        .ok()?;
    let new = parts
        .next()?
        .strip_prefix('+')?
        .split(',')
        .next()?
        .parse()
        .ok()?;
    Some((old, new))
}

/// Per-buffer-line ours/theirs line numbers for the combined conflict-snippet
/// buffer, mirroring `render_conflict_view`'s assembly so it aligns 1:1 with the
/// shown lines. Ours maps to the `old` slot, theirs to `new`, so the two diff
/// [`LineNumberRenderer`]s are reused unchanged. Markers, file headers, the
/// elision cue and the structural notice get no number.
///
/// Numbers come purely from each file's materialized conflict text (no engine
/// help), the same way the diff view derives old/new from `@@` + line prefixes: a
/// line of the "ours" side advances only the ours counter, a "theirs" line only
/// theirs, an unconflicted line both. Elided runs (hidden behind a cue) still
/// advance the counters by their line count, so numbers stay continuous across an
/// expand.
pub(crate) fn conflict_line_numbers(view: &[ConflictFileView]) -> Vec<DiffLineNo> {
    let mut out = Vec::new();
    for fv in view {
        // The `─── path ───` header line.
        out.push(DiffLineNo::default());
        if !fv.resolvable {
            // The structural-conflict notice line (no snippet, no pieces).
            out.push(DiffLineNo::default());
            continue;
        }
        // Number every line of the file's full conflict text.
        let mut nums: Vec<DiffLineNo> = Vec::new();
        let mut ours = 1u32;
        let mut theirs = 1u32;
        for kind in classify_conflict_lines(&fv.full_text) {
            nums.push(match kind {
                ConflictLineKind::Plain => {
                    let n = DiffLineNo {
                        old: Some(ours),
                        new: Some(theirs),
                    };
                    ours += 1;
                    theirs += 1;
                    n
                }
                ConflictLineKind::Ours => {
                    let n = DiffLineNo {
                        old: Some(ours),
                        new: None,
                    };
                    ours += 1;
                    n
                }
                ConflictLineKind::Theirs => {
                    let n = DiffLineNo {
                        old: None,
                        new: Some(theirs),
                    };
                    theirs += 1;
                    n
                }
                // Marker and base lines belong to neither side's file.
                _ => DiffLineNo::default(),
            });
        }
        // Project onto the shown snippet via the pieces recorded at render time: a
        // shown run copies its numbers; an elided run is skipped (its lines still
        // advanced the counters) and stands in the buffer as a single cue line.
        let mut cursor = 0;
        for piece in &fv.pieces {
            match piece {
                ConflictPiece::Shown { lines } => {
                    for _ in 0..*lines {
                        out.push(nums.get(cursor).copied().unwrap_or_default());
                        cursor += 1;
                    }
                }
                ConflictPiece::Elided { lines } => {
                    cursor += lines.len();
                    out.push(DiffLineNo::default());
                }
            }
        }
    }
    out
}

/// Horizontal padding around a number column, in pixels.
const XPAD: i32 = 4;

/// The dim gray used for the numbers, matching the diff's "meta" tone (`#6e7781`).
fn number_color() -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::new(0.431, 0.467, 0.506, 1.0)
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct LineNumberRenderer {
        /// Per-buffer-line numbers, indexed by line. Refreshed wholesale on every
        /// buffer change (load, splice, interactive edit).
        pub(super) numbers: RefCell<Vec<DiffLineNo>>,
        /// Which side this instance draws.
        pub(super) column: Cell<NumColumn>,
        /// Desired column width, driven by the widest number currently shown.
        pub(super) width_px: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LineNumberRenderer {
        const NAME: &'static str = "CommeditLineNumberRenderer";
        type Type = super::LineNumberRenderer;
        type ParentType = sourceview5::GutterRenderer;
    }

    impl ObjectImpl for LineNumberRenderer {}

    impl WidgetImpl for LineNumberRenderer {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            if orientation == gtk::Orientation::Horizontal {
                let w = self.width_px.get();
                (w, w, -1, -1)
            } else {
                (0, 0, -1, -1)
            }
        }
    }

    impl GutterRendererImpl for LineNumberRenderer {
        fn snapshot_line(&self, snapshot: &gtk::Snapshot, lines: &GutterLines, line: u32) {
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

            // Match the source view's monospace font by laying out through it.
            let obj = self.obj();
            let layout = obj.view().create_pango_layout(Some(&value.to_string()));
            let (lw, lh) = layout.pixel_size();
            // The snapshot is shared across all lines (not pre-translated per line),
            // so position explicitly: the line's own y within the visible area from
            // `line_yrange`, plus right-alignment within the width and vertical
            // centering against the line height.
            let avail = obj.width();
            let (line_y, cell_h) = lines.line_yrange(line, GutterRendererAlignmentMode::Cell);
            let x = (avail - lw - XPAD).max(0) as f32;
            let y = line_y as f32 + ((cell_h - lh) / 2).max(0) as f32;

            snapshot.save();
            snapshot.translate(&gtk::graphene::Point::new(x, y));
            snapshot.append_layout(&layout, &number_color());
            snapshot.restore();
        }
    }
}

glib::wrapper! {
    pub struct LineNumberRenderer(ObjectSubclass<imp::LineNumberRenderer>)
        @extends sourceview5::GutterRenderer, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl LineNumberRenderer {
    pub(crate) fn new(column: NumColumn) -> Self {
        let obj: Self = glib::Object::builder().build();
        obj.imp().column.set(column);
        obj
    }

    /// Replace the per-line numbers, resize the column to fit the widest one, and
    /// repaint. An empty `nums` collapses the column to zero width (used to blank
    /// the gutter when the view shows conflict snippets rather than a diff).
    pub(crate) fn set_numbers(&self, nums: &[DiffLineNo]) {
        let imp = self.imp();
        let col = imp.column.get();
        let max = nums
            .iter()
            .filter_map(|n| match col {
                NumColumn::Old => n.old,
                NumColumn::New => n.new,
            })
            .max()
            .unwrap_or(0);
        *imp.numbers.borrow_mut() = nums.to_vec();
        let width = if max == 0 {
            0
        } else {
            (max.ilog10() as i32 + 1) * self.digit_width() + 2 * XPAD
        };
        imp.width_px.set(width);
        self.queue_resize();
        self.queue_draw();
    }

    /// Pixel width of one digit in the view's monospace font.
    fn digit_width(&self) -> i32 {
        self.view().create_pango_layout(Some("0")).pixel_size().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nums(text: &str) -> Vec<(Option<u32>, Option<u32>)> {
        diff_line_numbers(text)
            .into_iter()
            .map(|n| (n.old, n.new))
            .collect()
    }

    #[test]
    fn context_added_removed() {
        let diff = "\
--- a/f
+++ b/f
@@ -1,3 +1,3 @@
 keep
-was
+now
 tail";
        assert_eq!(
            nums(diff),
            vec![
                (None, None),       // --- a/f
                (None, None),       // +++ b/f
                (None, None),       // @@
                (Some(1), Some(1)), // " keep"
                (Some(2), None),    // "-was"
                (None, Some(2)),    // "+now"
                (Some(3), Some(3)), // " tail"
            ],
        );
    }

    #[test]
    fn second_hunk_reseeds_counters() {
        let diff = "\
@@ -10,2 +20,2 @@
 a
 b
@@ -50,1 +60,2 @@
 c
+d";
        assert_eq!(
            nums(diff),
            vec![
                (None, None),
                (Some(10), Some(20)),
                (Some(11), Some(21)),
                (None, None),
                (Some(50), Some(60)),
                (None, Some(61)),
            ],
        );
    }

    #[test]
    fn two_files_each_reseed() {
        let diff = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1,1 +1,1 @@
 x
diff --git a/y b/y
--- a/y
+++ b/y
@@ -100,1 +100,1 @@
 y";
        assert_eq!(
            nums(diff),
            vec![
                (None, None),           // diff --git x
                (None, None),           // --- a/x
                (None, None),           // +++ b/x
                (None, None),           // @@
                (Some(1), Some(1)),     // " x"
                (None, None),           // diff --git y
                (None, None),           // --- a/y
                (None, None),           // +++ b/y
                (None, None),           // @@
                (Some(100), Some(100)), // " y"
            ],
        );
    }

    #[test]
    fn no_newline_marker_and_pill_suffixed_header() {
        // A `\ No newline` line must not consume a number; a pill appended to the
        // `@@` header must not break parsing.
        let diff = "\
@@ -1,1 +1,2 @@ ◀ revert hunk ▶
-old
\\ No newline at end of file
+new1
+new2";
        assert_eq!(
            nums(diff),
            vec![
                (None, None),    // @@ … ◀ revert hunk ▶
                (Some(1), None), // "-old"
                (None, None),    // "\ No newline…"
                (None, Some(1)), // "+new1"
                (None, Some(2)), // "+new2"
            ],
        );
    }

    #[test]
    fn empty_and_synthetic_lines_are_unnumbered() {
        // Empty diff (just a synthetic header) — nothing numbered, no panic.
        assert_eq!(nums(""), vec![(None, None)]);
        assert_eq!(nums("── annotations ──"), vec![(None, None)]);
    }

    #[test]
    fn added_file_zero_old_start() {
        let diff = "\
@@ -0,0 +1,2 @@
+line1
+line2";
        assert_eq!(
            nums(diff),
            vec![(None, None), (None, Some(1)), (None, Some(2))],
        );
    }

    // --- conflict_line_numbers ---

    use commedit_engine::diff::ContextExpansion;

    fn cfv(full_text: &str, pieces: Vec<ConflictPiece>, resolvable: bool) -> ConflictFileView {
        ConflictFileView {
            path: "f".to_string(),
            resolvable,
            marker_len: 7,
            full_text: full_text.to_string(),
            exp: ContextExpansion::default(),
            pieces,
            gaps: Vec::new(),
        }
    }

    fn conflict_nums(view: &[ConflictFileView]) -> Vec<(Option<u32>, Option<u32>)> {
        conflict_line_numbers(view)
            .into_iter()
            .map(|n| (n.old, n.new))
            .collect()
    }

    #[test]
    fn conflict_numbers_count_each_side() {
        // ours file: ctx1 ctx2 our1 ctx3 = 1..4; theirs file: ctx1 ctx2 their1
        // their2 ctx3 = 1..5. Markers and the header carry no number.
        let ft = "ctx1\nctx2\n<<<<<<<\nour1\n=======\ntheir1\ntheir2\n>>>>>>>\nctx3";
        let fv = cfv(ft, vec![ConflictPiece::Shown { lines: 9 }], true);
        assert_eq!(
            conflict_nums(&[fv]),
            vec![
                (None, None),       // ─── header ───
                (Some(1), Some(1)), // ctx1
                (Some(2), Some(2)), // ctx2
                (None, None),       // <<<<<<<
                (Some(3), None),    // our1
                (None, None),       // =======
                (None, Some(3)),    // their1
                (None, Some(4)),    // their2
                (None, None),       // >>>>>>>
                (Some(4), Some(5)), // ctx3
            ],
        );
    }

    #[test]
    fn conflict_numbers_continue_across_an_elided_run() {
        // Nine plain lines, the middle three elided behind one cue line: numbers
        // jump from 3 to 7 across the cue, staying continuous.
        let ft = (1..=9)
            .map(|n| format!("l{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let pieces = vec![
            ConflictPiece::Shown { lines: 3 },
            ConflictPiece::Elided {
                lines: vec!["l4".into(), "l5".into(), "l6".into()],
            },
            ConflictPiece::Shown { lines: 3 },
        ];
        assert_eq!(
            conflict_nums(&[cfv(&ft, pieces, true)]),
            vec![
                (None, None),       // ─── header ───
                (Some(1), Some(1)), // l1
                (Some(2), Some(2)), // l2
                (Some(3), Some(3)), // l3
                (None, None),       // ↕ elision cue (hidden l4..l6)
                (Some(7), Some(7)), // l7
                (Some(8), Some(8)), // l8
                (Some(9), Some(9)), // l9
            ],
        );
    }

    #[test]
    fn conflict_numbers_reset_per_file_and_skip_structural() {
        // First file numbered, second (structural) is just header + notice, third
        // restarts its counters at 1.
        let a = cfv("a1\na2", vec![ConflictPiece::Shown { lines: 2 }], true);
        let b = cfv("", Vec::new(), false);
        let c = cfv("c1", vec![ConflictPiece::Shown { lines: 1 }], true);
        assert_eq!(
            conflict_nums(&[a, b, c]),
            vec![
                (None, None),       // header a
                (Some(1), Some(1)), // a1
                (Some(2), Some(2)), // a2
                (None, None),       // header b
                (None, None),       // structural notice
                (None, None),       // header c
                (Some(1), Some(1)), // c1 (counters reset)
            ],
        );
    }
}
