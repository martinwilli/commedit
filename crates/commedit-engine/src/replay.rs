//! Asymmetric patch replay, for auto-resolving *spurious* reorder conflicts.
//!
//! When jj rebases a reordered commit it uses a symmetric 3-way tree merge, which
//! conflicts whenever two commits touch *adjacent but independent* lines — even
//! though the combined result is order-independent. Concretely:
//!
//! ```text
//!   base:  foo
//!   C1:    foo / +bar
//!   C2:    foo / bar / +baz
//! ```
//!
//! Reorder C2 before C1 and jj reads "bar is absent in the new base" as a deletion
//! competing with C2's "+baz next to bar", so the intermediate conflicts — yet the
//! final tip is unchanged.
//!
//! [`replay_change`] resolves that spurious case the way a human reads it: it takes
//! the change `base → theirs` and replays it onto `ours`, **trusting `ours` for
//! context** and only layering in `theirs`'s net edits. This asymmetry is exactly
//! what a symmetric 3-way merge (jj, git, diff3) can't do. A genuine overlap (both
//! sides rewrite the same base lines to different content) returns `None`, which
//! the caller hands back to manual resolution.
//!
//! The engine uses it in two directions, both `(base → theirs)` onto `ours`:
//! - **forward** — apply a commit's introduced change onto a rebased parent
//!   (`base = old parent`, `theirs = commit`, `ours = new parent`);
//! - **peel** — remove a commit's change from a tree above it, to reconstruct the
//!   tree below (`base = commit`, `theirs = old parent`, `ours = tree above`).

use similar::{DiffOp, TextDiff};

/// Replay the change `base → theirs` onto `ours`, trusting `ours` for context.
///
/// Returns the reconstructed text, or `None` when an edit `theirs` makes overlaps
/// a base region `ours` rewrote to *different* content (a real conflict that can't
/// be replayed). Pure insertions by `theirs` are always relocatable and never
/// conflict; a base line `theirs` deletes that `ours` also deleted is a no-op
/// (same change), not a conflict.
///
/// Output is newline-normalized (non-empty results end in `\n`), matching
/// [`crate::diff::apply_patch`]. A `base`/`ours`/`theirs` without a trailing
/// newline therefore round-trips to one *with* a trailing newline; the caller's
/// tip-equality gate rejects the rare case where that matters, falling back to
/// manual resolution.
pub fn replay_change(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let base_lines: Vec<&str> = base.lines().collect();
    let ours_lines: Vec<&str> = ours.lines().collect();
    let theirs_lines: Vec<&str> = theirs.lines().collect();

    // How `ours` changed each base line, indexed by base line:
    //   `Some(j)` — unchanged, sitting at `ours` line `j` (an Equal region);
    //   `None` with `deleted[i] = true`  — `ours` deleted it;
    //   `None` with `deleted[i] = false` — `ours` rewrote it (Replace).
    let mut base_to_ours: Vec<Option<usize>> = vec![None; base_lines.len()];
    let mut deleted_in_ours: Vec<bool> = vec![false; base_lines.len()];
    // The `ours` position to splice theirs-content in *before*, for content
    // anchored before base line `a` (`a` in `0..=base_len`, where `base_len`
    // anchors at the end of `ours`). A kept base line anchors at its own `ours`
    // position; a deleted or rewritten one anchors at where it sat, so an
    // insertion lands at the right spot even when `ours` changed the anchor line.
    let mut anchor: Vec<usize> = vec![ours_lines.len(); base_lines.len() + 1];
    for op in TextDiff::from_lines(base, ours).ops() {
        match *op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for k in 0..len {
                    base_to_ours[old_index + k] = Some(new_index + k);
                    anchor[old_index + k] = new_index + k;
                }
            }
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => {
                for k in 0..old_len {
                    deleted_in_ours[old_index + k] = true;
                    anchor[old_index + k] = new_index;
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                ..
            } => {
                // A base region `ours` rewrote to other content (`base_to_ours`
                // stays `None`, `deleted` stays `false`); anchor it at the start
                // of the replacement.
                for k in 0..old_len {
                    anchor[old_index + k] = new_index;
                }
            }
            DiffOp::Insert { .. } => {} // ours-only lines; consume no base index
        }
    }

    let anchor_in_ours = |a: usize| -> usize { anchor[a] };

    // Edits expressed in `ours` coordinates.
    let mut drop_line = vec![false; ours_lines.len()];
    // (position to insert before, lines) — kept in theirs order, stably.
    let mut inserts: Vec<(usize, &[&str])> = Vec::new();

    // Mark every base line in `[a, a+len)` (which `theirs` deletes or replaces) for
    // removal from `ours`, or bail if `ours` rewrote it to something else.
    let drop_base_range = |a: usize, len: usize, drop_line: &mut Vec<bool>| -> Option<()> {
        for i in a..a + len {
            match base_to_ours[i] {
                Some(j) => drop_line[j] = true,  // ours kept it -> drop it
                None if deleted_in_ours[i] => {} // ours already dropped it -> no-op
                None => return None,             // ours rewrote it -> real conflict
            }
        }
        Some(())
    };

    for op in TextDiff::from_lines(base, theirs).ops() {
        match *op {
            DiffOp::Equal { .. } => {}
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                let pos = anchor_in_ours(old_index);
                inserts.push((pos, &theirs_lines[new_index..new_index + new_len]));
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                drop_base_range(old_index, old_len, &mut drop_line)?;
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                drop_base_range(old_index, old_len, &mut drop_line)?;
                let pos = anchor_in_ours(old_index);
                inserts.push((pos, &theirs_lines[new_index..new_index + new_len]));
            }
        }
    }

    // Stable sort keeps multiple insertions at the same anchor in theirs order.
    inserts.sort_by_key(|(pos, _)| *pos);

    let mut out: Vec<&str> = Vec::new();
    let mut next = 0;
    for pos in 0..=ours_lines.len() {
        while next < inserts.len() && inserts[next].0 == pos {
            out.extend_from_slice(inserts[next].1);
            next += 1;
        }
        if pos < ours_lines.len() && !drop_line[pos] {
            out.push(ours_lines[pos]);
        }
    }

    if out.is_empty() {
        Some(String::new())
    } else {
        Some(format!("{}\n", out.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::replay_change;

    #[test]
    fn adjacent_independent_insertions_resolve() {
        // The canonical spurious-reorder case, forward direction: replay C2's
        // "+baz" (base C1 -> theirs C2) onto the rebased parent `ours = base`.
        let got = replay_change("foo\nbar\n", "foo\n", "foo\nbar\nbaz\n");
        assert_eq!(got.as_deref(), Some("foo\nbaz\n"));
    }

    #[test]
    fn peel_direction_removes_a_commits_change() {
        // Reconstruction "peel": remove C1's "+bar" from the tip to get the new
        // bottom commit's tree (base = C1, theirs = C1's parent, ours = tip).
        let got = replay_change("foo\nbar\n", "foo\nbar\nbaz\n", "foo\n");
        assert_eq!(got.as_deref(), Some("foo\nbaz\n"));
    }

    #[test]
    fn overlapping_edits_to_the_same_line_conflict() {
        // Both sides rewrite the middle line differently: a true conflict.
        let got = replay_change("1\n2\n3\n", "1\nA\n3\n", "1\nB\n3\n");
        assert_eq!(got, None);
    }

    #[test]
    fn theirs_pure_insert_with_ours_unrelated_edit() {
        // ours edits line 1; theirs inserts a line near the end. Independent.
        let got = replay_change("a\nb\nc\n", "A\nb\nc\n", "a\nb\nc\nd\n");
        assert_eq!(got.as_deref(), Some("A\nb\nc\nd\n"));
    }

    #[test]
    fn theirs_deletes_a_line_ours_kept() {
        let got = replay_change("a\nb\nc\n", "a\nb\nc\nd\n", "a\nc\n");
        // theirs removed `b`; ours added `d`. Net: a, c, d.
        assert_eq!(got.as_deref(), Some("a\nc\nd\n"));
    }

    #[test]
    fn both_delete_the_same_line_is_not_a_conflict() {
        // theirs deletes `b`; ours also deleted `b` (and added `x`). No conflict.
        let got = replay_change("a\nb\nc\n", "a\nc\nx\n", "a\nc\n");
        assert_eq!(got.as_deref(), Some("a\nc\nx\n"));
    }

    #[test]
    fn theirs_replaces_a_region_ours_left_alone() {
        let got = replay_change("a\nb\nc\n", "a\nb\nc\nz\n", "a\nB1\nB2\nc\n");
        // theirs replaced `b` with `B1,B2`; ours added `z` at end. Independent.
        assert_eq!(got.as_deref(), Some("a\nB1\nB2\nc\nz\n"));
    }

    #[test]
    fn theirs_replace_overlapping_ours_edit_conflicts() {
        // theirs replaces `b`; ours also rewrote `b` -> different content.
        let got = replay_change("a\nb\nc\n", "a\nOURS\nc\n", "a\nTHEIRS\nc\n");
        assert_eq!(got, None);
    }

    #[test]
    fn multi_hunk_independent_changes() {
        let base = "1\n2\n3\n4\n5\n";
        // ours edits line 2; theirs inserts after line 1 and after line 4.
        let ours = "1\nTWO\n3\n4\n5\n";
        let theirs = "1\n1b\n2\n3\n4\n4b\n5\n";
        let got = replay_change(base, ours, theirs);
        assert_eq!(got.as_deref(), Some("1\n1b\nTWO\n3\n4\n4b\n5\n"));
    }

    #[test]
    fn insert_at_start_of_file() {
        let got = replay_change("b\nc\n", "b\nc\nd\n", "a\nb\nc\n");
        assert_eq!(got.as_deref(), Some("a\nb\nc\nd\n"));
    }

    #[test]
    fn no_change_is_identity() {
        // theirs == base: nothing to apply, result is ours unchanged.
        let got = replay_change("a\nb\n", "a\nX\nb\n", "a\nb\n");
        assert_eq!(got.as_deref(), Some("a\nX\nb\n"));
    }

    #[test]
    fn theirs_makes_file_empty() {
        let got = replay_change("a\nb\n", "a\nb\n", "");
        assert_eq!(got.as_deref(), Some(""));
    }
}
