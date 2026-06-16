//! Diff- and conflict-aware line numbers for the file pane's `GtkSourceView`.
//!
//! These are the **pure**, GTK-free, inline-tested cores that map buffer text to
//! per-line old/new numbers; the gutter renderer that draws them — sharing each
//! column with the clickable cue buttons — lives in [`crate::diff_cues`].
//!
//! * [`diff_line_numbers`] — walks the rendered unified-diff buffer text and,
//!   seeding running old/new counters from each `@@` hunk header, reconstructs the
//!   old-side and new-side line number for every content line. No engine data is
//!   needed beyond the text the view already shows: the `@@ -a,b +c,d @@` headers
//!   carry the base offsets.
//! * [`conflict_line_numbers`] — the conflict-view analogue: ours/theirs numbers
//!   derived from each file's materialized conflict text and projected onto the
//!   shown snippet via the recorded pieces.
//!
//! GTK-only, no MCP/engine counterpart (the diff viewer is a GTK affordance).

use commedit_engine::diff::{classify_conflict_lines, ConflictLineKind, ConflictPiece};

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
