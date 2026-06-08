//! Commit and working-copy list rows: building a row's content box, updating it
//! in place, and the drag-safe `populate_*` refreshers. The "reuse rows, hide —
//! never unparent — the surplus" discipline in `populate_rows` is load-bearing
//! for drag-and-drop safety; see its doc comment.

use std::collections::HashSet;

use commedit_engine::history::CommitInfo;
use commedit_engine::workcopy::WorkingCopyEntry;
use gtk::prelude::*;
use gtk::{Box as GtkBox, Label, ListBox, ListBoxRow, Orientation, ScrolledWindow};

/// Build the `short-id   subject   ⚠` content box shown inside a history/trash
/// row. The trailing warning icon is present but hidden unless `conflicted`.
fn commit_row_box(commit: &CommitInfo, conflicted: bool) -> GtkBox {
    let short = commit.id_hex().chars().take(8).collect::<String>();
    let subject = if commit.subject.is_empty() {
        "(no description)"
    } else {
        &commit.subject
    };
    let id_label = Label::builder().xalign(0.0).build();
    id_label.set_markup(&format!("<tt>{short}</tt>"));
    let subject_label = Label::builder()
        .label(subject)
        .xalign(0.0)
        .halign(gtk::Align::Fill)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
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
    row_box.append(&id_label);
    row_box.append(&subject_label);
    row_box.append(&badge);
    row_box
}

/// Update the content of a row's existing labels in place, without replacing the
/// child widget — so the labels (and the row) survive a drag-triggered rebuild.
/// Falls back to building a fresh child if the row has none yet.
fn set_row_commit(row: &ListBoxRow, commit: &CommitInfo, conflicted: bool) {
    let short = commit.id_hex().chars().take(8).collect::<String>();
    let subject = if commit.subject.is_empty() {
        "(no description)"
    } else {
        &commit.subject
    };
    let row_box = row.child().and_downcast::<GtkBox>();
    let id_label = row_box
        .as_ref()
        .and_then(|b| b.first_child())
        .and_downcast::<Label>();
    let subject_label = id_label
        .as_ref()
        .and_then(|l| l.next_sibling())
        .and_downcast::<Label>();
    let badge = row_box
        .as_ref()
        .and_then(|b| b.last_child())
        .and_downcast::<gtk::Image>();
    match (id_label, subject_label, badge) {
        (Some(id_label), Some(subject_label), Some(badge)) => {
            id_label.set_markup(&format!("<tt>{short}</tt>"));
            subject_label.set_text(subject);
            badge.set_visible(conflicted);
        }
        // Older row layout (or a freshly-created empty row): build it whole.
        _ => row.set_child(Some(&commit_row_box(commit, conflicted))),
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
        set_row_commit(&row, commit, conflicts.contains(&commit.change_id_hex()));
    }
    // Hide surplus rows rather than removing them (see the note above).
    let mut i = commits.len() as i32;
    while let Some(extra) = list.row_at_index(i) {
        extra.set_visible(false);
        i += 1;
    }
}

/// Show the history commits in `list` (newest first), reusing rows. See
/// [`populate_rows`].
pub(crate) fn populate_list(list: &ListBox, commits: &[CommitInfo], conflicts: &HashSet<String>) {
    populate_rows(list, commits, true, conflicts);
}

/// Fill the trash list with the session's dropped commits, reusing rows. When
/// empty, the scrolled list is hidden so the panel collapses to just its trash
/// icon (the icon still carries the drop target).
pub(crate) fn populate_trash(list: &ListBox, scroll: &ScrolledWindow, commits: &[CommitInfo]) {
    scroll.set_visible(!commits.is_empty());
    populate_rows(list, commits, false, &HashSet::new());
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
