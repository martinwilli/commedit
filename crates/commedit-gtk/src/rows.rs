//! Commit and working-copy list rows: building a row's content box, updating it
//! in place, and the drag-safe `populate_*` refreshers. The "reuse rows, hide —
//! never unparent — the surplus" discipline in `populate_rows` is load-bearing
//! for drag-and-drop safety; see its doc comment.

use std::collections::{BTreeMap, HashSet};

use commedit_engine::history::CommitInfo;
use commedit_engine::transparency::{RefDecoration, RefKind};
use commedit_engine::workcopy::WorkingCopyEntry;
use gtk::prelude::*;
use gtk::{Box as GtkBox, Label, ListBox, ListBoxRow, Orientation, Overlay, ScrolledWindow};

/// Build the commit-id cell: a short-id [`Label`] wrapped in an [`Overlay`] so a
/// copy icon can float over the id's right edge on hover. Overlay children are
/// excluded from the size request, so the icon clips against — and may hide —
/// the id's last characters but never widens the column. Clicking the *icon*
/// copies the full hash and claims the press, so it does not select the commit;
/// clicking the id itself selects the commit as usual (no copy). The hash is
/// carried in the cell's tooltip (both the copy source and a hover hint showing
/// the id in full).
fn id_cell(short: &str, full_hash: &str) -> Overlay {
    let id_label = Label::builder().xalign(0.0).build();
    id_label.set_markup(&format!("<tt>{short}</tt>"));

    let cell = Overlay::new();
    cell.set_child(Some(&id_label));
    cell.set_halign(gtk::Align::Start);
    set_id_hash(&cell, full_hash);

    let copy = gtk::Image::from_icon_name("edit-copy-symbolic");
    // An explicit pixel size is required: as a non-measured overlay child the
    // icon would otherwise request zero size and never show.
    copy.set_pixel_size(16);
    copy.set_halign(gtk::Align::End);
    copy.set_valign(gtk::Align::Center);
    copy.set_cursor_from_name(Some("pointer"));
    copy.add_css_class("commit-id-copy");
    copy.set_visible(false);
    cell.add_overlay(&copy);

    // Reveal the copy icon only while the pointer is over the id.
    let motion = gtk::EventControllerMotion::new();
    motion.connect_enter({
        let copy = copy.clone();
        move |_, _, _| copy.set_visible(true)
    });
    motion.connect_leave({
        let copy = copy.clone();
        move |_| copy.set_visible(false)
    });
    cell.add_controller(motion);

    // Clicking the icon copies the full hash. Claim the press so the click stays
    // off the row: unlike clicking the id, it must not select the commit.
    let click = gtk::GestureClick::new();
    click.connect_pressed(|gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    click.connect_released({
        let cell = cell.clone();
        move |_, _, _, _| {
            if let Some(hash) = cell.tooltip_text() {
                cell.clipboard().set_text(&hash);
            }
        }
    });
    copy.add_controller(click);
    cell
}

/// Store a commit's full hash on its id cell. The cell's tooltip is both the
/// copy source (read back on click) and a hover hint showing the id in full.
fn set_id_hash(cell: &Overlay, full_hash: &str) {
    cell.set_tooltip_text(Some(full_hash));
}

/// Build the `short-id   subject  [pills]   ⚠` content box shown inside a
/// history/trash row. The pill box hugs the subject's right edge: the subject
/// label does *not* expand, the pill box does (start-aligned), so the pills sit
/// right after the text and the slack stays empty. The trailing warning icon is
/// present but hidden unless `conflicted`.
fn commit_row_box(commit: &CommitInfo, conflicted: bool, refs: &[RefDecoration]) -> GtkBox {
    let short = commit.id_hex().chars().take(8).collect::<String>();
    let subject = if commit.subject.is_empty() {
        "(no description)"
    } else {
        &commit.subject
    };
    let id_cell = id_cell(&short, &commit.id_hex());
    let subject_label = Label::builder()
        .label(subject)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let pills = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(4)
        .hexpand(true)
        .halign(gtk::Align::Start)
        .build();
    set_pills(&pills, refs);
    let badge = gtk::Image::from_icon_name("dialog-warning-symbolic");
    badge.set_tooltip_text(Some("This commit has unresolved conflicts"));
    badge.set_visible(conflicted);
    let row_box = GtkBox::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .margin_start(8)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    row_box.append(&id_cell);
    row_box.append(&subject_label);
    row_box.append(&pills);
    row_box.append(&badge);
    row_box
}

/// Fill the pill box with one colored label per branch/tag pointing at the
/// commit, reusing the existing labels and hiding the surplus — the same
/// never-unparent discipline as [`populate_rows`], one level down.
fn set_pills(pills: &GtkBox, refs: &[RefDecoration]) {
    let mut next = pills.first_child();
    for r in refs {
        let label = match next.clone().and_downcast::<Label>() {
            Some(label) => {
                next = label.next_sibling();
                label
            }
            None => {
                let label = Label::new(None);
                // Cap a runaway ref name rather than crowding out the badge;
                // the tooltip below always carries it in full.
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                label.set_max_width_chars(24);
                label.add_css_class("ref-pill");
                pills.append(&label);
                label
            }
        };
        label.set_visible(true);
        label.set_text(&r.name);
        label.set_tooltip_text(Some(&match r.kind {
            RefKind::Branch => format!("Branch {}", r.name),
            RefKind::Tag => format!("Tag {}", r.name),
        }));
        label.remove_css_class("ref-branch");
        label.remove_css_class("ref-tag");
        label.add_css_class(match r.kind {
            RefKind::Branch => "ref-branch",
            RefKind::Tag => "ref-tag",
        });
    }
    while let Some(extra) = next {
        next = extra.next_sibling();
        extra.set_visible(false);
    }
}

/// Update the content of a row's existing labels in place, without replacing the
/// child widget — so the labels (and the row) survive a drag-triggered rebuild.
/// Falls back to building a fresh child if the row has none yet.
fn set_row_commit(row: &ListBoxRow, commit: &CommitInfo, conflicted: bool, refs: &[RefDecoration]) {
    let short = commit.id_hex().chars().take(8).collect::<String>();
    let subject = if commit.subject.is_empty() {
        "(no description)"
    } else {
        &commit.subject
    };
    let row_box = row.child().and_downcast::<GtkBox>();
    let id_cell = row_box
        .as_ref()
        .and_then(|b| b.first_child())
        .and_downcast::<Overlay>();
    let id_label = id_cell.as_ref().and_then(|c| c.child()).and_downcast::<Label>();
    let subject_label = id_cell
        .as_ref()
        .and_then(|c| c.next_sibling())
        .and_downcast::<Label>();
    let pills = subject_label
        .as_ref()
        .and_then(|l| l.next_sibling())
        .and_downcast::<GtkBox>();
    let badge = row_box
        .as_ref()
        .and_then(|b| b.last_child())
        .and_downcast::<gtk::Image>();
    match (id_cell, id_label, subject_label, pills, badge) {
        (Some(id_cell), Some(id_label), Some(subject_label), Some(pills), Some(badge)) => {
            id_label.set_markup(&format!("<tt>{short}</tt>"));
            set_id_hash(&id_cell, &commit.id_hex());
            subject_label.set_text(subject);
            set_pills(&pills, refs);
            badge.set_visible(conflicted);
        }
        // Older row layout (or a freshly-created empty row): build it whole.
        _ => row.set_child(Some(&commit_row_box(commit, conflicted, refs))),
    }
}

/// Show one row per commit, **reusing** the existing `ListBoxRow` widgets in
/// place (updating their labels) and **hiding** — never removing — any surplus
/// rows.
///
/// This is load-bearing for drag-and-drop, not a micro-optimization. A drop
/// rewrites history and repopulates immediately afterwards, while GTK still
/// holds one of these rows as the drop-crossing target for the just-ended
/// gesture (the trash sits below the list, so dragging to it leaves a lower row
/// cached). GTK walks that cached widget's parent chain on the next pointer
/// event; if we had unparented it (by removing or clearing rows) the walk hits a
/// NULL parent and segfaults. Reusing rows and only *hiding* the surplus keeps
/// every row parented, so the walk always terminates. Hidden rows are reused
/// when the list grows again.
fn populate_rows(
    list: &ListBox,
    commits: &[CommitInfo],
    selectable: bool,
    conflicts: &HashSet<String>,
    refs: &BTreeMap<String, Vec<RefDecoration>>,
) {
    for (i, commit) in commits.iter().enumerate() {
        let row = list.row_at_index(i as i32).unwrap_or_else(|| {
            let row = ListBoxRow::new();
            // Trash rows aren't editable; they exist only to be dragged back out.
            row.set_selectable(selectable);
            row.set_activatable(selectable);
            list.append(&row);
            row
        });
        row.set_visible(true);
        set_row_commit(
            &row,
            commit,
            conflicts.contains(&commit.change_id_hex()),
            refs.get(&commit.id_hex()).map_or(&[], Vec::as_slice),
        );
    }
    // Hide surplus rows rather than removing them (see the note above).
    let mut i = commits.len() as i32;
    while let Some(extra) = list.row_at_index(i) {
        extra.set_visible(false);
        i += 1;
    }
}

/// Show the history commits in `list` (newest first), reusing rows. See
/// [`populate_rows`]. `refs` (commit hex → branch/tag names, from
/// `Repo::commit_refs`) supplies the pill decorations after each subject.
pub(crate) fn populate_list(
    list: &ListBox,
    commits: &[CommitInfo],
    conflicts: &HashSet<String>,
    refs: &BTreeMap<String, Vec<RefDecoration>>,
) {
    populate_rows(list, commits, true, conflicts, refs);
}

/// Add the `op-affected` highlight to every history row whose commit's change id
/// is in `affected` — used while hovering an "Edit history" dropdown entry, to
/// show which commit(s) that operation touched before committing to a jump.
pub(crate) fn highlight_affected(list: &ListBox, commits: &[CommitInfo], affected: &[String]) {
    for (i, commit) in commits.iter().enumerate() {
        if affected.iter().any(|c| *c == commit.change_id_hex()) {
            if let Some(row) = list.row_at_index(i as i32) {
                row.add_css_class("op-affected");
            }
        }
    }
}

/// Remove the `op-affected` highlight from every history row (on hover leave or
/// when the "Edit history" popover closes).
pub(crate) fn clear_highlight(list: &ListBox) {
    let mut i = 0;
    while let Some(row) = list.row_at_index(i) {
        row.remove_css_class("op-affected");
        i += 1;
    }
}

/// Fill the trash list with the session's dropped commits, reusing rows. When
/// empty, the scrolled list is hidden so the panel collapses to just its trash
/// icon (the icon still carries the drop target).
pub(crate) fn populate_trash(list: &ListBox, scroll: &ScrolledWindow, commits: &[CommitInfo]) {
    scroll.set_visible(!commits.is_empty());
    // No ref pills in the trash: a dropped commit was just cut out of its
    // branch, so a ref still naming it would only be confusing here.
    populate_rows(list, commits, false, &HashSet::new(), &BTreeMap::new());
}

/// Fill the working-copy list with one summary row per uncommitted entry (newest
/// first, the leaf `@` first), reusing rows and hiding the surplus — the same
/// drag-safe pattern as [`populate_rows`]. Each row is a single label, e.g.
/// "✏ Uncommitted changes — 2 files" (or "⚠ … conflicts in N files").
pub(crate) fn populate_wc(list: &ListBox, entries: &[WorkingCopyEntry]) {
    for (i, entry) in entries.iter().enumerate() {
        let row = list.row_at_index(i as i32).unwrap_or_else(|| {
            let label = Label::new(None);
            label.set_halign(gtk::Align::Start);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_margin_start(8);
            label.set_margin_end(8);
            label.set_margin_top(4);
            label.set_margin_bottom(4);
            let row = ListBoxRow::new();
            row.set_child(Some(&label));
            list.append(&row);
            row
        });
        row.set_visible(true);
        let n = entry.changed_files;
        let s = if n == 1 { "" } else { "s" };
        let text = if entry.has_conflict {
            format!("\u{26A0} Uncommitted changes \u{2014} conflicts in {n} file{s}")
        } else {
            format!("\u{270E} Uncommitted changes \u{2014} {n} file{s}")
        };
        if let Some(label) = row.child().and_downcast::<Label>() {
            label.set_text(&text);
        }
    }
    let mut i = entries.len() as i32;
    while let Some(extra) = list.row_at_index(i) {
        extra.set_visible(false);
        i += 1;
    }
}
