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
use commedit_engine::history::{ReorderMove, ReorderSetMove};
use commedit_engine::squash::{parse_squash_mode, SquashMode};
use gtk::prelude::*;
use gtk::{
    gdk, glib, Box as GtkBox, Button, DragSource, DropTarget, ListBox, ListBoxRow, Orientation,
    Popover,
};

use crate::dnd::{DraggedCommit, DraggedCommits};
use crate::rows::{lane_color, populate_trash};
use crate::state::{Callbacks, Data, DragOrigin, DragState, PendingTrashOp, PostDrag, Widgets};

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
    let wc_list = w.wc_list.clone();
    let repo = d.repo.clone();
    let commits = d.commits.clone();
    let graph = d.graph.clone();
    let trashed = d.trashed.clone();
    let pending_trash_op = d.pending_trash_op.clone();
    let wc_entries = d.wc_entries.clone();
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
        let commits = commits.clone();
        Rc::new(move |y: f64| -> usize {
            let n = commits.borrow().len();
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
            let n = commits.borrow().len();
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
                                new_gap,
                            )
                            .is_empty()
                    })
            } else {
                drag_from.get().is_some_and(|from| match drag_origin.get() {
                    DragOrigin::History => {
                        let set = drag_set.borrow();
                        let commits = commits.borrow();
                        if set.len() > 1 {
                            // Multi-drag: at least one ancestry line bounded by commits
                            // outside the set must cross the gap.
                            let ids: HashSet<_> = set
                                .iter()
                                .filter_map(|&i| commits.get(i).map(|c| c.id.clone()))
                                .collect();
                            !repo
                                .borrow()
                                .plan_reorder_set_candidates(
                                    &commits,
                                    &graph.borrow(),
                                    &ids,
                                    new_gap,
                                )
                                .is_empty()
                        } else {
                            !repo
                                .borrow()
                                .plan_reorder_candidates(&commits, &graph.borrow(), from, new_gap)
                                .is_empty()
                        }
                    }
                    DragOrigin::Trash => trashed.borrow().get(from).is_some_and(|info| {
                        !repo
                            .borrow()
                            .plan_restore_candidates(
                                &commits.borrow(),
                                &graph.borrow(),
                                info,
                                new_gap,
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
        let repo = repo.clone();
        let drag_from = drag_from.clone();
        let drag_set = drag_set.clone();
        let drag_origin = drag_origin.clone();
        let drop_onto = drop_onto.clone();
        let trashed = trashed.clone();
        let wc_entries = wc_entries.clone();
        Rc::new(move |ci: usize| {
            if drop_onto.get() == Some(ci) {
                return;
            }
            if let Some(prev) = drop_onto.get() {
                if let Some(r) = list.row_at_index(prev as i32) {
                    r.remove_css_class("squash-drop-target");
                }
            }
            // A history drag squashes one chain commit onto another; a trash drag
            // squashes the trashed commit onto the chain commit at `ci`; a
            // working-copy drag folds that uncommitted entry into it (a fixup).
            let valid = drag_from.get().is_some_and(|from| match drag_origin.get() {
                DragOrigin::History => {
                    let set = drag_set.borrow();
                    let commits = commits.borrow();
                    if set.len() > 1 {
                        // Every selected commit must fold onto the target, and the
                        // target must not be one of them.
                        !set.contains(&ci)
                            && set
                                .iter()
                                .all(|&i| repo.borrow().plan_squash(&commits, i, ci).is_some())
                    } else {
                        repo.borrow().plan_squash(&commits, from, ci).is_some()
                    }
                }
                DragOrigin::Trash => trashed.borrow().get(from).is_some_and(|info| {
                    repo.borrow()
                        .plan_squash_restore(&commits.borrow(), info, ci)
                        .is_some()
                }),
                DragOrigin::WorkingCopy => wc_entries.borrow().get(from).is_some_and(|e| {
                    repo.borrow()
                        .plan_squash_restore(&commits.borrow(), &e.info, ci)
                        .is_some()
                }),
            });
            if valid {
                if let Some(r) = list.row_at_index(ci as i32) {
                    r.add_css_class("squash-drop-target");
                }
                drop_onto.set(Some(ci));
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
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let drag_set = drag_set.clone();
        let drag_origin = drag_origin.clone();
        let commits = commits.clone();
        let own_key = own_key.clone();
        move |source, _x, y| {
            let row = list.row_at_y(y as i32)?;
            let idx = row.index() as usize;
            // If the grabbed row is part of a standing multi-selection, drag the
            // whole set as a group; otherwise it's an ordinary single-commit drag.
            // Indices are in commit space (no placeholder is inserted yet) and stay
            // valid for the gesture — the rewrite only runs at drag-end.
            let mut selected: Vec<usize> = list
                .selected_rows()
                .iter()
                .map(|r| r.index() as usize)
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
            // Carry the dragged commit(s) as a text payload, so a drop onto
            // another commedit window — a separate process — can cherry-pick
            // them. An in-process drop reads the very same string back; it just
            // ignores the commit list and works from the live `drag_*` state.
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
                DraggedCommits {
                    pid: std::process::id(),
                    repo_key: own_key.clone().unwrap_or_default(),
                    branch: None,
                    commits: dragged
                        .iter()
                        .filter_map(|&i| c.get(i))
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
        let repo = repo.clone();
        let commits = commits.clone();
        let list = list.clone();
        move |_source, _drag| {
            if let Some(row) = drag_row.borrow().as_ref() {
                row.add_css_class("commit-dragging");
            }
            // Highlight where this commit would squash: green for the real
            // target(s), yellow for other autosquash commits aimed at the same
            // target. Empty (no-op) unless the dragged commit is prefixed. Skipped
            // for a multi-drag — a group squash always asks via the popover.
            if let Some(from) = drag_from.get().filter(|_| drag_set.borrow().len() <= 1) {
                let recs = repo
                    .borrow()
                    .squash_recommendations(&commits.borrow(), from);
                for i in recs.targets {
                    if let Some(r) = list.row_at_index(i as i32) {
                        r.add_css_class("squash-recommended");
                    }
                }
                for i in recs.siblings {
                    if let Some(r) = list.row_at_index(i as i32) {
                        r.add_css_class("squash-sibling");
                    }
                }
                // Purple: every line this commit removes blames to one single
                // commit — a content-derived "it belongs here", stronger than the
                // name match. Wins over green/yellow on the same row, so strip
                // those first to keep the colour unambiguous.
                if let Some(i) = repo.borrow().blame_single_source(&commits.borrow(), from) {
                    if let Some(r) = list.row_at_index(i as i32) {
                        r.remove_css_class("squash-recommended");
                        r.remove_css_class("squash-sibling");
                        r.add_css_class("squash-blame");
                    }
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
        let wc_entries = wc_entries.clone();
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
            let to = match drop_gap.get() {
                Some(to) => to,
                None => gap_at(y),
            };
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
                    let cands = repo.borrow().plan_cherry_pick_candidates(
                        &commits.borrow(),
                        &graph.borrow(),
                        &target,
                        to,
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
                        _ => show_lane_popover(
                            &list,
                            to,
                            cands.into_iter().map(|c| (c.lane, c.mv)).collect(),
                            &apply,
                            false,
                        ),
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
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let selected_change = selected_change.clone();
                    let selected_changes = selected_changes.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        // Validate the target and collect the source ids (newest
                        // first) before mutating; bail if anything no longer fits.
                        let (sources, dest, dest_change) = {
                            let c = commits.borrow();
                            if set.contains(&onto)
                                || !set
                                    .iter()
                                    .all(|&i| repo.borrow().plan_squash(&c, i, onto).is_some())
                            {
                                return;
                            }
                            let sources: Vec<_> = set
                                .iter()
                                .filter_map(|&i| c.get(i).map(|x| x.id.clone()))
                                .collect();
                            let Some(dest) = c.get(onto) else {
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
                    let graph = graph.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        let ids: HashSet<_> = {
                            let c = commits.borrow();
                            set.iter()
                                .filter_map(|&i| c.get(i).map(|x| x.id.clone()))
                                .collect()
                        };
                        let cands = repo.borrow().plan_reorder_set_candidates(
                            &commits.borrow(),
                            &graph.borrow(),
                            &ids,
                            to,
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
                            _ => show_lane_popover(
                                &list,
                                to,
                                cands.into_iter().map(|c| (c.lane, c.mv)).collect(),
                                &apply,
                                true,
                            ),
                        }
                    }));
                    true
                }
                DragOrigin::History if onto.is_some() => {
                    // Dropped ONTO a commit: squash the dragged commit into it. A
                    // prefixed commit acts immediately; an unprefixed one opens a
                    // popover to pick the mode.
                    let onto = onto.unwrap();
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let selected_change = selected_change.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        let plan =
                            repo.borrow()
                                .plan_squash(&commits.borrow(), from as usize, onto);
                        let Some((source, dest)) = plan else {
                            return;
                        };
                        let subject = commits.borrow()[from as usize].subject.clone();
                        // After the squash, select the drop target: its change id is
                        // stable across the rewrite, the squashed-away source's is gone.
                        let dest_change = commits.borrow()[onto].change_id_hex();

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
                    let graph = graph.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let list = list.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        // One candidate per ancestry line crossing the gap; a
                        // no-op, merge or off-branch drop yields none.
                        let cands = repo.borrow().plan_reorder_candidates(
                            &commits.borrow(),
                            &graph.borrow(),
                            from as usize,
                            to,
                        );
                        if cands.is_empty() {
                            return;
                        }

                        // Run the chosen splice and report the outcome.
                        let apply: Rc<dyn Fn(&ReorderMove)> = {
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

                        match &cands[..] {
                            // A single crossing line: splice right in.
                            [single] => apply(&single.mv),
                            // Several lines cross the gap: ask which one.
                            _ => show_lane_popover(
                                &list,
                                to,
                                cands.into_iter().map(|c| (c.lane, c.mv)).collect(),
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
                        let Some(info) = trashed.borrow().get(from as usize).cloned() else {
                            return;
                        };
                        let plan =
                            repo.borrow()
                                .plan_squash_restore(&commits.borrow(), &info, onto);
                        let Some((source, dest)) = plan else {
                            return;
                        };
                        let subject = info.subject.clone();
                        let change_hex = info.change_id_hex();
                        // After the squash, select the drop target: its change id is
                        // stable across the rewrite, the squashed-in source's is gone.
                        let dest_change = commits.borrow()[onto].change_id_hex();

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
                            to,
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
                            _ => show_lane_popover(
                                &list,
                                to,
                                cands.into_iter().map(|c| (c.lane, c.mv)).collect(),
                                &apply,
                                true,
                            ),
                        }
                    }));
                    true
                }
                DragOrigin::WorkingCopy if onto.is_some() => {
                    // Dropped a working-copy entry ONTO a commit: fold its changes
                    // in as a Fixup — no popover, no message change.
                    let onto = onto.unwrap();
                    let repo = repo.clone();
                    let commits = commits.clone();
                    let wc_entries = wc_entries.clone();
                    let refresh = refresh.clone();
                    let show_status = show_status.clone();
                    let enter_conflict_mode = enter_conflict_mode.clone();
                    let selected_change = selected_change.clone();
                    *post_drag.borrow_mut() = Some(Box::new(move || {
                        let entry = wc_entries
                            .borrow()
                            .get(from as usize)
                            .map(|e| e.info.clone());
                        let Some(entry) = entry else {
                            return;
                        };
                        // Validate the target sits on the branch chain (reuse the
                        // trash-squash planner). Fold by the entry's *stable change
                        // id* so the leaf's churning commit id can't go stale across
                        // the internal snapshot.
                        if repo
                            .borrow()
                            .plan_squash_restore(&commits.borrow(), &entry, onto)
                            .is_none()
                        {
                            return;
                        }
                        let dest = commits.borrow()[onto].id.clone();
                        // After the fixup, select the drop target: its change id is
                        // stable across the rewrite.
                        let dest_change = commits.borrow()[onto].change_id_hex();
                        let change_hex = entry.change_id_hex();
                        let outcome = repo.borrow_mut().squash_working_copy_into(
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
        let list = list.clone();
        move |_source, _drag| {
            if let Some(row) = drag_row.borrow().as_ref() {
                row.add_css_class("commit-dragging");
            }
            // Same green/yellow squash hints as a history drag, for a trashed
            // commit whose subject carries an autosquash prefix. Empty otherwise.
            if let Some(info) = drag_from
                .get()
                .and_then(|f| trashed.borrow().get(f).cloned())
            {
                let recs = repo
                    .borrow()
                    .squash_recommendations_for(&commits.borrow(), &info);
                for i in recs.targets {
                    if let Some(r) = list.row_at_index(i as i32) {
                        r.add_css_class("squash-recommended");
                    }
                }
                for i in recs.siblings {
                    if let Some(r) = list.row_at_index(i as i32) {
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

    // The working-copy list is a drag *source* only (its rows can be folded onto a
    // commit), never a drop target. It shares the history list's drop target and
    // the deferred `post_drag` machinery; the drop handler's `WorkingCopy` arm
    // folds the dragged entry in as a fixup.
    let wc_drag = DragSource::new();
    wc_drag.set_actions(gdk::DragAction::MOVE);
    wc_drag.connect_prepare({
        let wc_list = wc_list.clone();
        let drag_row = drag_row.clone();
        let drag_origin = drag_origin.clone();
        let drag_from = drag_from.clone();
        move |source, _x, y| {
            let row = wc_list.row_at_y(y as i32)?;
            let paintable = gtk::WidgetPaintable::new(Some(&row));
            source.set_icon(Some(&paintable), 0, 0);
            *drag_row.borrow_mut() = Some(row.clone());
            drag_origin.set(DragOrigin::WorkingCopy);
            // Index into `wc_entries`, read by the motion/drop handlers.
            drag_from.set(Some(row.index() as usize));
            Some(gdk::ContentProvider::for_value(&row.index().to_value()))
        }
    });
    wc_drag.connect_drag_begin({
        let drag_row = drag_row.clone();
        move |_source, _drag| {
            if let Some(row) = drag_row.borrow().as_ref() {
                row.add_css_class("commit-dragging");
            }
            // No autosquash recommendations: uncommitted entries carry no subject.
        }
    });
    wc_drag.connect_drag_end({
        let drag_row = drag_row.clone();
        let drag_from = drag_from.clone();
        let clear_gap = clear_gap.clone();
        let clear_squash_target = clear_squash_target.clone();
        let post_drag = post_drag.clone();
        move |_source, _drag, _delete| {
            if let Some(row) = drag_row.borrow_mut().take() {
                row.remove_css_class("commit-dragging");
            }
            drag_from.set(None);
            clear_gap();
            clear_squash_target();
            run_post_drag(&post_drag);
        }
    });
    wc_list.add_controller(wc_drag);

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
        let repo = repo.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let drag_origin = drag_origin.clone();
        let trashed = trashed.clone();
        let pending_trash_op = pending_trash_op.clone();
        let trash_list = trash_list.clone();
        let trash_scroll = trash_scroll.clone();
        let on_restore = on_restore.clone();
        let wc_entries = wc_entries.clone();
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
            let wc_entries = wc_entries.clone();
            let refresh = refresh.clone();
            let show_status = show_status.clone();
            let trashed = trashed.clone();
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
                    // Discard an uncommitted-changes entry. It has no git object to
                    // graft back, so — unlike a dropped commit — it is gone for
                    // good: not pushed to `trashed`, not listed in the trash. Drop
                    // by the entry's stable change id (the leaf's commit id churns
                    // on the internal snapshot).
                    let change = wc_entries
                        .borrow()
                        .get(from as usize)
                        .map(|e| e.info.change_id_hex());
                    let Some(change) = change else {
                        return;
                    };
                    // Bind the outcome before matching so the `borrow_mut` is
                    // released — `refresh` borrows `repo` again (a `match`
                    // scrutinee's temporary otherwise lives across the arms).
                    let outcome = repo.borrow_mut().drop_working_copy(Some(&change));
                    match outcome {
                        Ok(()) => refresh(),
                        Err(err) => show_status(&format!("Drop failed: {err}")),
                    }
                    return;
                }
                if set.len() > 1 {
                    // Group drop: trash every selected commit in one rebase. Refuse
                    // if it would empty the displayed branch (nothing left to anchor).
                    let (infos, targets) = {
                        let c = commits.borrow();
                        if set.len() >= c.len() {
                            show_status("Can't drop every commit");
                            return;
                        }
                        let mut infos = Vec::new();
                        let mut targets = Vec::new();
                        for &i in &set {
                            match repo.borrow().plan_drop(&c, i) {
                                Some(id) => {
                                    infos.push(c[i].clone());
                                    targets.push(id);
                                }
                                None => {
                                    show_status("Can't drop one of the selected commits");
                                    return;
                                }
                            }
                        }
                        (infos, targets)
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
                            *pending_trash_op.borrow_mut() = Some(PendingTrashOp::Drop(infos));
                            enter_conflict_mode(commits);
                        }
                        Err(err) => show_status(&format!("Drop failed: {err}")),
                    }
                    return;
                }
                let Some(info) = commits.borrow().get(from as usize).cloned() else {
                    return;
                };
                // Only commits on the current branch's linear chain (and not its
                // sole commit) can be dropped; refuse merges/off-branch/root rows.
                let target = repo.borrow().plan_drop(&commits.borrow(), from as usize);
                let Some(target) = target else {
                    show_status("Can't drop this commit");
                    return;
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
                        let fi = from as usize;
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
                        *pending_trash_op.borrow_mut() = Some(PendingTrashOp::Drop(vec![info]));
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

/// Width/height of one lane swatch in the pick-a-line popover.
const SWATCH_W: i32 = 16;
const SWATCH_H: i32 = 28;

/// A popover at the drop gap letting the user pick which ancestry line to
/// splice the dragged commit(s) into, when several cross it: one flat button per
/// candidate `(lane, payload)` (lane order, matching the graph's columns
/// left-to-right), each drawing just a vertical line in its lane's color — no
/// text. A click runs `apply` with that candidate's `payload`; a click outside
/// dismisses. Generic over the payload so both a single-commit [`ReorderMove`]
/// and a group [`ReorderSetMove`] reuse it. Shown from the post-drag idle like
/// [`show_squash_popover`]: the gesture is fully torn down and no rewrite has
/// happened yet, so the rows — and the candidates' commit ids — stay valid while
/// it is open (autohide grabs input, so no second drag can start under it).
fn show_lane_popover<T: 'static>(
    list: &ListBox,
    gap: usize,
    candidates: Vec<(usize, T)>,
    apply: &Rc<dyn Fn(&T)>,
    autohide: bool,
) {
    let popover = Popover::new();
    let hbox = GtkBox::new(Orientation::Horizontal, 0);
    for (lane, payload) in candidates {
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
        let btn = Button::new();
        btn.set_child(Some(&swatch));
        btn.add_css_class("flat");
        hbox.append(&btn);
        let apply = apply.clone();
        let popover = popover.clone();
        btn.connect_clicked(move |_| {
            apply(&payload);
            popover.popdown();
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
