//! A scoped, content-derived "blame" for the drag-to-squash hint.
//!
//! When a single commit is dragged in the history list,
//! [`Repo::blame_single_source`] asks: do *all* the lines this commit removes
//! trace back to one single commit? If so, that commit is almost certainly where
//! the change belongs — a stronger signal than the subject-string match
//! [`crate::squash::squash_recommendations`] makes, so the UI highlights it in
//! its own colour.
//!
//! jj-lib exposes no blame/annotate API, so we compute one ourselves, narrowly:
//! starting from the dragged commit's removed lines, we walk first-parent
//! ancestry and attribute each tracked line to the commit that last introduced
//! it. [`crate::diff::commit_changes`] already yields, per file, the parent
//! version (`old_text`) and the commit version (`new_text`) — exactly the
//! per-step diff this walk consumes — and `similar` maps line positions across
//! it. Two early-outs keep it cheap: bail the instant a *second* distinct source
//! commit appears (the "single commit" answer can no longer be yes), and stop
//! once every tracked line is attributed.

use similar::{DiffOp, TextDiff};

use jj_lib::backend::CommitId;
use jj_lib::repo::Repo as _;

use crate::diff::commit_changes;
use crate::history::CommitInfo;
use crate::repo::Repo;

impl Repo {
    /// The display index of the single commit every line removed by the commit at
    /// row `from` blames to, or `None` when there is no such commit: the dragged
    /// commit is a merge (ambiguous), removes nothing, its removed lines trace to
    /// more than one commit, some line can't be attributed (a merge or history
    /// boundary on the way down), or the single source isn't a row in `commits`.
    ///
    /// Runs for *any* single dragged commit, not only autosquash-prefixed ones —
    /// the content-derived complement to [`Self::squash_recommendations`]'s name
    /// match.
    pub fn blame_single_source(&self, commits: &[CommitInfo], from: usize) -> Option<usize> {
        let src = commits.get(from)?;
        // Blaming across a merge is ambiguous: a removed line could come from
        // either side. Only a linear (single-parent) commit has one diff to read.
        if src.parents.len() != 1 {
            return None;
        }

        // The lines `src` removes, per file, as line indices into `src`'s parent
        // version of each file — the version the walk starts from.
        let changes = commit_changes(&self.repo, &src.id).ok()?;
        let mut tracking: Vec<(String, Vec<usize>)> = Vec::new();
        for fc in &changes {
            if fc.is_binary {
                continue;
            }
            let removed = removed_old_indices(
                fc.old_text.as_deref().unwrap_or(""),
                fc.new_text.as_deref().unwrap_or(""),
            );
            if !removed.is_empty() {
                tracking.push((fc.path.clone(), removed));
            }
        }
        if tracking.is_empty() {
            return None; // nothing removed — no lines to blame
        }

        // Walk first-parent ancestry. The tracked indices are always in the
        // *version* of `current` (the new side of `current`'s own diff).
        let mut current = src.parents[0].clone();
        let mut source: Option<CommitId> = None;
        loop {
            let commit = self.repo.store().get_commit(&current).ok()?;
            let cc = commit_changes(&self.repo, &current).ok()?;
            for (path, lines) in tracking.iter_mut() {
                if lines.is_empty() {
                    continue;
                }
                let Some(fc) = cc.iter().find(|fc| &fc.path == path && !fc.is_binary) else {
                    // `current` left this file untouched: identical at its parent,
                    // so the indices carry over unchanged and nothing is attributed.
                    continue;
                };
                let (introduced, remapped) = step_through(
                    fc.old_text.as_deref().unwrap_or(""),
                    fc.new_text.as_deref().unwrap_or(""),
                    lines,
                );
                if introduced {
                    match &source {
                        Some(prev) if prev != &current => return None, // a 2nd source
                        _ => source = Some(current.clone()),
                    }
                }
                *lines = remapped;
            }
            if tracking.iter().all(|(_, l)| l.is_empty()) {
                break;
            }
            // Step to the single parent; a merge or history boundary ends the walk
            // with lines still tracked, which fails the "all blame to one" test.
            let parents = commit.parent_ids();
            if parents.len() != 1 {
                break;
            }
            current = parents[0].clone();
        }

        // Every removed line must have been attributed, all to one commit that is
        // actually shown in the list.
        if tracking.iter().any(|(_, l)| !l.is_empty()) {
            return None;
        }
        let target = source?;
        commits.iter().position(|c| c.id == target)
    }
}

/// The old-side line indices `old` → `new` removes: the `-` lines of the diff,
/// i.e. the old side of every `Delete` and `Replace` op.
fn removed_old_indices(old: &str, new: &str) -> Vec<usize> {
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for op in diff.ops() {
        if let DiffOp::Delete {
            old_index, old_len, ..
        }
        | DiffOp::Replace {
            old_index, old_len, ..
        } = *op
        {
            out.extend(old_index..old_index + old_len);
        }
    }
    out
}

/// Advance `tracked` line indices (in `new`, i.e. a commit's version) back
/// through that commit's `old` → `new` diff. A line the commit carried unchanged
/// (`Equal`) is remapped to its index in `old` (the parent's version) and
/// returned; a line the commit added or rewrote (`Insert`/`Replace`) is dropped
/// and flips `introduced` to true. Returns `(introduced, remapped)`.
fn step_through(old: &str, new: &str, tracked: &[usize]) -> (bool, Vec<usize>) {
    let diff = TextDiff::from_lines(old, new);
    let ops = diff.ops();
    let mut remapped = Vec::with_capacity(tracked.len());
    let mut introduced = false;
    for &ni in tracked {
        for op in ops {
            match *op {
                DiffOp::Equal {
                    old_index,
                    new_index,
                    len,
                } if ni >= new_index && ni < new_index + len => {
                    remapped.push(old_index + (ni - new_index));
                    break;
                }
                DiffOp::Insert {
                    new_index, new_len, ..
                } if ni >= new_index && ni < new_index + new_len => {
                    introduced = true;
                    break;
                }
                DiffOp::Replace {
                    new_index, new_len, ..
                } if ni >= new_index && ni < new_index + new_len => {
                    introduced = true;
                    break;
                }
                _ => {}
            }
        }
    }
    (introduced, remapped)
}

#[cfg(test)]
mod tests {
    use super::{removed_old_indices, step_through};

    #[test]
    fn removed_indices_cover_deletes_and_replaces() {
        // old: a b c d  -> new: a X d   (delete 'b', replace 'c' with 'X')
        // similar coalesces this as a Replace of "b c" -> "X"; either way the
        // removed old-side lines are b (1) and c (2).
        let old = "a\nb\nc\nd\n";
        let new = "a\nX\nd\n";
        assert_eq!(removed_old_indices(old, new), vec![1, 2]);
        // Pure additions remove nothing.
        assert_eq!(removed_old_indices("a\n", "a\nb\n"), Vec::<usize>::new());
    }

    #[test]
    fn step_attributes_changed_lines_and_remaps_carried_ones() {
        // `current`'s diff: parent "a\nc\n" -> current "a\nb\nc\n" (inserted 'b').
        // Tracking new-line 1 ('b', introduced here) and new-line 2 ('c', carried).
        let old = "a\nc\n";
        let new = "a\nb\nc\n";
        let (introduced, remapped) = step_through(old, new, &[1, 2]);
        assert!(introduced); // 'b' was introduced by `current`
        assert_eq!(remapped, vec![1]); // 'c' maps back to old index 1
    }

    #[test]
    fn step_without_introductions_only_remaps() {
        // current didn't touch the tracked lines: 'a' (0) and 'c' (2) carry back.
        let old = "a\nb\nc\n";
        let new = "a\nb\nc\n";
        let (introduced, remapped) = step_through(old, new, &[0, 2]);
        assert!(!introduced);
        assert_eq!(remapped, vec![0, 2]);
    }
}
