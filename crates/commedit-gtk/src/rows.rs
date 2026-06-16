//! Commit and working-copy list rows: building a row's content box, updating it
//! in place, and the drag-safe `populate_*` refreshers. The "reuse rows, hide —
//! never unparent — the surplus" discipline in `populate_rows` is load-bearing
//! for drag-and-drop safety; see its doc comment.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

use commedit_engine::graph::GraphLayout;
use commedit_engine::history::CommitInfo;
use commedit_engine::transparency::{RefDecoration, RefKind};
use commedit_engine::workcopy::WorkingCopyEntry;
use gtk::prelude::*;
use gtk::{Box as GtkBox, Label, ListBox, ListBoxRow, Orientation, Overlay, ScrolledWindow};

use crate::msglint::{self, RepoStyle};
use crate::state::{LintFixCallback, MergeOutCallback, RestoreToWorktreeCallback, RevertCallback};

/// The history list's ancestry-graph layout, shared between `build_ui`'s refresh
/// (which recomputes it) and every row's drawing area (which reads its own row).
pub(crate) type SharedGraph = Rc<RefCell<GraphLayout>>;

/// Width of one graph lane column, px.
const LANE_W: f64 = 12.0;
/// Radius of a commit's node circle.
const NODE_R: f64 = 3.5;
/// Stroke width of the ancestry lines.
const EDGE_W: f64 = 2.0;

/// Per-lane line colors (cycled): saturated enough to stay readable on both
/// light/dark themes and on the tinted selection / squash-target row backgrounds.
const LANE_COLORS: [(f64, f64, f64); 8] = [
    (0.21, 0.52, 0.89),
    (0.18, 0.76, 0.49),
    (0.85, 0.65, 0.13),
    (0.88, 0.11, 0.14),
    (0.61, 0.35, 0.71),
    (0.10, 0.74, 0.61),
    (0.90, 0.49, 0.13),
    (0.55, 0.55, 0.55),
];

/// The line color of `lane`, cycled through the palette. Shared with the
/// drag-and-drop lane picker so its swatches match the drawn graph.
pub(crate) fn lane_color(lane: usize) -> (f64, f64, f64) {
    LANE_COLORS[lane % LANE_COLORS.len()]
}

/// Pixel width of the graph column: uniform across rows (the layout's widest
/// point) so the lane columns align down the list.
fn graph_width(layout: &GraphLayout) -> i32 {
    (layout.max_lanes.max(1) as f64 * LANE_W) as i32
}

/// Build the per-row ancestry drawing area. The draw func captures the shared
/// layout plus this row's **creation index** and reads `rows[index]` at draw
/// time — valid because `populate_rows` always shows `commits[i]` in the i-th
/// appended row and never unparents (so a row keeps its index for life); a
/// hidden surplus row indexes past the layout and draws nothing.
///
/// Each row draws edge-to-edge: incoming edges from the top edge to the vertical
/// center, the node at the center, outgoing edges down to the bottom edge — so
/// adjacent rows' lines connect without any cross-row drawing state. Colors
/// follow `compute_graph`'s contract (above-edges by `from` lane, below-edges by
/// `to` lane) to keep a line's color continuous across rows.
fn graph_area(graph: &SharedGraph, index: usize) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(graph_width(&graph.borrow()));
    // Purely decorative: keep GTK's pointer/drop picking on the row itself.
    area.set_can_target(false);
    let graph = graph.clone();
    area.set_draw_func(move |_, cr, _w, h| {
        let layout = graph.borrow();
        let Some(row) = layout.rows.get(index) else {
            return;
        };
        let h = h as f64;
        let mid = h / 2.0;
        let x = |lane: usize| LANE_W * (lane as f64 + 0.5);
        let set_color = |lane: usize| {
            let (r, g, b) = lane_color(lane);
            cr.set_source_rgb(r, g, b);
        };
        cr.set_line_width(EDGE_W);
        for &(from, to) in &row.edges_above {
            set_color(from);
            cr.move_to(x(from), 0.0);
            if from == to {
                cr.line_to(x(to), mid);
            } else {
                cr.curve_to(x(from), mid * 0.7, x(to), mid * 0.3, x(to), mid);
            }
            let _ = cr.stroke();
        }
        for &(from, to) in &row.edges_below {
            set_color(to);
            cr.move_to(x(from), mid);
            if from == to {
                cr.line_to(x(to), h);
            } else {
                cr.curve_to(
                    x(from),
                    mid + (h - mid) * 0.7,
                    x(to),
                    mid + (h - mid) * 0.3,
                    x(to),
                    h,
                );
            }
            let _ = cr.stroke();
        }
        cr.arc(x(row.node_lane), mid, NODE_R, 0.0, std::f64::consts::TAU);
        if row.is_merge {
            // A merge's node is a black disc, an ordinary commit's a lane-colored one.
            cr.set_source_rgb(0.0, 0.0, 0.0);
        } else {
            set_color(row.node_lane);
        }
        let _ = cr.fill();
    });
    area
}

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

/// Float a revert button over `content`'s right edge — the row's right boundary, so
/// the buttons line up down the list and a wide subject simply scrolls underneath —
/// revealed on hover, the same non-measured-overlay + hover pattern as [`id_cell`]'s
/// copy icon. Clicking it claims the press (so it never selects the row) and calls
/// `on_revert` with the row's current display index, which drops a revert of that
/// commit directly on top of it. The button stays hidden while `content` is tagged
/// `no-revert` (merge commits, set by [`commit_row_box`]/[`set_row_commit`]), since
/// a merge has no single parent to invert.
fn add_revert_button(content: &Overlay, on_revert: &RevertCallback) {
    let btn = gtk::Image::from_icon_name("edit-undo-symbolic");
    // An explicit pixel size is required: as a non-measured overlay child the
    // icon would otherwise request zero size and never show.
    btn.set_pixel_size(16);
    btn.set_halign(gtk::Align::End);
    btn.set_valign(gtk::Align::Center);
    // Keep the icon clear of the list's right edge / scrollbar.
    btn.set_margin_end(8);
    btn.set_cursor_from_name(Some("pointer"));
    btn.set_tooltip_text(Some("Revert this commit (drops a revert on top of it)"));
    btn.add_css_class("commit-revert");
    btn.set_visible(false);
    content.add_overlay(&btn);

    // Reveal the button only while the pointer is over the row content, and never
    // on a merge row (the `no-revert` class).
    let motion = gtk::EventControllerMotion::new();
    motion.connect_enter({
        let btn = btn.clone();
        let content = content.clone();
        move |_, _, _| btn.set_visible(!content.has_css_class("no-revert"))
    });
    motion.connect_leave({
        let btn = btn.clone();
        move |_| btn.set_visible(false)
    });
    content.add_controller(motion);

    // Clicking the button reverts the commit. Claim the press so the click stays
    // off the row: unlike clicking the row, it must not select the commit.
    let click = gtk::GestureClick::new();
    click.connect_pressed(|gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    click.connect_released({
        let btn = btn.clone();
        let on_revert = on_revert.clone();
        move |_, _, _, _| {
            if let Some(row) = btn
                .ancestor(ListBoxRow::static_type())
                .and_downcast::<ListBoxRow>()
            {
                on_revert(row.index());
            }
        }
    });
    btn.add_controller(click);
}

/// Float a merge-out button (a right arrow) over `content`'s right edge, *beside*
/// the revert button (to its left), revealed on hover with the same
/// non-measured-overlay pattern as [`add_revert_button`]. Clicking it claims the
/// press (so it never selects the row) and calls `on_merge_out` with the row's
/// current display index, which introduces a merge commit directly above that
/// commit (the commit becomes a side branch the merge folds back). It stays hidden
/// while `content` is tagged `no-merge-out` (merge commits *and* the root, set by
/// [`commit_row_box`]/[`set_row_commit`]) — only a single-parent commit has a
/// mainline parent to fold out around.
fn add_merge_out_button(content: &Overlay, on_merge_out: &MergeOutCallback) {
    let btn = gtk::Image::from_icon_name("go-next-symbolic");
    // An explicit pixel size is required: as a non-measured overlay child the
    // icon would otherwise request zero size and never show.
    btn.set_pixel_size(16);
    btn.set_halign(gtk::Align::End);
    btn.set_valign(gtk::Align::Center);
    // Sit just left of the revert button (at margin_end 8): 8 + its 16px icon + a
    // small gap, so the two line up at the row's right edge without overlapping.
    btn.set_margin_end(28);
    btn.set_cursor_from_name(Some("pointer"));
    btn.set_tooltip_text(Some(
        "Introduce a merge above this commit (it becomes a side branch)",
    ));
    btn.add_css_class("commit-revert");
    btn.set_visible(false);
    content.add_overlay(&btn);

    // Reveal the button only while the pointer is over the row content, and never
    // on a merge or root row (the `no-merge-out` class).
    let motion = gtk::EventControllerMotion::new();
    motion.connect_enter({
        let btn = btn.clone();
        let content = content.clone();
        move |_, _, _| btn.set_visible(!content.has_css_class("no-merge-out"))
    });
    motion.connect_leave({
        let btn = btn.clone();
        move |_| btn.set_visible(false)
    });
    content.add_controller(motion);

    // Claim the press so the click stays off the row (it must not select it).
    let click = gtk::GestureClick::new();
    click.connect_pressed(|gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    click.connect_released({
        let btn = btn.clone();
        let on_merge_out = on_merge_out.clone();
        move |_, _, _, _| {
            if let Some(row) = btn
                .ancestor(ListBoxRow::static_type())
                .and_downcast::<ListBoxRow>()
            {
                on_merge_out(row.index());
            }
        }
    });
    btn.add_controller(click);
}

/// Float a restore button over a *trash* row's right edge — the same
/// non-measured-overlay + hover pattern as [`add_revert_button`], so it lines up
/// down the list. Clicking it claims the press (so it never selects the row) and
/// calls `on_restore` with the row's current display index, which writes that
/// dropped commit's changes to the working tree as uncommitted edits and removes
/// it from the trash. Trashed commits are never merges, so — unlike the revert
/// button — there is no `no-revert` suppression.
fn add_restore_button(content: &Overlay, on_restore: &RestoreToWorktreeCallback) {
    let btn = gtk::Image::from_icon_name("go-bottom-symbolic");
    // An explicit pixel size is required: as a non-measured overlay child the
    // icon would otherwise request zero size and never show.
    btn.set_pixel_size(16);
    btn.set_halign(gtk::Align::End);
    btn.set_valign(gtk::Align::Center);
    // Keep the icon clear of the list's right edge / scrollbar.
    btn.set_margin_end(8);
    btn.set_cursor_from_name(Some("pointer"));
    btn.set_tooltip_text(Some(
        "Restore this commit's changes to the working tree (unstaged)",
    ));
    btn.add_css_class("commit-revert");
    btn.set_visible(false);
    content.add_overlay(&btn);

    // Reveal the button only while the pointer is over the row content.
    let motion = gtk::EventControllerMotion::new();
    motion.connect_enter({
        let btn = btn.clone();
        move |_, _, _| btn.set_visible(true)
    });
    motion.connect_leave({
        let btn = btn.clone();
        move |_| btn.set_visible(false)
    });
    content.add_controller(motion);

    // Claim the press so the click stays off the row (it must not select it).
    let click = gtk::GestureClick::new();
    click.connect_pressed(|gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
    });
    click.connect_released({
        let btn = btn.clone();
        let on_restore = on_restore.clone();
        move |_, _, _, _| {
            if let Some(row) = btn
                .ancestor(ListBoxRow::static_type())
                .and_downcast::<ListBoxRow>()
            {
                on_restore(row.index());
            }
        }
    });
    btn.add_controller(click);
}

/// Build the commit-style "smiley" badge — a clickable 🤔 [`Label`] shown inline
/// **left of the subject** (the caller appends it between the id cell and the
/// subject text), so a flagged summary reads as prefixed by the 🤔 rather than
/// having it float at the row's right edge where the revert/merge-out hover buttons
/// would cover a long, ellipsized subject. It is **persistent** (shown whenever
/// [`update_lint_badge`] finds the summary drifts from the repo's de-facto
/// conventions, see [`crate::msglint`], not only on hover) and collapses to zero
/// width while hidden, so a conforming summary sits flush after the id. Clicking it
/// claims the press (so it never selects/drags the row, like the id cell's copy
/// icon) and calls `on_lint` with the row's current display index, which auto-fixes
/// the mechanical issues or opens the commit for a manual edit. Inert (never wired,
/// stays hidden) on rows with no `on_lint` — trash rows.
fn build_lint_badge(on_lint: Option<&LintFixCallback>) -> Label {
    let badge = Label::new(None);
    badge.set_visible(false);
    badge.set_valign(gtk::Align::Center);
    badge.set_cursor_from_name(Some("pointer"));
    badge.add_css_class("commit-lint");
    if let Some(on_lint) = on_lint {
        let click = gtk::GestureClick::new();
        click.connect_pressed(|gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        click.connect_released({
            let badge = badge.clone();
            let on_lint = on_lint.clone();
            move |_, _, _, _| {
                if let Some(row) = badge
                    .ancestor(ListBoxRow::static_type())
                    .and_downcast::<ListBoxRow>()
                {
                    on_lint(row.index());
                }
            }
        });
        badge.add_controller(click);
    }
    badge
}

/// Refresh a row's lint badge for `commit` against the repo's learned `style`:
/// show the 🤔 with a tooltip listing the drift (and what a click will do) when the
/// summary is out of step, hide it otherwise. Merge commits and missing styles
/// (small sample / undecided) are never flagged.
fn update_lint_badge(badge: &Label, commit: &CommitInfo, style: Option<&RepoStyle>) {
    let lints = match style {
        Some(style) if commit.parents.len() <= 1 => msglint::lint_subject(&commit.subject, style),
        _ => Vec::new(),
    };
    if lints.is_empty() {
        badge.set_visible(false);
        badge.set_tooltip_text(None);
        return;
    }
    let fixable = lints.iter().any(|l| l.auto_fixable());
    let mut tip = String::from("This summary is out of step with the repo's commit style:");
    for lint in &lints {
        tip.push_str("\n\u{2022} ");
        tip.push_str(&lint.message);
    }
    tip.push('\n');
    tip.push_str(if fixable {
        "Click to fix it automatically."
    } else {
        "Click to edit the message."
    });
    badge.set_text("\u{1F914}"); // 🤔
    badge.set_tooltip_text(Some(&tip));
    badge.set_visible(true);
}

/// The subject text shown in a row: the commit's subject, or a placeholder when
/// it has no description. Shared so the row build, in-place update, and the
/// search highlight/reset all agree on the placeholder.
fn display_subject(commit: &CommitInfo) -> &str {
    if commit.subject.is_empty() {
        "(no description)"
    } else {
        &commit.subject
    }
}

/// The subject `Label` of a row, via the shared
/// `[graph?, content-overlay → row_box → (id_cell, lint_badge, subject_label, …)]`
/// layout. Mirrors the subject leg of [`set_row_commit`]'s traversal (the lint
/// badge sits between the id cell and the subject); `None` for a row that hasn't
/// been populated yet.
fn row_subject_label(row: &ListBoxRow) -> Option<Label> {
    let child = row.child().and_downcast::<GtkBox>();
    let area = child
        .as_ref()
        .and_then(|b| b.first_child())
        .and_downcast::<gtk::DrawingArea>();
    let content_overlay = match &area {
        Some(area) => area.next_sibling().and_downcast::<Overlay>(),
        None => child
            .as_ref()
            .and_then(|b| b.first_child())
            .and_downcast::<Overlay>(),
    };
    let row_box = content_overlay
        .as_ref()
        .and_then(|c| c.child())
        .and_downcast::<GtkBox>();
    let id_cell = row_box
        .as_ref()
        .and_then(|b| b.first_child())
        .and_downcast::<Overlay>();
    let lint_badge = id_cell
        .as_ref()
        .and_then(|c| c.next_sibling())
        .and_downcast::<Label>();
    lint_badge
        .as_ref()
        .and_then(|c| c.next_sibling())
        .and_downcast::<Label>()
}

/// Build the `short-id   subject  [pills]   ⚠` content box shown inside a
/// history/trash row. The pill box hugs the subject's right edge: the subject
/// label does *not* expand, the pill box does (start-aligned), so the pills sit
/// right after the text and the slack stays empty. The trailing warning icon is
/// present but hidden unless `conflicted`.
///
/// The content box is always wrapped in an [`Overlay`] inside a margin-free outer
/// box, so a right-edge action button can float over it (aligned down the list,
/// overlapping only a wide subject — the same hover pattern as the id cell's copy
/// icon): a *revert* button on a history row when `on_revert` is `Some`, a
/// *restore* button on a trash row when `on_restore` is `Some`. The persistent
/// *lint badge* is **not** a right-edge overlay — it is an inline row-box cell
/// between the id and the subject (so the hover buttons can't cover it), shown when
/// the summary drifts from the repo style. With `graph` (the
/// shared layout and this row's index — history rows only, trash rows pass
/// `None`) the ancestry drawing area leads the outer box: the graph lines must
/// reach the row's top/bottom edges to connect across rows, which the content
/// box's vertical margins would otherwise gap. Both row kinds share this
/// outer+overlay shape so [`set_row_commit`]'s traversal stays uniform.
#[allow(clippy::too_many_arguments)]
fn commit_row_box(
    commit: &CommitInfo,
    conflicted: bool,
    refs: &[RefDecoration],
    graph: Option<(&SharedGraph, usize)>,
    on_revert: Option<&RevertCallback>,
    on_merge_out: Option<&MergeOutCallback>,
    on_restore: Option<&RestoreToWorktreeCallback>,
    on_lint: Option<&LintFixCallback>,
    style: Option<&RepoStyle>,
) -> GtkBox {
    let short = commit.id_hex().chars().take(8).collect::<String>();
    let subject = display_subject(commit);
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
    // The lint badge sits between the id and the subject, so a flagged summary reads
    // as prefixed by the 🤔 and a conforming one is flush after the id (a hidden box
    // child takes no space). Inline, not a right-edge overlay, so the revert/merge-out
    // hover buttons can't cover it on a long subject.
    let lint_badge = build_lint_badge(on_lint);
    update_lint_badge(&lint_badge, commit, style);
    row_box.append(&lint_badge);
    row_box.append(&subject_label);
    row_box.append(&pills);
    row_box.append(&badge);
    // Wrap the content in an Overlay so an action button can float at the row's
    // right edge; both history and trash rows get this so `set_row_commit`'s
    // traversal is uniform.
    let content = Overlay::new();
    content.set_hexpand(true);
    content.set_child(Some(&row_box));
    if let Some(on_revert) = on_revert {
        add_revert_button(&content, on_revert);
    }
    if let Some(on_merge_out) = on_merge_out {
        add_merge_out_button(&content, on_merge_out);
    }
    if let Some(on_restore) = on_restore {
        add_restore_button(&content, on_restore);
    }
    // Suppress the right-edge buttons where they have no valid target: a merge has
    // no single parent to invert (revert), and a merge or the root has no mainline
    // parent to fold out around (merge-out). Mirrored by `set_row_commit`'s in-place
    // update so a row toggled into/out of a merge by a rewrite stays consistent.
    if commit.parents.len() > 1 {
        content.add_css_class("no-revert");
    }
    if commit.parents.len() != 1 {
        content.add_css_class("no-merge-out");
    }
    let outer = GtkBox::new(Orientation::Horizontal, 0);
    if let Some((graph, index)) = graph {
        // The lane column supplies the leading whitespace; the graph lines must
        // reach the row edges, so the content box drops its leading margin.
        row_box.set_margin_start(4);
        row_box.set_hexpand(true);
        outer.append(&graph_area(graph, index));
    }
    outer.append(&content);
    outer
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
        label.set_tooltip_text(Some(&match (r.kind, r.current) {
            (RefKind::Branch, true) => format!("Branch {} (checked out)", r.name),
            (RefKind::Branch, false) => format!("Branch {}", r.name),
            (RefKind::Tag, _) => format!("Tag {}", r.name),
        }));
        label.remove_css_class("ref-branch");
        label.remove_css_class("ref-tag");
        label.remove_css_class("ref-current");
        label.add_css_class(match r.kind {
            RefKind::Branch => "ref-branch",
            RefKind::Tag => "ref-tag",
        });
        // The checked-out branch gets an extra class, layered over `ref-branch`.
        if r.current {
            label.add_css_class("ref-current");
        }
    }
    while let Some(extra) = next {
        next = extra.next_sibling();
        extra.set_visible(false);
    }
}

/// Update the content of a row's existing labels in place, without replacing the
/// child widget — so the labels (and the row) survive a drag-triggered rebuild.
/// Falls back to building a fresh child if the row has none yet.
#[allow(clippy::too_many_arguments)]
fn set_row_commit(
    row: &ListBoxRow,
    commit: &CommitInfo,
    conflicted: bool,
    refs: &[RefDecoration],
    graph: Option<(&SharedGraph, usize)>,
    on_revert: Option<&RevertCallback>,
    on_merge_out: Option<&MergeOutCallback>,
    on_restore: Option<&RestoreToWorktreeCallback>,
    on_lint: Option<&LintFixCallback>,
    style: Option<&RepoStyle>,
) {
    let short = commit.id_hex().chars().take(8).collect::<String>();
    let subject = display_subject(commit);
    let child = row.child().and_downcast::<GtkBox>();
    // Every row's child is the outer `[graph area?, content overlay]` box from
    // [`commit_row_box`]: a history row leads with the ancestry drawing area, a
    // trash row omits it (no graph) but keeps the content overlay so its restore
    // button can float at the right edge. The content overlay wraps the content
    // box either way.
    let area = child
        .as_ref()
        .and_then(|b| b.first_child())
        .and_downcast::<gtk::DrawingArea>();
    let content_overlay = match &area {
        Some(area) => area.next_sibling().and_downcast::<Overlay>(),
        None => child
            .as_ref()
            .and_then(|b| b.first_child())
            .and_downcast::<Overlay>(),
    };
    let row_box = content_overlay
        .as_ref()
        .and_then(|c| c.child())
        .and_downcast::<GtkBox>();
    let id_cell = row_box
        .as_ref()
        .and_then(|b| b.first_child())
        .and_downcast::<Overlay>();
    let id_label = id_cell
        .as_ref()
        .and_then(|c| c.child())
        .and_downcast::<Label>();
    // The lint badge sits between the id cell and the subject (see `commit_row_box`).
    let lint_badge = id_cell
        .as_ref()
        .and_then(|c| c.next_sibling())
        .and_downcast::<Label>();
    let subject_label = lint_badge
        .as_ref()
        .and_then(|c| c.next_sibling())
        .and_downcast::<Label>();
    let pills = subject_label
        .as_ref()
        .and_then(|c| c.next_sibling())
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
            // Suppress the right-edge buttons where they have no valid target: a
            // merge has no single parent to invert (revert), and a merge or the root
            // has no mainline parent to fold out around (merge-out). The content
            // overlay hosts both; trash rows have none. Mirrors `commit_row_box`.
            if let Some(content) = &content_overlay {
                if commit.parents.len() > 1 {
                    content.add_css_class("no-revert");
                } else {
                    content.remove_css_class("no-revert");
                }
                if commit.parents.len() != 1 {
                    content.add_css_class("no-merge-out");
                } else {
                    content.remove_css_class("no-merge-out");
                }
            }
            // The lint badge is a row_box cell between the id and subject — refresh
            // it for this commit.
            if let Some(lint_badge) = &lint_badge {
                update_lint_badge(lint_badge, commit, style);
            }
            set_pills(&pills, refs);
            badge.set_visible(conflicted);
            if let (Some(area), Some((graph, _))) = (&area, graph) {
                // The shared layout was just recomputed: re-fit the lane column
                // and repaint this row's slice of it.
                area.set_content_width(graph_width(&graph.borrow()));
                area.queue_draw();
            }
        }
        // Older row layout (or a freshly-created empty row): build it whole.
        _ => row.set_child(Some(&commit_row_box(
            commit,
            conflicted,
            refs,
            graph,
            on_revert,
            on_merge_out,
            on_restore,
            on_lint,
            style,
        ))),
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
#[allow(clippy::too_many_arguments)]
fn populate_rows(
    list: &ListBox,
    commits: &[CommitInfo],
    selectable: bool,
    conflicts: &HashSet<String>,
    refs: &BTreeMap<String, Vec<RefDecoration>>,
    graph: Option<&SharedGraph>,
    on_revert: Option<&RevertCallback>,
    on_merge_out: Option<&MergeOutCallback>,
    on_restore: Option<&RestoreToWorktreeCallback>,
    on_lint: Option<&LintFixCallback>,
    style: Option<&RepoStyle>,
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
            graph.map(|g| (g, i)),
            on_revert,
            on_merge_out,
            on_restore,
            on_lint,
            style,
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
/// `Repo::commit_refs`) supplies the pill decorations after each subject;
/// `graph` (from `compute_graph` over the same commits) the ancestry lines.
#[allow(clippy::too_many_arguments)]
pub(crate) fn populate_list(
    list: &ListBox,
    commits: &[CommitInfo],
    conflicts: &HashSet<String>,
    refs: &BTreeMap<String, Vec<RefDecoration>>,
    graph: &SharedGraph,
    on_revert: Option<&RevertCallback>,
    on_merge_out: Option<&MergeOutCallback>,
    on_lint: Option<&LintFixCallback>,
    style: Option<&RepoStyle>,
) {
    populate_rows(
        list,
        commits,
        true,
        conflicts,
        refs,
        Some(graph),
        on_revert,
        on_merge_out,
        None,
        on_lint,
        style,
    );
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

/// Paint the search matches of `query` across the history `list` (matching the
/// commit subjects by substring term, see [`crate::search::search_match`]) and
/// return the matched row indices, ascending. A matched subject's characters are
/// highlighted via [`crate::search::highlight_markup`]; every other row — and
/// every row when `query` is empty, the "search cleared" path — is reset to plain
/// text. Mirrors the row-walking style of
/// [`highlight_affected`] / [`clear_highlight`].
pub(crate) fn apply_search_highlight(
    list: &ListBox,
    commits: &[CommitInfo],
    query: &str,
) -> Vec<usize> {
    let mut matches = Vec::new();
    for (i, commit) in commits.iter().enumerate() {
        let Some(row) = list.row_at_index(i as i32) else {
            break;
        };
        let Some(label) = row_subject_label(&row) else {
            continue;
        };
        match crate::search::search_match(&commit.subject, query) {
            Some(pos) => {
                label.set_markup(&crate::search::highlight_markup(
                    display_subject(commit),
                    &pos,
                ));
                matches.push(i);
            }
            None => label.set_text(display_subject(commit)),
        }
    }
    matches
}

/// Scroll `scroll` so the row at display index `idx` in `list` is visible. The
/// row's bounds are taken in `list` coordinates — the same space the scrolled
/// window's vertical adjustment uses, since `list` is its scroll child — and the
/// adjustment is nudged the minimum needed to bring the row into the page. No-op
/// if the row isn't present or laid out yet.
pub(crate) fn scroll_row_into_view(scroll: &ScrolledWindow, list: &ListBox, idx: usize) {
    let Some(row) = list.row_at_index(idx as i32) else {
        return;
    };
    let Some(bounds) = row.compute_bounds(list) else {
        return;
    };
    let adj = scroll.vadjustment();
    let top = bounds.y() as f64;
    let bottom = top + bounds.height() as f64;
    let page = adj.page_size();
    if top < adj.value() {
        adj.set_value(top.max(adj.lower()));
    } else if bottom > adj.value() + page {
        adj.set_value((bottom - page).clamp(adj.lower(), (adj.upper() - page).max(adj.lower())));
    }
}

/// Fill the trash list with the session's dropped commits, reusing rows. When
/// empty, the scrolled list is hidden so the panel collapses to just its trash
/// icon (the icon still carries the drop target). `on_restore`, when `Some`, adds
/// a hover button to each row that writes the commit's changes to the working
/// tree (passed `None` in conflict mode, where restoring is blocked).
pub(crate) fn populate_trash(
    list: &ListBox,
    scroll: &ScrolledWindow,
    commits: &[CommitInfo],
    on_restore: Option<&RestoreToWorktreeCallback>,
) {
    scroll.set_visible(!commits.is_empty());
    // No ref pills and no ancestry graph in the trash: a dropped commit was just
    // cut out of its branch, so neither applies here. No revert or merge-out button
    // either — a trashed commit isn't part of the branch — but a restore button
    // (when `on_restore` is given) pulls its changes into the working tree.
    populate_rows(
        list,
        commits,
        false,
        &HashSet::new(),
        &BTreeMap::new(),
        None,
        None,
        None,
        on_restore,
        None,
        None,
    );
}

/// Summarize an entry's changed files as up to two basenames plus a count of the
/// rest, e.g. "main.rs", "main.rs, lib.rs" or "main.rs, lib.rs (+3 more)".
fn summarize_files(names: &[String]) -> String {
    let basenames: Vec<&str> = names
        .iter()
        .take(2)
        .map(|p| p.rsplit('/').next().unwrap_or(p))
        .collect();
    let mut summary = basenames.join(", ");
    let more = names.len().saturating_sub(2);
    if more > 0 {
        summary.push_str(&format!(" (+{more} more)"));
    }
    summary
}

/// Fill the working-copy list with one summary row per uncommitted entry (newest
/// first, the leaf `@` first), reusing rows and hiding the surplus — the same
/// drag-safe pattern as [`populate_rows`]. Each row is a single label, e.g.
/// "✏ Uncommitted changes — main.rs, lib.rs (+3 more)" (or "⚠ … conflicts in …").
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
        let files = summarize_files(&entry.file_names);
        let text = if entry.has_conflict {
            format!("\u{26A0} Uncommitted changes \u{2014} conflicts in {files}")
        } else {
            format!("\u{270E} Uncommitted changes \u{2014} {files}")
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
