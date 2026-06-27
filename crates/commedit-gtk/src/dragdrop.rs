//! Drag-and-drop wiring for the history/trash/working-copy lists: the hover-zone
//! reorder-gap and squash-target feedback, the three drag sources and two drop
//! targets, the deferred `post_drag` staging (see `run_post_drag` for why drops
//! defer their rewrite to `drag-end`), and the unprefixed-squash mode popover.
//!
//! `wire` takes the state as borrowed bundles and clones the individual handles
//! its closures capture — the same handles `build_ui` holds, so both share one
//! source of truth.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::history::{CommitInfo, ReorderMove, ReorderSetMove};
use commedit_engine::repo::Repo;
use commedit_engine::squash::{parse_squash_mode, SquashMode};
use commedit_engine::CommitId;
use gtk::prelude::*;
use gtk::{
    gdk, glib, Box as GtkBox, Button, DragSource, DropTarget, Label, ListBox, ListBoxRow,
    Orientation, Popover,
};

use crate::dnd::{DraggedCommit, DraggedCommits};
use crate::lanebranch::{BranchTip, LaneBranches};
use crate::rows::{lane_color, populate_trash};
use crate::state::{
    row_commit_gap, row_commit_index, Callbacks, Data, DisplayRow, DragOrigin, DragState,
    PendingTrashOp, PostDrag, Widgets,
};

/// Build the lane→branch map for the currently displayed `commits` from the
/// repo's editable set: each editable branch paired with its current tip (via
/// `local_branches()`), fed to [`LaneBranches::compute`]. Reads the repo fresh, so
/// it reflects the branch moves a clean save exported. A singleton editable set
/// yields a map in which no drop is ever cross-branch (see
/// [`LaneBranches::is_cross_branch`]), preserving today's single-branch behaviour.
fn lane_branches(repo: &Repo, commits: &[CommitInfo]) -> LaneBranches {
    let editable: std::collections::HashSet<String> =
        repo.editable_branches().into_iter().collect();
    let tips: Vec<BranchTip> = repo
        .local_branches()
        .into_iter()
        .filter(|b| editable.contains(&b.name))
        .map(|b| BranchTip {
            name: b.name,
            tip: b.head,
        })
        .collect();
    LaneBranches::compute(commits, &tips)
}

/// Pick the origin branch to remember for `id` when it is dropped to the trash,
/// so a later "restore to working tree" routes its changes back to that branch's
/// worktree `@` rather than always the launch one. A commit reachable from the
/// primary belongs to the launch line (restore there); a commit on *only* sibling
/// branches takes the first such sibling. `None` ⇒ no editable branch reaches it,
/// so restore falls back to the launch worktree, as before.
fn trash_origin(lb: &LaneBranches, primary: Option<&str>, id: &CommitId) -> Option<String> {
    let set = lb.branches_of(id);
    if set.is_empty() {
        return None;
    }
    if let Some(p) = primary.filter(|p| set.contains(*p)) {
        return Some(p.to_string());
    }
    set.iter().next().cloned()
}

/// The commits that identify a reorder/insert candidate's destination *line* —
/// the ones it re-parents (`new_children`), falling back to its `new_parents` for
/// a top/childless splice. [`LaneBranches`] reads the branch identity off these.
fn line_commits(mv: &ReorderMove) -> &[CommitId] {
    if mv.new_children.is_empty() {
        &mv.new_parents
    } else {
        &mv.new_children
    }
}

/// Install the drag-and-drop controllers on the history, trash and working-copy
/// lists. See the module docs.
pub(crate) fn wire(w: &Widgets, d: &Data, drag: &DragState, cb: &Callbacks) {
    // Re-bind the bundle handles to the local names the closures below clone
    // from, so the moved code is verbatim — each closure still `let x = x.clone()`s
    // exactly what it captures, and the staged `post_drag` boxes capture cloned
    // individual `Rc`s (they outlive the gesture), never a borrow of a bundle.
    let list = w.list.clone();
    let placeholder = w.placeholder.clone();
    let trash_list = w.trash_list.clone();
    let trash_scroll = w.trash_scroll.clone();
    let trash_box = w.trash_box.clone();
    let repo = d.repo.clone();
    let commits = d.commits.clone();
    let graph = d.graph.clone();
    // The interleaved display list: drag/drop work in *display* (list-row) indices
    // (drag_from/drag_set/drop_gap/drop_onto), translating to *commit* indices at
    // every planner / `commits[...]` site via `row_commit_index` / `row_commit_gap`
    // — and a working-copy `@` row, which has no commit index, drags as a fold.
    let display = d.display.clone();
    // Commit index → display-row index, for placing planner results (squash
    // recommendations, blame hint) on the right rows now that `@` nodes shift them.
    let commit_rows = d.commit_rows.clone();
    let trashed = d.trashed.clone();
    let trashed_origin = d.trashed_origin.clone();
    let pending_trash_op = d.pending_trash_op.clone();
    let selected_change = d.selected_change.clone();
    let selected_changes = d.selected_changes.clone();
    let drag_origin = drag.drag_origin.clone();
    let drag_row = drag.drag_row.clone();
    let drag_from = drag.drag_from.clone();
    let drag_set = drag.drag_set.clone();
    let drop_gap = drag.drop_gap.clone();
    let drop_onto = drag.drop_onto.clone();
    let post_drag = drag.post_drag.clone();
    let refresh = cb.refresh.clone();
    let show_status = cb.show_status.clone();
    let enter_conflict_mode = cb.enter_conflict_mode.clone();
    // Repopulating the trash after a drag must keep the restore buttons intact —
    // except off-worktree, where there is no working copy to restore into, so the
    // "restore to working tree" button is omitted (the engine refuses it too).
    let on_restore = repo
        .borrow()
        .is_worktree_bound()
        .then(|| cb.on_restore.clone());

    // This window's identity, for cross-instance drags: our process (so a drop
    // can tell our own drag from one started in another window) and our repo's
    // object-store key (so we can tell a sibling-branch window of the same repo —
    // whose commit we can cherry-pick from the shared ODB — from a foreign repo).
    let own_pid = std::process::id();
    let own_key = repo.borrow().object_store_key();
    // Set while a foreign commit is hovering this window's history list, so the
    // motion handler offers only insertion gaps (a cherry-pick, never a squash)
    // and a copy cursor. The drop itself re-derives this from the payload, so
    // correctness never rides on the flag — it only steers the hover feedback.
    let foreign_drag = Rc::new(Cell::new(false));
    // The single hovering foreign commit's sha, so `show_gap` can gate the
    // placeholder on a real cherry-pick destination — the dragged commit lives in
    // another process, so this window's `drag_*` state says nothing about it.
    let foreign_sha: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Map a y coordinate to an insertion gap: onto a row's lower half drops below
    // it; past the last row drops at the bottom; above the first, at the top. The
    // placeholder must not be inserted when this runs, or row indices are off.
    let gap_at: Rc<dyn Fn(f64) -> usize> = {
        let list = list.clone();
        let display = display.clone();
        Rc::new(move |y: f64| -> usize {
            let n = display.borrow().len();
            match list.row_at_y(y as i32) {
                Some(row) => {
                    let alloc = row.allocation();
                    let below = (y as i32) > alloc.y() + alloc.height() / 2;
                    (row.index() as usize + usize::from(below)).min(n)
                }
                None if y <= 0.0 => 0,
                None => n,
            }
        })
    };

    // Move the placeholder to the gap under `y`, but only when that gap actually
    // changes — re-inserting it on every motion event makes the rows below
    // flicker. The placeholder occupies list index == its gap, so a row hit at
    // that index is the placeholder (gap unchanged); other rows' indices are
    // mapped back past it to commit coordinates.
    let show_gap: Rc<dyn Fn(f64)> = {
        let list = list.clone();
        let placeholder = placeholder.clone();
        let commits = commits.clone();
        let display = display.clone();
        let graph = graph.clone();
        let drop_gap = drop_gap.clone();
        let repo = repo.clone();
        let drag_from = drag_from.clone();
        let drag_set = drag_set.clone();
        let drag_origin = drag_origin.clone();
        let trashed = trashed.clone();
        let foreign_drag = foreign_drag.clone();
        let foreign_sha = foreign_sha.clone();
        Rc::new(move |y: f64| {
            let n = display.borrow().len();
            let current = drop_gap.get();
            let new_gap = match list.row_at_y(y as i32) {
                Some(row) => {
                    let li = row.index() as usize;
                    if current == Some(li) {
                        return; // hovering the placeholder: the gap is unchanged
                    }
                    let alloc = row.allocation();
                    let below = (y as i32) > alloc.y() + alloc.height() / 2;
                    let ci = match current {
                        Some(g) if li > g => li - 1,
                        _ => li,
                    };
                    (ci + usize::from(below)).min(n)
                }
                None if y <= 0.0 => 0,
                None => n,
            };
            if current == Some(new_gap) {
                return;
            }
            // Only open a gap where dropping would actually move/graft the
            // commit — i.e. at least one ancestry line crossing it is a valid
            // destination. For a history drag the dragged row's own line (and a
            // merge or off-branch row) yields no candidates; for a trash drag
            // the same gate runs through the restore candidates. A foreign
            // (cross-window) drag has no row in this process, so gate it on the
            // cherry-pick candidates for the hovering commit instead.
            // The gap in commit space (working-copy rows don't count) — the space the
            // planners use.
            let to_ci = row_commit_gap(&display.borrow(), new_gap);
            let real_move = if foreign_drag.get() {
                foreign_sha
                    .borrow()
                    .as_ref()
                    .and_then(|sha| repo.borrow().lookup_commit_in_store(sha))
                    .is_some_and(|target| {
                        !repo
                            .borrow()
                            .plan_cherry_pick_candidates(
                                &commits.borrow(),
                                &graph.borrow(),
                                &target,
                                to_ci,
                            )
                            .is_empty()
                    })
            } else {
                drag_from.get().is_some_and(|from| match drag_origin.get() {
                    DragOrigin::History => {
                        let set = drag_set.borrow();
                        let commits = commits.borrow();
                        let display = display.borrow();
                        if set.len() > 1 {
                            // Multi-drag: at least one ancestry line bounded by commits
                            // outside the set must cross the gap (set holds display
                            // indices — map each to its commit).
                            let ids: HashSet<_> = set
                                .iter()
                                .filter_map(|&di| row_commit_index(&display, di))
                                .filter_map(|ci| commits.get(ci).map(|c| c.id.clone()))
                                .collect();
                            !repo
                                .borrow()
                                .plan_reorder_set_candidates(&commits, &graph.borrow(), &ids, to_ci)
                                .is_empty()
                        } else {
                            // Across the whole editable DAG, so a gap that only a
                            // cross-branch line crosses still opens.
                            match row_commit_index(&display, from) {
                                Some(from_ci) => !repo
                                    .borrow()
                                    .plan_reorder_candidates_multi(
                                        &commits,
                                        &graph.borrow(),
                                        from_ci,
                                        to_ci,
                                    )
                                    .is_empty(),
                                None => false,
                            }
                        }
                    }
                    DragOrigin::Trash => trashed.borrow().get(from).is_some_and(|info| {
                        !repo
                            .borrow()
                            .plan_restore_candidates(
                                &commits.borrow(),
                                &graph.borrow(),
                                info,
                                to_ci,
                            )
                            .is_empty()
                    }),
                    // A working-copy entry only folds *onto* a commit — never between.
                    DragOrigin::WorkingCopy => false,
                })
            };
            if !real_move {
                if placeholder.parent().is_some() {
                    list.remove(&placeholder);
                }
                drop_gap.set(None);
                return;
            }
            if placeholder.parent().is_some() {
                list.remove(&placeholder);
            }
            drop_gap.set(Some(new_gap));
            list.insert(&placeholder, new_gap as i32);
        })
    };
    let clear_gap: Rc<dyn Fn()> = {
        let list = list.clone();
        let placeholder = placeholder.clone();
        let drop_gap = drop_gap.clone();
        Rc::new(move || {
            if placeholder.parent().is_some() {
                list.remove(&placeholder);
            }
            drop_gap.set(None);
        })
    };

    // Mark the row at commit index `ci` as the active squash target (red — a
    // drop will rewrite it), if squashing the dragged row onto it is valid; clear
    // any previous target first. A no-op when `ci` is already the active target
    // (flicker guard, mirroring `show_gap`).
    let set_squash_target: Rc<dyn Fn(usize)> = {
        let list = list.clone();
        let commits = commits.clone();
        let display = display.clone();
        let repo = repo.clone();
        let drag_from = drag_from.clone();
        let drag_set = drag_set.clone();
        let drag_origin = drag_origin.clone();
        let drop_onto = drop_onto.clone();
        let trashed = trashed.clone();
        // `di` is a *display* row index (the hovered row); `drop_onto` stores it.
        Rc::new(move |di: usize| {
            if drop_onto.get() == Some(di) {
                return;
            }
            if let Some(prev) = drop_onto.get() {
                if let Some(r) = list.row_at_index(prev as i32) {
                    r.remove_css_class("squash-drop-target");
                }
            }
            // The target must be a real commit (a working-copy `@` row is never a
            // squash target); map the hovered display row to its commit index.
            let target_ci = row_commit_index(&display.borrow(), di);
            // A history drag squashes one commit onto another; a trash drag squashes
            // the trashed commit onto the commit at `di`; a working-copy drag folds
            // that uncommitted `@` into it (a fixup).
            let valid = target_ci.is_some_and(|ci| {
                drag_from.get().is_some_and(|from| match drag_origin.get() {
                    DragOrigin::History => {
                        let set = drag_set.borrow();
                        let commits = commits.borrow();
                        let display = display.borrow();
                        // Validate across the whole editable DAG (`_multi`), so a
                        // squash onto another branch's commit lights up too.
                        if set.len() > 1 {
                            // Every selected commit must fold onto the target, and the
                            // target must not be one of them (compared in display space).
                            !set.contains(&di)
                                && set.iter().all(|&sd| {
                                    row_commit_index(&display, sd).is_some_and(|si| {
                                        repo.borrow().plan_squash_multi(&commits, si, ci).is_some()
                                    })
                                })
                        } else {
                            row_commit_index(&display, from).is_some_and(|from_ci| {
                                repo.borrow()
                                    .plan_squash_multi(&commits, from_ci, ci)
                                    .is_some()
                            })
                        }
                    }
                    DragOrigin::Trash => trashed.borrow().get(from).is_some_and(|info| {
                        repo.borrow()
                            .plan_squash_restore(&commits.borrow(), info, ci)
                            .is_some()
                    }),
                    DragOrigin::WorkingCopy => {
                        matches!(display.borrow().get(from), Some(DisplayRow::Wc { entry, .. }) if {
                            repo.borrow()
                                .plan_squash_restore(&commits.borrow(), &entry.info, ci)
                                .is_some()
                        })
                    }
                })
            });
            if valid {
                if let Some(r) = list.row_at_index(di as i32) {
                    r.add_css_class("squash-drop-target");
                }
                drop_onto.set(Some(di));
            } else {
                drop_onto.set(None);
            }
        })
    };
    let clear_squash_target: Rc<dyn Fn()> = {
        let list = list.clone();
        let drop_onto = drop_onto.clone();
        Rc::new(move || {
            if let Some(prev) = drop_onto.get() {
                if let Some(r) = list.row_at_index(prev as i32) {
                    r.remove_css_class("squash-drop-target");
                }
            }
            drop_onto.set(None);
        })
    };

    // Motion dispatcher: a row's top/bottom quarter opens a reorder gap
    // (`show_gap`), its middle half marks a squash target (`set_squash_target`).
    // At most one is active at a time — switching zones clears the other's
    // visual, which also keeps the placeholder absent whenever a squash index is
    // computed, so the list-vs-commit index math stays simple.
    let show_zone: Rc<dyn Fn(f64)> = {
        let list = list.clone();
        let show_gap = show_gap.clone();
        let clear_gap = clear_gap.clone();
        let set_squash_target = set_squash_target.clone();
        let clear_squash_target = clear_squash_target.clone();
        let drop_gap = drop_gap.clone();
        let drag_origin = drag_origin.clone();
        let foreign_drag = foreign_drag.clone();
        Rc::new(move |y: f64| {
            // A commit dragged in from another window can only be cherry-picked
            // *between* commits, so it shows insertion gaps and never a squash
            // target — whatever this process's stale `drag_origin` happens to say.
            if foreign_drag.get() {
                clear_squash_target();
                show_gap(y);
                return;
            }
            // A working-copy entry can only be folded *onto* a commit (fixup), so
            // the whole row is a squash target and no reorder gap ever opens.
            let wc_drag = drag_origin.get() == DragOrigin::WorkingCopy;
            let Some(row) = list.row_at_y(y as i32) else {
                // Above the first / below the last row: a pure reorder gap (none
                // for a working-copy drag).
                clear_squash_target();
                if wc_drag {
                    clear_gap();
                } else {
                    show_gap(y);
                }
                return;
            };
            let li = row.index() as usize;
            // Hovering the placeholder itself: the gap is unchanged, leave it.
            if drop_gap.get() == Some(li) {
                return;
            }
            if wc_drag {
                // No placeholder is ever inserted for a working-copy drag, so the
                // list index is the commit index.
                clear_gap();
                set_squash_target(li);
                return;
            }
            let alloc = row.allocation();
            let local = (y as i32) - alloc.y();
            let h = alloc.height().max(1);
            if local < h / 4 || local >= h - h / 4 {
                // Edge: reorder gap.
                clear_squash_target();
                show_gap(y);
            } else {
                // Center: squash onto this commit. Map the list index past a
                // present placeholder (same rule as `show_gap`) before removing it.
                let ci = match drop_gap.get() {
                    Some(g) if li > g => li - 1,
                    _ => li,
                };
                clear_gap();
                set_squash_target(ci);
            }
        })
    };

    let drag_source = DragSource::new();
    // MOVE within this window (reorder/squash consume the source); COPY onto
    // another window (a cherry-pick that leaves our commit in place).
    drag_source.set_actions(gdk::DragAction::MOVE | gdk::DragAction::COPY);
    drag_source.connect_prepare({
        let list = list.clone();
        let display = display.clone();
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let drag_set = drag_set.clone();
        let drag_origin = drag_origin.clone();
        let commits = commits.clone();
        let own_key = own_key.clone();
        move |source, _x, y| {
            let row = list.row_at_y(y as i32)?;
            let idx = row.index() as usize;
            // A working-copy `@` node drags as a fold/discard source (carrying its
            // display index as an i32, read back from `display` at drop time); a
            // commit row drags as history (a text payload). `drag_from`/`drag_set`
            // are display indices throughout — translated to commit indices at the
            // planner sites.
            if matches!(display.borrow().get(idx), Some(DisplayRow::Wc { .. })) {
                let paintable = gtk::WidgetPaintable::new(Some(&row));
                source.set_icon(Some(&paintable), 0, 0);
                *drag_row.borrow_mut() = Some(row.clone());
                drag_set.borrow_mut().clear();
                drag_from.set(Some(idx));
                drag_origin.set(DragOrigin::WorkingCopy);
                return Some(gdk::ContentProvider::for_value(&(idx as i32).to_value()));
            }
            // A commit row. Every commit in the unified DAG is editable, so every
            // commit row is a drag source. If the grabbed row is part of a standing
            // multi-selection, drag the whole set as a group (commit rows only —
            // `@` nodes never join one); otherwise an ordinary single-commit drag.
            // Indices stay valid for the gesture (the rewrite only runs at drag-end).
            let mut selected: Vec<usize> = list
                .selected_rows()
                .iter()
                .map(|r| r.index() as usize)
                .filter(|&di| matches!(display.borrow().get(di), Some(DisplayRow::Commit(_))))
                .collect();
            selected.sort_unstable(); // ascending = newest-first (top row is newest)
            *drag_set.borrow_mut() = if selected.len() > 1 && selected.contains(&idx) {
                selected
            } else {
                Vec::new()
            };
            // Show the dragged row under the cursor for feedback.
            let paintable = gtk::WidgetPaintable::new(Some(&row));
            source.set_icon(Some(&paintable), 0, 0);
            *drag_row.borrow_mut() = Some(row.clone());
            drag_from.set(Some(idx));
            drag_origin.set(DragOrigin::History);
            // Carry the dragged commit(s) as a text payload, so a drop onto another
            // commedit window — a separate process — can cherry-pick them. An
            // in-process drop reads the very same string back; it just ignores the
            // commit list and works from the live `drag_*` state.
            let dragged: Vec<usize> = {
                let set = drag_set.borrow();
                if set.is_empty() {
                    vec![idx]
                } else {
                    set.clone()
                }
            };
            let payload = {
                let c = commits.borrow();
                let display = display.borrow();
                DraggedCommits {
                    pid: std::process::id(),
                    repo_key: own_key.clone().unwrap_or_default(),
                    branch: None,
                    commits: dragged
                        .iter()
                        .filter_map(|&di| row_commit_index(&display, di))
                        .filter_map(|ci| c.get(ci))
                        .map(|info| DraggedCommit {
                            sha: info.id_hex(),
                            change_id: info.change_id_hex(),
                            subject: info.subject.clone(),
                        })
                        .collect(),
                }
            };
            Some(gdk::ContentProvider::for_value(
                &payload.serialize().to_value(),
            ))
        }
    });
    drag_source.connect_drag_begin({
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let drag_set = drag_set.clone();
        let drag_origin = drag_origin.clone();
        let repo = repo.clone();
        let commits = commits.clone();
        let display = display.clone();
        let commit_rows = commit_rows.clone();
        let list = list.clone();
        move |_source, _drag| {
            if let Some(row) = drag_row.borrow().as_ref() {
                row.add_css_class("commit-dragging");
            }
            // Squash hints apply only to a single-commit history drag (a working-copy
            // `@` carries no subject; a multi-drag always asks via the popover).
            if drag_origin.get() != DragOrigin::History || drag_set.borrow().len() > 1 {
                return;
            }
            // Map the dragged display row to its commit index for the planners; map
            // their commit-index results back to display rows for the highlight.
            let Some(from) = drag_from
                .get()
                .and_then(|di| row_commit_index(&display.borrow(), di))
            else {
                return;
            };
            let row_of = |ci: usize| {
                commit_rows
                    .borrow()
                    .get(ci)
                    .and_then(|&di| list.row_at_index(di as i32))
            };
            // Highlight where this commit would squash: green for the real target(s),
            // yellow for other autosquash commits aimed at the same target. Empty
            // (no-op) unless the dragged commit is prefixed.
            let recs = repo
                .borrow()
                .squash_recommendations(&commits.borrow(), from);
            for i in recs.targets {
                if let Some(r) = row_of(i) {
                    r.add_css_class("squash-recommended");
                }
            }
            for i in recs.siblings {
                if let Some(r) = row_of(i) {
                    r.add_css_class("squash-sibling");
                }
            }
            // Purple: every line this commit removes blames to one single commit — a
            // content-derived "it belongs here", stronger than the name match. Wins
            // over green/yellow on the same row, so strip those first to keep the
            // colour unambiguous.
            if let Some(i) = repo.borrow().blame_single_source(&commits.borrow(), from) {
                if let Some(r) = row_of(i) {
                    r.remove_css_class("squash-recommended");
                    r.remove_css_class("squash-sibling");
                    r.add_css_class("squash-blame");
                }
            }
        }
    });
    drag_source.connect_drag_end({
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let drag_set = drag_set.clone();
        let clear_gap = clear_gap.clone();
        let clear_squash_target = clear_squash_target.clone();
        let list = list.clone();
        let post_drag = post_drag.clone();
        move |_source, _drag, _delete| {
            if let Some(row) = drag_row.borrow_mut().take() {
                row.remove_css_class("commit-dragging");
            }
            drag_from.set(None);
            drag_set.borrow_mut().clear();
            clear_gap();
            // populate_rows won't touch our highlight classes, so strip them here.
            let mut i = 0;
            while let Some(r) = list.row_at_index(i) {
                r.remove_css_class("squash-recommended");
                r.remove_css_class("squash-sibling");
                r.remove_css_class("squash-blame");
                i += 1;
            }
            clear_squash_target();
            run_post_drag(&post_drag);
        }
    });
    list.add_controller(drag_source);

    // Accept the history payload (text, so it can come from another window) and
    // the trash/working-copy row index (i32, in-process only). COPY is allowed
    // for a cross-instance cherry-pick. Preload makes the payload readable during
    // motion, so the hover feedback can adapt to a foreign drag.
    let drop_target = DropTarget::new(
        String::static_type(),
        gdk::DragAction::MOVE | gdk::DragAction::COPY,
    );
    drop_target.set_types(&[String::static_type(), i32::static_type()]);
    drop_target.set_preload(true);
    // Reading the foreign payload (preloaded) at enter/motion: light the foreign
    // flag and stash the hovering commit's sha so `show_zone`/`show_gap` show an
    // insertion gap and a copy cursor.
    let mark_foreign = {
        let foreign_drag = foreign_drag.clone();
        let foreign_sha = foreign_sha.clone();
        move |target: &DropTarget| {
            let sha = foreign_pick_sha(target, own_pid);
            foreign_drag.set(foreign_payload(target, own_pid).is_some());
            *foreign_sha.borrow_mut() = sha;
            foreign_drag.get()
        }
    };
    drop_target.connect_enter({
        let show_zone = show_zone.clone();
        let mark_foreign = mark_foreign.clone();
        move |target, _x, y| {
            let foreign = mark_foreign(target);
            show_zone(y);
            if foreign {
                gdk::DragAction::COPY
            } else {
                gdk::DragAction::MOVE
            }
        }
    });
    drop_target.connect_motion({
        let show_zone = show_zone.clone();
        let mark_foreign = mark_foreign.clone();
        move |target, _x, y| {
            let foreign = mark_foreign(target);
            show_zone(y);
            if foreign {
                gdk::DragAction::COPY
            } else {
                gdk::DragAction::MOVE
            }
        }
    });
    drop_target.connect_leave({
        let clear_gap = clear_gap.clone();
        let clear_squash_target = clear_squash_target.clone();
        let foreign_drag = foreign_drag.clone();
        let foreign_sha = foreign_sha.clone();
        move |_target| {
            foreign_drag.set(false);
            *foreign_sha.borrow_mut() = None;
            clear_gap();
            clear_squash_target();
        }
    });
    drop_target.connect_drop({
        let commits = commits.clone();
        let display = display.clone();
        let repo = repo.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let gap_at = gap_at.clone();
        let clear_gap = clear_gap.clone();
        let drop_gap = drop_gap.clone();
        let drop_onto = drop_onto.clone();
        let list = list.clone();
        let drag_origin = drag_origin.clone();
        let trashed = trashed.clone();
        let pending_trash_op = pending_trash_op.clone();
        let trash_list = trash_list.clone();
        let trash_scroll = trash_scroll.clone();
        let on_restore = on_restore.clone();
        let selected_change = selected_change.clone();
        let selected_changes = selected_changes.clone();
        let post_drag = post_drag.clone();
        let drag_set = drag_set.clone();
        let drag_from = drag_from.clone();
        let graph = graph.clone();
        let own_key = own_key.clone();
        let foreign_drag = foreign_drag.clone();
        let foreign_sha = foreign_sha.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        move |_target, value, _x, y| {
            // History drags (including from another commedit window) arrive as a
            // text payload; trash and working-copy drags as an i32 row index.
            let payload = value
                .get::<String>()
                .ok()
                .and_then(|s| DraggedCommits::parse(&s));
            // Prefer the gap the placeholder marked; fall back to the drop point.
            // `to` is a *display* gap (used to anchor the popovers at a list row);
            // `to_ci` is its commit-space gap (used by the planners).
            let to = match drop_gap.get() {
                Some(to) => to,
                None => gap_at(y),
            };
            let to_ci = row_commit_gap(&display.borrow(), to);
            clear_gap();
            foreign_drag.set(false);
            *foreign_sha.borrow_mut() = None;

            // A commit dragged from another window: copy it in (cherry-pick),
            // leaving the source window's history untouched. Same-repo only — a
            // foreign commit's objects only exist in our store when the two
            // windows share one.
            if let Some(payload) = payload.as_ref().filter(|p| p.pid != own_pid) {
                if own_key.as_deref() != Some(payload.repo_key.as_str()) {
                    show_status("Can't move commits between different repositories");
                    return true;
                }
                let [picked] = payload.commits.as_slice() else {
                    show_status("Drag a single commit at a time between windows");
                    return true;
                };
                let sha = picked.sha.clone();
                let repo = repo.clone();
                let commits = commits.clone();
                let graph = graph.clone();
                let refresh = refresh.clone();
                let show_status = show_status.clone();
                let enter_conflict_mode = enter_conflict_mode.clone();
                let list = list.clone();
                // Unlike an in-process drop, no `drag-end` fires in *this* window
                // to run a staged `post_drag` (the gesture ended in the source
                // window's process). Schedule the rewrite at idle directly — the
                // same below-GDK-event priority that lets pending drop-crossing
                // events drain before we rebuild the rows, just driven from the
                // drop itself since there is no local gesture left to ride.
                glib::idle_add_local_once(move || {
                    let Some(target) = repo.borrow().lookup_commit_in_store(&sha) else {
                        show_status("That commit isn't in this repository's object store");
                        return;
                    };
                    // The whole editable DAG is a graft target (a foreign commit
                    // can land on any visible lane), so plan across all editable
                    // heads, and label each candidate line with the branch it
                    // carries. `to_ci` is the gap in commit space.
                    let cands = repo.borrow().plan_cherry_pick_candidates_multi(
                        &commits.borrow(),
                        &graph.borrow(),
                        &target,
                        to_ci,
                    );
                    if cands.is_empty() {
                        show_status("Can't insert the commit here");
                        return;
                    }
                    let apply: Rc<dyn Fn(&ReorderMove)> = {
                        let repo = repo.clone();
                        let refresh = refresh.clone();
                        let show_status = show_status.clone();
                        let enter_conflict_mode = enter_conflict_mode.clone();
                        Rc::new(move |mv: &ReorderMove| {
                            let outcome = repo.borrow_mut().cherry_pick_commit(
                                &mv.target,
                                mv.new_parents.clone(),
                                mv.new_children.clone(),
                                None,
                            );
                            match outcome {
                                Ok(SaveOutcome::Clean) => refresh(),
                                Ok(SaveOutcome::Conflicts { commits }) => {
                                    enter_conflict_mode(commits)
                                }
                                Err(err) => show_status(&format!("Cherry-pick failed: {err}")),
                            }
                        })
                    };
                    match &cands[..] {
                        [single] => apply(&single.mv),
                        // A merge gap crosses several lines: ask which to splice
                        // into. Non-autohide (the `false`) so it never grabs the
                        // seat — a cross-window drop's compositor drag grab may
                        // still be releasing, and grabbing into that wedges all
                        // input. In-process drops show this from `drag-end` (grab
                        // already gone) and keep autohide.
                        _ => {
                            let lb = lane_branches(&repo.borrow(), &commits.borrow());
                            show_lane_popover(
                                &list,
                                to,
                                cands
                                    .into_iter()
                                    .map(|c| (c.lane, lb.label_for(line_commits(&c.mv)), c.mv))
                                    .collect(),
                                &apply,
                                false,
                            )
                        }
                    }
                });
                return true;
            }

            // In-process from here. `from` is the source row index: every drag
            // source records it in `drag_from` (the history payload doesn't carry
            // it back), and trash/working-copy also pass it as the i32 value.
            let from = if payload.is_some() {
                match drag_from.get() {
                    Some(i) => i as i32,
                    None => return false,
                }
            } else {
                match value.get::<i32>() {
                    Ok(i) => i,
                    Err(_) => return false, // unrecognized (e.g. plain text)
                }
            };
            // A center-zone hover marks a squash target; snapshot it now, since
            // `drag-end` clears it before the staged work runs.
            let onto = drop_onto.get();
            // A multi-selection dragged as a group (history only): its display
            // indices, captured at drag start. Empty for an ordinary single drag.
            let set = drag_set.borrow().clone();
            let multi = set.len() > 1;
            // Stage the work; `drag-end` runs it once the gesture is fully over
            // (rewriting history rebuilds these rows, which is unsafe mid-drag).
            match drag_origin.get() {
                DragOrigin::History if multi && onto.is_some() => {
                    // Group squash: fold every selected commit into the target. A
                    // group always asks how to merge (Fixup/Squash/Amend) — there
                    // is no single prefix to honour.
                    let onto = onto.unwrap();
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let display = display.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let selected_change = selected_change.clone();
                    let selected_changes = selected_changes.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        // Validate the target and collect the source ids (newest
                        // first) before mutating; bail if anything no longer fits.
                        // `set`/`onto` are display indices — map each to its commit.
                        let (sources, dest, dest_change) = {
                            let c = commits.borrow();
                            let disp = display.borrow();
                            let Some(onto_ci) = row_commit_index(&disp, onto) else {
                                return;
                            };
                            if set.contains(&onto)
                                || !set.iter().all(|&sd| {
                                    row_commit_index(&disp, sd).is_some_and(|si| {
                                        repo.borrow().plan_squash(&c, si, onto_ci).is_some()
                                    })
                                })
                            {
                                return;
                            }
                            let sources: Vec<_> = set
                                .iter()
                                .filter_map(|&sd| row_commit_index(&disp, sd))
                                .filter_map(|si| c.get(si).map(|x| x.id.clone()))
                                .collect();
                            let Some(dest) = c.get(onto_ci) else {
                                return;
                            };
                            (sources, dest.id.clone(), dest.change_id_hex())
                        };
                        let apply: Rc<dyn Fn(SquashMode)> = {
                            let repo = repo.clone();
                            let refresh = refresh.clone();
                            let show_status = show_status.clone();
                            let enter_conflict_mode = enter_conflict_mode.clone();
                            let selected_change = selected_change.clone();
                            let selected_changes = selected_changes.clone();
                            Rc::new(move |mode| {
                                let outcome = repo.borrow_mut().squash_into_many(
                                    sources.clone(),
                                    &dest,
                                    mode,
                                    None,
                                );
                                match outcome {
                                    Ok(SaveOutcome::Clean) => {
                                        // The sources folded away; select the target
                                        // (its change id is stable across the rewrite).
                                        *selected_change.borrow_mut() = Some(dest_change.clone());
                                        *selected_changes.borrow_mut() = vec![dest_change.clone()];
                                        refresh();
                                    }
                                    Ok(SaveOutcome::Conflicts { commits }) => {
                                        enter_conflict_mode(commits)
                                    }
                                    Err(err) => show_status(&format!("Squash failed: {err}")),
                                }
                            })
                        };
                        let Some(target_row) = list.row_at_index(onto as i32) else {
                            return;
                        };
                        show_squash_popover(&target_row, &apply);
                    }));
                    true
                }
                DragOrigin::History if multi => {
                    // Group move: relocate the whole selection to the gap, keeping
                    // their order and leaving the unselected commits in between.
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let display = display.clone();
                    let graph = graph.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        // `set` holds display indices — map each to its commit id.
                        let ids: HashSet<_> = {
                            let c = commits.borrow();
                            let disp = display.borrow();
                            set.iter()
                                .filter_map(|&sd| row_commit_index(&disp, sd))
                                .filter_map(|si| c.get(si).map(|x| x.id.clone()))
                                .collect()
                        };
                        let cands = repo.borrow().plan_reorder_set_candidates(
                            &commits.borrow(),
                            &graph.borrow(),
                            &ids,
                            to_ci,
                        );
                        if cands.is_empty() {
                            return;
                        }
                        let apply: Rc<dyn Fn(&ReorderSetMove)> = {
                            let repo = repo.clone();
                            let refresh = refresh.clone();
                            let show_status = show_status.clone();
                            let enter_conflict_mode = enter_conflict_mode.clone();
                            Rc::new(move |mv: &ReorderSetMove| {
                                let outcome = repo.borrow_mut().reorder_commits(
                                    mv.targets.clone(),
                                    mv.new_parents.clone(),
                                    mv.new_children.clone(),
                                    &mv.new_tip,
                                );
                                match outcome {
                                    Ok(SaveOutcome::Clean) => refresh(),
                                    Ok(SaveOutcome::Conflicts { commits }) => {
                                        enter_conflict_mode(commits)
                                    }
                                    Err(err) => show_status(&format!("Reorder failed: {err}")),
                                }
                            })
                        };
                        match &cands[..] {
                            [single] => apply(&single.mv),
                            // A group move stays within the branch (its in-branch
                            // lanes), so the lane picker is the colour swatch alone.
                            _ => show_lane_popover(
                                &list,
                                to,
                                cands.into_iter().map(|c| (c.lane, None, c.mv)).collect(),
                                &apply,
                                true,
                            ),
                        }
                    }));
                    true
                }
                DragOrigin::History if onto.is_some() => {
                    // Dropped ONTO a commit: squash the dragged commit into it,
                    // anywhere in the DAG — onto another branch's commit too (a
                    // squash always consumes the source, so there is no Copy/Move
                    // choice). A prefixed commit acts immediately; an unprefixed
                    // one opens a popover to pick the mode.
                    let onto = onto.unwrap();
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let display = display.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let selected_change = selected_change.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        // Map the source row and target row (display indices) to their
                        // commit indices.
                        let (from_ci, onto_ci) = {
                            let disp = display.borrow();
                            match (
                                row_commit_index(&disp, from as usize),
                                row_commit_index(&disp, onto),
                            ) {
                                (Some(f), Some(o)) => (f, o),
                                _ => return,
                            }
                        };
                        let plan =
                            repo.borrow()
                                .plan_squash_multi(&commits.borrow(), from_ci, onto_ci);
                        let Some((source, dest)) = plan else {
                            return;
                        };
                        let subject = commits.borrow()[from_ci].subject.clone();
                        // After the squash, select the drop target: its change id is
                        // stable across the rewrite, the squashed-away source's is gone.
                        let dest_change = commits.borrow()[onto_ci].change_id_hex();

                        // Run a chosen mode and report the outcome.
                        let apply: Rc<dyn Fn(SquashMode)> = {
                            let repo = repo.clone();
                            let refresh = refresh.clone();
                            let show_status = show_status.clone();
                            let enter_conflict_mode = enter_conflict_mode.clone();
                            let selected_change = selected_change.clone();
                            Rc::new(move |mode| {
                                let outcome =
                                    repo.borrow_mut().squash_into(&source, &dest, mode, None);
                                match outcome {
                                    Ok(SaveOutcome::Clean) => {
                                        *selected_change.borrow_mut() = Some(dest_change.clone());
                                        refresh();
                                    }
                                    Ok(SaveOutcome::Conflicts { commits }) => {
                                        enter_conflict_mode(commits)
                                    }
                                    Err(err) => show_status(&format!("Squash failed: {err}")),
                                }
                            })
                        };

                        match parse_squash_mode(&subject) {
                            // Prefixed: the prefix picks the mode, apply at once.
                            Some(mode) => apply(mode),
                            // Unprefixed: ask how to merge, anchored at the target.
                            None => {
                                let Some(target_row) = list.row_at_index(onto as i32) else {
                                    return;
                                };
                                show_squash_popover(&target_row, &apply);
                            }
                        }
                    }));
                    true
                }
                DragOrigin::History => {
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let display = display.clone();
                    let graph = graph.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        // Map the dragged row to its commit index (`to_ci` is the gap
                        // in commit space).
                        let Some(from_ci) = row_commit_index(&display.borrow(), from as usize)
                        else {
                            return;
                        };
                        // One candidate per ancestry line crossing the gap, across
                        // *every* editable branch's lanes (so a line on another
                        // branch is a real destination); a no-op, merge or fully
                        // off-branch drop yields none.
                        let cands = repo.borrow().plan_reorder_candidates_multi(
                            &commits.borrow(),
                            &graph.borrow(),
                            from_ci,
                            to_ci,
                        );
                        if cands.is_empty() {
                            return;
                        }
                        let source = commits.borrow()[from_ci].id.clone();
                        let lb = Rc::new(lane_branches(&repo.borrow(), &commits.borrow()));

                        // Run a Move (reparent — consume the source) and report.
                        let apply_move: Rc<dyn Fn(&ReorderMove)> = {
                            let repo = repo.clone();
                            let refresh = refresh.clone();
                            let show_status = show_status.clone();
                            let enter_conflict_mode = enter_conflict_mode.clone();
                            Rc::new(move |mv: &ReorderMove| {
                                let outcome = repo.borrow_mut().reorder_commit(
                                    &mv.target,
                                    mv.new_parents.clone(),
                                    mv.new_children.clone(),
                                    &mv.new_tip,
                                );
                                match outcome {
                                    Ok(SaveOutcome::Clean) => refresh(),
                                    Ok(SaveOutcome::Conflicts { commits }) => {
                                        enter_conflict_mode(commits)
                                    }
                                    Err(err) => show_status(&format!("Reorder failed: {err}")),
                                }
                            })
                        };
                        // Run a Copy (cherry-pick — leave the source intact) and report.
                        let apply_copy: Rc<dyn Fn(&ReorderMove)> = {
                            let repo = repo.clone();
                            let refresh = refresh.clone();
                            let show_status = show_status.clone();
                            let enter_conflict_mode = enter_conflict_mode.clone();
                            Rc::new(move |mv: &ReorderMove| {
                                let outcome = repo.borrow_mut().cherry_pick_commit(
                                    &mv.target,
                                    mv.new_parents.clone(),
                                    mv.new_children.clone(),
                                    None,
                                );
                                match outcome {
                                    Ok(SaveOutcome::Clean) => refresh(),
                                    Ok(SaveOutcome::Conflicts { commits }) => {
                                        enter_conflict_mode(commits)
                                    }
                                    Err(err) => show_status(&format!("Copy failed: {err}")),
                                }
                            })
                        };

                        // Splice the chosen line: an in-branch line moves straight
                        // away (no popover, exactly as before); a line on another
                        // branch first asks Copy vs Move (the source rides along on
                        // Move, stays put on Copy).
                        let apply: Rc<dyn Fn(&ReorderMove)> = {
                            let apply_move = apply_move.clone();
                            let apply_copy = apply_copy.clone();
                            let list = list.clone();
                            let source = source.clone();
                            let lb = lb.clone();
                            Rc::new(move |mv: &ReorderMove| {
                                let line = line_commits(mv);
                                if !lb.line_is_cross_branch(&source, line) {
                                    apply_move(mv);
                                    return;
                                }
                                // Anchor the chooser at the row bordering the gap:
                                // the row at `to`, else the last row for a bottom
                                // gap. If none is alive, fall back to a plain Move.
                                let Some(target_row) = popover_anchor_row(&list, to) else {
                                    apply_move(mv);
                                    return;
                                };
                                let dest = lb.label_for(line);
                                let on_move: Rc<dyn Fn()> = {
                                    let apply_move = apply_move.clone();
                                    let mv = mv.clone();
                                    Rc::new(move || apply_move(&mv))
                                };
                                let on_copy: Rc<dyn Fn()> = {
                                    let apply_copy = apply_copy.clone();
                                    let mv = mv.clone();
                                    Rc::new(move || apply_copy(&mv))
                                };
                                show_copy_move_popover(
                                    &target_row,
                                    dest.as_deref(),
                                    &on_move,
                                    &on_copy,
                                );
                            })
                        };

                        match &cands[..] {
                            // A single crossing line: splice it (Copy/Move if it
                            // crosses a branch, straight Move otherwise).
                            [single] => apply(&single.mv),
                            // Several lines cross the gap: ask which one (labelled
                            // by branch), then splice the picked line via `apply`.
                            _ => show_lane_popover(
                                &list,
                                to,
                                cands
                                    .into_iter()
                                    .map(|c| (c.lane, lb.label_for(line_commits(&c.mv)), c.mv))
                                    .collect(),
                                &apply,
                                true,
                            ),
                        }
                    }));
                    true
                }
                DragOrigin::Trash if onto.is_some() => {
                    // Dropped a trashed commit ONTO a chain commit: squash its
                    // changes into that commit and forget it from the trash. A
                    // prefixed trashed subject acts at once; otherwise a popover
                    // picks the mode — mirroring the history squash arm above.
                    let onto = onto.unwrap();
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let display = display.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let selected_change = selected_change.clone();
                    let trashed = trashed.clone();
                    let trash_list = trash_list.clone();
                    let trash_scroll = trash_scroll.clone();
                    let on_restore = on_restore.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        // `from` is the trash row index (a separate list); `onto` is a
                        // history display row — map it to its commit index.
                        let Some(onto_ci) = row_commit_index(&display.borrow(), onto) else {
                            return;
                        };
                        let Some(info) = trashed.borrow().get(from as usize).cloned() else {
                            return;
                        };
                        let plan =
                            repo.borrow()
                                .plan_squash_restore(&commits.borrow(), &info, onto_ci);
                        let Some((source, dest)) = plan else {
                            return;
                        };
                        let subject = info.subject.clone();
                        let change_hex = info.change_id_hex();
                        // After the squash, select the drop target: its change id is
                        // stable across the rewrite, the squashed-in source's is gone.
                        let dest_change = commits.borrow()[onto_ci].change_id_hex();

                        // Run a chosen mode and report the outcome.
                        let apply: Rc<dyn Fn(SquashMode)> = {
                            let repo = repo.clone();
                            let refresh = refresh.clone();
                            let show_status = show_status.clone();
                            let enter_conflict_mode = enter_conflict_mode.clone();
                            let selected_change = selected_change.clone();
                            let trashed = trashed.clone();
                            let trash_list = trash_list.clone();
                            let trash_scroll = trash_scroll.clone();
                            let on_restore = on_restore.clone();
                            Rc::new(move |mode| {
                                let outcome = repo
                                    .borrow_mut()
                                    .squash_restore_into(&source, &dest, mode, None);
                                // On success (Clean or pending Conflicts) the
                                // changes now live in the target, so forget the
                                // trashed commit — match by change id, since the
                                // popover may have let the trash drift.
                                match outcome {
                                    Ok(SaveOutcome::Clean) => {
                                        trashed
                                            .borrow_mut()
                                            .retain(|c| c.change_id_hex() != change_hex);
                                        populate_trash(
                                            &trash_list,
                                            &trash_scroll,
                                            &trashed.borrow(),
                                            on_restore.as_ref(),
                                        );
                                        *selected_change.borrow_mut() = Some(dest_change.clone());
                                        refresh();
                                    }
                                    Ok(SaveOutcome::Conflicts { commits }) => {
                                        trashed
                                            .borrow_mut()
                                            .retain(|c| c.change_id_hex() != change_hex);
                                        populate_trash(
                                            &trash_list,
                                            &trash_scroll,
                                            &trashed.borrow(),
                                            on_restore.as_ref(),
                                        );
                                        enter_conflict_mode(commits);
                                    }
                                    Err(err) => show_status(&format!("Squash failed: {err}")),
                                }
                            })
                        };

                        match parse_squash_mode(&subject) {
                            // Prefixed: the prefix picks the mode, apply at once.
                            Some(mode) => apply(mode),
                            // Unprefixed: ask how to merge, anchored at the target.
                            None => {
                                let Some(target_row) = list.row_at_index(onto as i32) else {
                                    return;
                                };
                                show_squash_popover(&target_row, &apply);
                            }
                        }
                    }));
                    true
                }
                DragOrigin::Trash => {
                    // Restoring a trashed commit: graft it back into the graph at
                    // the drop gap (picking the line when several cross it), drop
                    // it from the trash, and select it.
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let graph = graph.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let trashed = trashed.clone();
                    let pending_trash_op = pending_trash_op.clone();
                    let trash_list = trash_list.clone();
                    let trash_scroll = trash_scroll.clone();
                    let on_restore = on_restore.clone();
                    let selected_change = selected_change.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        let Some(info) = trashed.borrow().get(from as usize).cloned() else {
                            return;
                        };
                        let cands = repo.borrow().plan_restore_candidates(
                            &commits.borrow(),
                            &graph.borrow(),
                            &info,
                            to_ci,
                        );
                        if cands.is_empty() {
                            return;
                        }

                        // Run the chosen splice and report the outcome. Forget the
                        // restored commit by change id — the popover may have let
                        // the trash drift (mirroring the trash-squash arm).
                        let apply: Rc<dyn Fn(&ReorderMove)> = {
                            let repo = repo.clone();
                            let refresh = refresh.clone();
                            let show_status = show_status.clone();
                            let trashed = trashed.clone();
                            let pending_trash_op = pending_trash_op.clone();
                            let trash_list = trash_list.clone();
                            let trash_scroll = trash_scroll.clone();
                            let on_restore = on_restore.clone();
                            let selected_change = selected_change.clone();
                            let enter_conflict_mode = enter_conflict_mode.clone();
                            let info = info.clone();
                            Rc::new(move |mv: &ReorderMove| {
                                let outcome = repo.borrow_mut().restore_commit(
                                    &mv.target,
                                    mv.new_parents.clone(),
                                    mv.new_children.clone(),
                                    &mv.new_tip,
                                );
                                match outcome {
                                    Ok(SaveOutcome::Clean) => {
                                        let change_hex = info.change_id_hex();
                                        trashed
                                            .borrow_mut()
                                            .retain(|c| c.change_id_hex() != change_hex);
                                        *selected_change.borrow_mut() = Some(change_hex);
                                        refresh();
                                        populate_trash(
                                            &trash_list,
                                            &trash_scroll,
                                            &trashed.borrow(),
                                            on_restore.as_ref(),
                                        );
                                    }
                                    Ok(SaveOutcome::Conflicts { commits }) => {
                                        // Don't remove from the trash yet: the rewrite
                                        // is held back from git until the conflicts
                                        // clear, so the trash mustn't change either.
                                        // Defer the removal — applied on a clean
                                        // resolution, dropped on abort.
                                        *pending_trash_op.borrow_mut() =
                                            Some(PendingTrashOp::Restore(Box::new(info.clone())));
                                        enter_conflict_mode(commits);
                                    }
                                    Err(err) => show_status(&format!("Restore failed: {err}")),
                                }
                            })
                        };

                        match &cands[..] {
                            [single] => apply(&single.mv),
                            // Restore grafts within the (primary) branch's lanes,
                            // so the picker is the colour swatch alone.
                            _ => show_lane_popover(
                                &list,
                                to,
                                cands.into_iter().map(|c| (c.lane, None, c.mv)).collect(),
                                &apply,
                                true,
                            ),
                        }
                    }));
                    true
                }
                DragOrigin::WorkingCopy if onto.is_some() => {
                    // Dropped a working-copy `@` node ONTO a commit: fold its changes
                    // into that commit as a Fixup — no popover, no message change. The
                    // `@`'s worktree (`WcTarget`) routes the fold at the right `@`.
                    let onto = onto.unwrap();
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let display = display.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let selected_change = selected_change.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        // Read the dragged `@` (branch + entry) and the target commit
                        // from the display list (`from`/`onto` are display indices).
                        let (branch, entry) = {
                            let disp = display.borrow();
                            match disp.get(from as usize) {
                                Some(DisplayRow::Wc { branch, entry }) => {
                                    (branch.clone(), entry.info.clone())
                                }
                                _ => return,
                            }
                        };
                        let Some(onto_ci) = row_commit_index(&display.borrow(), onto) else {
                            return;
                        };
                        let Some(target) = repo.borrow().wc_target_for_branch(&branch) else {
                            return;
                        };
                        // Validate the target sits on the branch chain (reuse the
                        // trash-squash planner). Fold by the entry's *stable change
                        // id* so the leaf's churning commit id can't go stale across
                        // the internal snapshot.
                        if repo
                            .borrow()
                            .plan_squash_restore(&commits.borrow(), &entry, onto_ci)
                            .is_none()
                        {
                            return;
                        }
                        let dest = commits.borrow()[onto_ci].id.clone();
                        // After the fixup, select the drop target: its change id is
                        // stable across the rewrite.
                        let dest_change = commits.borrow()[onto_ci].change_id_hex();
                        let change_hex = entry.change_id_hex();
                        let outcome = repo.borrow_mut().squash_working_copy_into_at(
                            target,
                            Some(&change_hex),
                            &dest,
                            None,
                        );
                        match outcome {
                            Ok(SaveOutcome::Clean) => {
                                *selected_change.borrow_mut() = Some(dest_change);
                                refresh();
                            }
                            Ok(SaveOutcome::Conflicts { commits }) => enter_conflict_mode(commits),
                            Err(err) => show_status(&format!("Fixup failed: {err}")),
                        }
                    }));
                    true
                }
                DragOrigin::WorkingCopy => {
                    // Dropped between commits (or off a commit): uncommitted entries
                    // can't be reordered, so there is nothing to do.
                    false
                }
            }
        }
    });
    list.add_controller(drop_target);

    // The trash list mirrors the history list's drag-and-drop: a source so its
    // rows can be dragged back into history (restore), and a drop target so
    // history rows dragged onto it are dropped (abandoned). Reordering within the
    // trash is meaningless, so trash→trash drops are ignored.
    let trash_drag = DragSource::new();
    trash_drag.set_actions(gdk::DragAction::MOVE);
    trash_drag.connect_prepare({
        let trash_list = trash_list.clone();
        let trashed = trashed.clone();
        let drag_row = drag_row.clone();
        let drag_origin = drag_origin.clone();
        let drag_from = drag_from.clone();
        move |source, _x, y| {
            if trashed.borrow().is_empty() {
                return None; // only the hint row is present
            }
            let row = trash_list.row_at_y(y as i32)?;
            let paintable = gtk::WidgetPaintable::new(Some(&row));
            source.set_icon(Some(&paintable), 0, 0);
            *drag_row.borrow_mut() = Some(row.clone());
            drag_origin.set(DragOrigin::Trash);
            // The motion handlers (show_gap / set_squash_target) read drag_from to
            // validate the restore/squash; it's the trash row index here.
            drag_from.set(Some(row.index() as usize));
            Some(gdk::ContentProvider::for_value(&row.index().to_value()))
        }
    });
    trash_drag.connect_drag_begin({
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let trashed = trashed.clone();
        let repo = repo.clone();
        let commits = commits.clone();
        let commit_rows = commit_rows.clone();
        let list = list.clone();
        move |_source, _drag| {
            if let Some(row) = drag_row.borrow().as_ref() {
                row.add_css_class("commit-dragging");
            }
            // Same green/yellow squash hints as a history drag, for a trashed
            // commit whose subject carries an autosquash prefix. Empty otherwise.
            // The recommendations are commit indices — map each to its display row.
            if let Some(info) = drag_from
                .get()
                .and_then(|f| trashed.borrow().get(f).cloned())
            {
                let row_of = |ci: usize| {
                    commit_rows
                        .borrow()
                        .get(ci)
                        .and_then(|&di| list.row_at_index(di as i32))
                };
                let recs = repo
                    .borrow()
                    .squash_recommendations_for(&commits.borrow(), &info);
                for i in recs.targets {
                    if let Some(r) = row_of(i) {
                        r.add_css_class("squash-recommended");
                    }
                }
                for i in recs.siblings {
                    if let Some(r) = row_of(i) {
                        r.add_css_class("squash-sibling");
                    }
                }
            }
        }
    });
    trash_drag.connect_drag_end({
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let clear_gap = clear_gap.clone();
        let clear_squash_target = clear_squash_target.clone();
        let list = list.clone();
        let post_drag = post_drag.clone();
        move |_source, _drag, _delete| {
            if let Some(row) = drag_row.borrow_mut().take() {
                row.remove_css_class("commit-dragging");
            }
            drag_from.set(None);
            clear_gap();
            // The trash drag highlights history rows too (green/yellow recs, red
            // target); strip them here, as populate_rows leaves them alone.
            let mut i = 0;
            while let Some(r) = list.row_at_index(i) {
                r.remove_css_class("squash-recommended");
                r.remove_css_class("squash-sibling");
                r.remove_css_class("squash-blame");
                i += 1;
            }
            clear_squash_target();
            run_post_drag(&post_drag);
        }
    });
    trash_list.add_controller(trash_drag);

    // Working-copy `@` nodes are dragged from the history `list` itself (its drag
    // source sets `DragOrigin::WorkingCopy` for an `@` row — see `connect_prepare`),
    // so there is no separate working-copy drag source any more. The history drop
    // target's `WorkingCopy` arm folds the dragged `@` into a commit; the trash drop
    // discards it.

    // History drags now arrive as a text payload, working-copy drags as an i32;
    // accept both. (Trash→trash and cross-window drops are rejected in the handler.)
    let trash_drop = DropTarget::new(String::static_type(), gdk::DragAction::MOVE);
    trash_drop.set_types(&[String::static_type(), i32::static_type()]);
    // Deliberately no widget mutation in enter/leave (no hover highlight): those
    // run inside GTK's drop-crossing synthesis, where touching the widget tree is
    // unsafe. Enter just advertises that the trash accepts the drag.
    trash_drop.connect_enter(move |_target, _x, _y| gdk::DragAction::MOVE);
    trash_drop.connect_drop({
        let commits = commits.clone();
        let display = display.clone();
        let repo = repo.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let drag_origin = drag_origin.clone();
        let trashed = trashed.clone();
        let pending_trash_op = pending_trash_op.clone();
        let trash_list = trash_list.clone();
        let trash_scroll = trash_scroll.clone();
        let on_restore = on_restore.clone();
        let post_drag = post_drag.clone();
        let enter_conflict_mode = enter_conflict_mode.clone();
        let list = list.clone();
        let selected_change = selected_change.clone();
        let selected_changes = selected_changes.clone();
        let drag_set = drag_set.clone();
        let drag_from = drag_from.clone();
        move |_target, value, _x, _y| {
            // A commit dragged from another commedit window isn't in this history,
            // so it can't be abandoned here — reject it.
            if value
                .get::<String>()
                .ok()
                .and_then(|s| DraggedCommits::parse(&s))
                .is_some_and(|p| p.pid != own_pid)
            {
                return false;
            }
            // The trash accepts a history commit (abandoned, but kept so it can be
            // dragged back to restore) or an uncommitted-changes entry (discarded
            // outright). A trash→trash drag has nothing to do.
            let origin = drag_origin.get();
            if origin != DragOrigin::History && origin != DragOrigin::WorkingCopy {
                return false;
            }
            // The source row index: history now travels as text, but every source
            // also records it in `drag_from`.
            let Some(from) = drag_from.get().map(|i| i as i32) else {
                return false;
            };
            // A multi-selection dragged as a group (history only); empty otherwise.
            let set = drag_set.borrow().clone();
            // Stage the work; the drag source runs it from `drag-end`, once the
            // gesture is fully over (rewriting + rebuilding the rows mid-drag
            // frees a row GTK still tracks, crashing the next event).
            let repo = repo.clone();
            let commits = commits.clone();
            let display = display.clone();
            let refresh = refresh.clone();
            let show_status = show_status.clone();
            let trashed = trashed.clone();
            let trashed_origin = trashed_origin.clone();
            let pending_trash_op = pending_trash_op.clone();
            let trash_list = trash_list.clone();
            let trash_scroll = trash_scroll.clone();
            let on_restore = on_restore.clone();
            let enter_conflict_mode = enter_conflict_mode.clone();
            let list = list.clone();
            let selected_change = selected_change.clone();
            let selected_changes = selected_changes.clone();
            *post_drag.borrow_mut() = Some(Box::new(move || {
                if origin == DragOrigin::WorkingCopy {
                    // Discard a working-copy `@`. It has no git object to graft back,
                    // so — unlike a dropped commit — it is gone for good: not pushed to
                    // `trashed`, not listed in the trash. Read its branch + change id
                    // from the dragged display row, and discard *that* worktree's `@`
                    // (the leaf's commit id churns on the internal snapshot).
                    let (branch, change) = {
                        let disp = display.borrow();
                        match disp.get(from as usize) {
                            Some(DisplayRow::Wc { branch, entry }) => {
                                (branch.clone(), entry.info.change_id_hex())
                            }
                            _ => return,
                        }
                    };
                    let Some(target) = repo.borrow().wc_target_for_branch(&branch) else {
                        return;
                    };
                    // Bind the outcome before matching so the `borrow_mut` is
                    // released — `refresh` borrows `repo` again (a `match`
                    // scrutinee's temporary otherwise lives across the arms).
                    let outcome = repo
                        .borrow_mut()
                        .drop_working_copy_at(target, Some(&change));
                    match outcome {
                        Ok(()) => refresh(),
                        Err(err) => show_status(&format!("Drop failed: {err}")),
                    }
                    return;
                }
                // `set` holds display indices of the dragged commit rows — map them to
                // commit indices for the planners.
                let set: Vec<usize> = {
                    let disp = display.borrow();
                    set.iter()
                        .filter_map(|&di| row_commit_index(&disp, di))
                        .collect()
                };
                if set.len() > 1 {
                    // Group drop: trash every selected commit in one rebase. Refuse
                    // if it would empty the displayed branch (nothing left to anchor).
                    // Each dropped commit, with its origin branch for restore
                    // routing (computed from the pre-drop lanes, before the rebase
                    // orphans them).
                    let (infos, targets, origins) = {
                        let c = commits.borrow();
                        if set.len() >= c.len() {
                            show_status("Can't drop every commit");
                            return;
                        }
                        let r = repo.borrow();
                        let lb = lane_branches(&r, &c);
                        let primary = r.target_branch_name();
                        let mut infos = Vec::new();
                        let mut targets = Vec::new();
                        let mut origins = Vec::new();
                        for &i in &set {
                            match r.plan_drop_multi(&c, i) {
                                Some(id) => {
                                    origins.push(trash_origin(&lb, primary, &c[i].id));
                                    infos.push(c[i].clone());
                                    targets.push(id);
                                }
                                None => {
                                    show_status("Can't drop one of the selected commits");
                                    return;
                                }
                            }
                        }
                        (infos, targets, origins)
                    };
                    let outcome = repo.borrow_mut().abandon_commits(targets);
                    match outcome {
                        Ok(SaveOutcome::Clean) => {
                            // The selected commits are gone; move the selection to the
                            // newest surviving commit so the pane stops showing them.
                            let survivor = {
                                let cs = commits.borrow();
                                cs.iter()
                                    .enumerate()
                                    .find(|(i, _)| !set.contains(i))
                                    .map(|(_, c)| c.change_id_hex())
                            };
                            *selected_change.borrow_mut() = survivor;
                            selected_changes.borrow_mut().clear();
                            list.unselect_all();
                            {
                                let mut om = trashed_origin.borrow_mut();
                                for (info, origin) in infos.iter().zip(&origins) {
                                    if let Some(o) = origin {
                                        om.insert(info.change_id_hex(), o.clone());
                                    }
                                }
                            }
                            trashed.borrow_mut().extend(infos);
                            refresh();
                            populate_trash(
                                &trash_list,
                                &trash_scroll,
                                &trashed.borrow(),
                                on_restore.as_ref(),
                            );
                        }
                        Ok(SaveOutcome::Conflicts { commits }) => {
                            *pending_trash_op.borrow_mut() = Some(PendingTrashOp::Drop(
                                infos.into_iter().zip(origins).collect(),
                            ));
                            enter_conflict_mode(commits);
                        }
                        Err(err) => show_status(&format!("Drop failed: {err}")),
                    }
                    return;
                }
                // The dragged history row in commit space.
                let Some(from_ci) = row_commit_index(&display.borrow(), from as usize) else {
                    return;
                };
                let Some(info) = commits.borrow().get(from_ci).cloned() else {
                    return;
                };
                // Only commits on an editable branch's linear chain (and not its
                // sole commit) can be dropped; refuse merges/off-branch/root rows.
                // `_multi` spans every editable head, so a sibling branch's commit
                // is droppable too.
                let target = repo.borrow().plan_drop_multi(&commits.borrow(), from_ci);
                let Some(target) = target else {
                    show_status("Can't drop this commit");
                    return;
                };
                // The origin branch to remember for restore routing, read from the
                // pre-drop lanes (the abandon below orphans the commit off them).
                let origin = {
                    let r = repo.borrow();
                    let cs = commits.borrow();
                    trash_origin(&lane_branches(&r, &cs), r.target_branch_name(), &info.id)
                };
                let outcome = repo.borrow_mut().abandon_commit(&target);
                match outcome {
                    Ok(SaveOutcome::Clean) => {
                        // If the dropped commit was the selected one it's gone now,
                        // so the detail pane would keep showing it: move the
                        // selection to a surviving neighbour (the commit that takes
                        // its slot, else its child) and force the reselect to
                        // re-fire `row-selected` so the diff reloads — the same idiom
                        // as the abort handler.
                        let fi = from_ci;
                        if selected_change.borrow().as_deref()
                            == Some(info.change_id_hex().as_str())
                        {
                            let neighbour = {
                                let cs = commits.borrow();
                                cs.get(fi + 1)
                                    .or_else(|| fi.checked_sub(1).and_then(|i| cs.get(i)))
                                    .map(|c| c.change_id_hex())
                            };
                            *selected_change.borrow_mut() = neighbour;
                            list.unselect_all();
                        }
                        if let Some(o) = &origin {
                            trashed_origin
                                .borrow_mut()
                                .insert(info.change_id_hex(), o.clone());
                        }
                        trashed.borrow_mut().push(info);
                        refresh();
                        populate_trash(
                            &trash_list,
                            &trash_scroll,
                            &trashed.borrow(),
                            on_restore.as_ref(),
                        );
                    }
                    Ok(SaveOutcome::Conflicts { commits }) => {
                        // Don't add to the trash yet: the rewrite is held back from
                        // git until the conflicts clear. Defer the add — applied on
                        // a clean resolution, dropped on abort. `enter_conflict_mode`
                        // selects the commit being resolved, so the pane refreshes.
                        *pending_trash_op.borrow_mut() =
                            Some(PendingTrashOp::Drop(vec![(info, origin)]));
                        enter_conflict_mode(commits);
                    }
                    Err(err) => show_status(&format!("Drop failed: {err}")),
                }
            }));
            true
        }
    });
    trash_box.add_controller(trash_drop);
}

/// Run a drop's staged action, scheduled from the drag source's `drag-end`.
///
/// The action rewrites history and rebuilds the list widgets, which unparents
/// the `GtkListBoxRow`s. If that happens while GTK still has drag-and-drop
/// crossing events queued for the just-finished gesture, GTK walks a row it
/// holds as the drop target after we've orphaned it (parent becomes NULL) and
/// segfaults. Scheduling at idle priority — below GDK's event priority — runs
/// the rebuild only once every pending crossing event has been drained, so the
/// rows are alive for all of them. (Scheduling from `drag-end` rather than the
/// drop handler matters too: an idle queued mid-gesture can fire between motion
/// events, i.e. before the drag is over.)
fn run_post_drag(post_drag: &PostDrag) {
    if let Some(action) = post_drag.borrow_mut().take() {
        glib::idle_add_local_once(action);
    }
}

/// The commit payload `target` currently holds *if* it comes from another
/// commedit window (`set_preload` makes the value available during motion).
/// `None` for this window's own drag or a non-commedit drop. Steers the hover
/// feedback only; the drop handler re-derives this from the dropped value, so
/// correctness never depends on the preload having landed.
fn foreign_payload(target: &DropTarget, own_pid: u32) -> Option<DraggedCommits> {
    target
        .value()
        .and_then(|v| v.get::<String>().ok())
        .and_then(|s| DraggedCommits::parse(&s))
        .filter(|p| p.pid != own_pid)
}

/// The single hovering commit's sha for the gap gate: a foreign drag of exactly
/// one commit (the only shape v1 cherry-picks). `None` otherwise.
fn foreign_pick_sha(target: &DropTarget, own_pid: u32) -> Option<String> {
    match foreign_payload(target, own_pid)?.commits.as_slice() {
        [one] => Some(one.sha.clone()),
        _ => None,
    }
}

/// A small popover anchored at `target_row` letting the user pick how to merge
/// an unprefixed commit dropped onto another: Fixup / Squash / Amend, or Cancel.
/// Each verb runs `apply(mode)` and dismisses; Cancel (or a click outside) just
/// dismisses. Shown from the post-drag idle, where the row is alive and GTK's
/// drag bookkeeping is already torn down.
fn show_squash_popover(target_row: &ListBoxRow, apply: &Rc<dyn Fn(SquashMode)>) {
    let popover = Popover::new();
    let vbox = GtkBox::new(Orientation::Vertical, 0);
    let button = |label: &str, tip: &str| {
        let b = Button::with_label(label);
        b.add_css_class("flat");
        b.set_tooltip_text(Some(tip));
        b.set_halign(gtk::Align::Fill);
        vbox.append(&b);
        b
    };
    let fixup_btn = button("Fixup", "Merge changes in; keep this commit's message.");
    let squash_btn = button(
        "Squash",
        "Merge changes in; append the dragged commit's message.",
    );
    let amend_btn = button(
        "Amend",
        "Merge changes in; replace this commit's message with the dragged commit's.",
    );
    vbox.append(&gtk::Separator::new(Orientation::Horizontal));
    let cancel_btn = button("Cancel", "Don't merge — leave history unchanged.");

    popover.set_child(Some(&vbox));
    // Parent to the list (the row's container), NOT the row itself: a *selected*
    // target row carries the selected-state foreground (white), which the
    // popover's button labels would inherit through the widget tree — leaving
    // white-on-grey, unreadable text. The list carries the normal theme colors.
    // Point the popover at the row's allocation (in list coordinates) so it
    // still anchors at the drop target.
    if let Some(parent) = target_row.parent() {
        popover.set_parent(&parent);
        popover.set_pointing_to(Some(&target_row.allocation()));
    } else {
        popover.set_parent(target_row);
    }
    popover.set_autohide(true);

    let wire = |btn: &Button, mode: Option<SquashMode>| {
        let apply = apply.clone();
        let popover = popover.clone();
        btn.connect_clicked(move |_| {
            if let Some(mode) = mode {
                apply(mode);
            }
            popover.popdown();
        });
    };
    wire(&fixup_btn, Some(SquashMode::Fixup));
    wire(&squash_btn, Some(SquashMode::Squash));
    wire(&amend_btn, Some(SquashMode::Amend));
    wire(&cancel_btn, None);

    // Detach when dismissed (verb click or outside-click) so a popover doesn't
    // leak per drop.
    popover.connect_closed(|p| p.unparent());
    popover.popup();
}

/// A small popover anchored at `target_row` letting the user choose how a drag
/// that **crosses a branch boundary** lands: **Move** (reparent — consume the
/// source) or **Copy** (cherry-pick — leave the source intact), or Cancel. The
/// UX family of [`show_squash_popover`]; shown only when the destination lane
/// belongs to a different branch than the source (an in-branch reorder never asks
/// — it just moves, exactly as before). `dest_branch` names the branch being
/// dropped onto, for the button tooltips. Shown from the post-drag idle, where the
/// row is alive and GTK's drag bookkeeping is torn down.
fn show_copy_move_popover(
    target_row: &ListBoxRow,
    dest_branch: Option<&str>,
    on_move: &Rc<dyn Fn()>,
    on_copy: &Rc<dyn Fn()>,
) {
    let popover = Popover::new();
    let vbox = GtkBox::new(Orientation::Vertical, 0);
    let onto = dest_branch
        .map(|b| format!(" onto {b}"))
        .unwrap_or_default();
    let button = |label: &str, tip: &str| {
        let b = Button::with_label(label);
        b.add_css_class("flat");
        b.set_tooltip_text(Some(tip));
        b.set_halign(gtk::Align::Fill);
        vbox.append(&b);
        b
    };
    let move_btn = button(
        "Move",
        &format!(
            "Reparent this commit{onto} — its descendants rebase, and it leaves its old branch."
        ),
    );
    let copy_btn = button(
        "Copy",
        &format!(
            "Cherry-pick this commit{onto} — a re-applied copy; the original stays where it is."
        ),
    );
    vbox.append(&gtk::Separator::new(Orientation::Horizontal));
    let cancel_btn = button("Cancel", "Don't move or copy — leave history unchanged.");

    popover.set_child(Some(&vbox));
    // Parent to the list, not the (possibly selected) row, so the button labels
    // get the normal theme colours — same reasoning as `show_squash_popover`.
    if let Some(parent) = target_row.parent() {
        popover.set_parent(&parent);
        popover.set_pointing_to(Some(&target_row.allocation()));
    } else {
        popover.set_parent(target_row);
    }
    popover.set_autohide(true);

    let wire = |btn: &Button, action: Option<Rc<dyn Fn()>>| {
        let popover = popover.clone();
        btn.connect_clicked(move |_| {
            if let Some(action) = action.as_ref() {
                action();
            }
            popover.popdown();
        });
    };
    wire(&move_btn, Some(on_move.clone()));
    wire(&copy_btn, Some(on_copy.clone()));
    wire(&cancel_btn, None);

    popover.connect_closed(|p| p.unparent());
    popover.popup();
}

/// The list row to anchor a gap-borne popover at: the (visible) row just below
/// the gap `to`, or — for the bottom gap — the last visible row above it. `None`
/// when neither is alive. Surplus rows `populate_rows` hid (never unparented)
/// have stale allocations, so they are skipped. Used by the Copy/Move chooser
/// (the lane picker inlines the same rule, pointing at the 1px gap edge instead).
fn popover_anchor_row(list: &ListBox, to: usize) -> Option<ListBoxRow> {
    let row_at = |i: usize| list.row_at_index(i as i32).filter(|r| r.is_visible());
    row_at(to).or_else(|| to.checked_sub(1).and_then(row_at))
}

/// Width/height of one lane swatch in the pick-a-line popover.
const SWATCH_W: i32 = 16;
const SWATCH_H: i32 = 28;

/// A popover at the drop gap letting the user pick which ancestry line to
/// splice the dragged commit(s) into, when several cross it: one flat button per
/// candidate `(lane, label, payload)` (lane order, matching the graph's columns
/// left-to-right). Each button draws a vertical line in its lane's colour and,
/// when `label` is set (the branch(es) the line carries — see [`LaneBranches`]),
/// shows that branch name under it, so picking a *cross-branch* destination is a
/// named choice rather than a bare colour guess (the multi-branch usability win).
/// A click runs `apply` with that candidate's `payload`; a click outside
/// dismisses. Generic over the payload so both a single-commit [`ReorderMove`]
/// and a group [`ReorderSetMove`] reuse it. Shown from the post-drag idle like
/// [`show_squash_popover`]: the gesture is fully torn down and no rewrite has
/// happened yet, so the rows — and the candidates' commit ids — stay valid while
/// it is open (autohide grabs input, so no second drag can start under it).
fn show_lane_popover<T: 'static>(
    list: &ListBox,
    gap: usize,
    candidates: Vec<(usize, Option<String>, T)>,
    apply: &Rc<dyn Fn(&T)>,
    autohide: bool,
) {
    let popover = Popover::new();
    let hbox = GtkBox::new(Orientation::Horizontal, 0);
    for (lane, label, payload) in candidates {
        let swatch = gtk::DrawingArea::new();
        swatch.set_content_width(SWATCH_W);
        swatch.set_content_height(SWATCH_H);
        swatch.set_draw_func(move |_, cr, w, h| {
            let (r, g, b) = lane_color(lane);
            cr.set_source_rgb(r, g, b);
            cr.set_line_width(3.0);
            cr.move_to(w as f64 / 2.0, 2.0);
            cr.line_to(w as f64 / 2.0, h as f64 - 2.0);
            let _ = cr.stroke();
        });
        // Swatch on top; the branch label (if any) below it, so a labelled lane
        // reads as "this colour = this branch".
        let cell = GtkBox::new(Orientation::Vertical, 2);
        cell.set_halign(gtk::Align::Center);
        cell.append(&swatch);
        if let Some(text) = label {
            let lbl = Label::new(Some(&text));
            lbl.add_css_class("caption");
            cell.append(&lbl);
        }
        let btn = Button::new();
        btn.set_child(Some(&cell));
        btn.add_css_class("flat");
        hbox.append(&btn);
        let apply = apply.clone();
        let popover = popover.clone();
        // `T` isn't `Clone`, but the idle closure below must own the payload, so
        // hold it behind an `Rc`.
        let payload = Rc::new(payload);
        btn.connect_clicked(move |_| {
            // Tear THIS popover down first so it releases its seat grab, then run
            // the action at idle. The action may itself open another grabbing
            // popover (the cross-branch Copy/Move chooser); mapping a grabbing
            // popup while this one still held the grab trips GTK's "grabbing popup
            // with a non-top most parent" refusal — the grab handoff aborts, no
            // popover ends up owning the seat, and all input wedges.
            popover.popdown();
            let apply = apply.clone();
            let payload = payload.clone();
            glib::idle_add_local_once(move || apply(payload.as_ref()));
        });
    }
    popover.set_child(Some(&hbox));

    // Parent to the list, not a row (same selected-row color reasoning as
    // `show_squash_popover`), pointing at the gap's 1px boundary strip: the
    // placeholder is gone by post-drag time, so the gap is the edge between two
    // rows — the top of the row at `gap`, or the bottom edge of the last row
    // for the bottom gap. Skip hidden surplus rows (`populate_rows` hides, never
    // unparents), whose allocations are stale.
    let row_at = |i: usize| list.row_at_index(i as i32).filter(|r| r.is_visible());
    let Some((row, at_top)) = row_at(gap).map(|r| (r, true)).or_else(|| {
        (gap > 0)
            .then(|| row_at(gap - 1).map(|r| (r, false)))
            .flatten()
    }) else {
        return;
    };
    let a = row.allocation();
    let y = if at_top {
        a.y()
    } else {
        a.y() + a.height() - 1
    };
    popover.set_parent(list);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(a.x(), y, a.width(), 1)));
    // An autohide popover grabs the seat (so an outside click dismisses it).
    // After a cross-window drop the compositor's drag grab may still be
    // releasing, and grabbing into that wedges all input — so the caller asks for
    // a non-autohide popover there: it never grabs (no dismiss-on-outside-click,
    // but picking a lane still closes it), trading that for a deterministic
    // no-freeze. In-process drops keep autohide.
    popover.set_autohide(autohide);
    popover.connect_closed(|p| p.unparent());
    popover.popup();
}
