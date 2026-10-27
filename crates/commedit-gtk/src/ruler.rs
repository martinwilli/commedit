//! The commit-message ruler, made draggable. GtkSourceView paints its right
//! margin as a plain line with no interaction of its own, so this adds one: a
//! capture-phase drag gesture claims a press on the ruler and moves it column
//! by column, while an OSD bubble beside the pointer shows the count. A double
//! click puts it back at git's conventional column; hovering it turns the
//! cursor into a column resizer and offers a tooltip saying so.
//!
//! The ruler only grabs in empty space. Where a line's text reaches it, a press
//! is meant for the caret and passes through to the text view untouched.
//!
//! The position lives in the view itself (`right_margin_position`), in columns
//! rather than pixels, so the text-size dropdown rescales it for free. GTK-only;
//! there is no MCP/engine counterpart.

use std::cell::Cell;
use std::rc::Rc;

use gtk::prelude::*;
use sourceview5::prelude::ViewExt;

/// git's conventional body wrap column: the default, and what a double click
/// resets the ruler to.
pub(crate) const DEFAULT_COLUMN: u32 = 72;

/// The narrowest column a drag can take the ruler to.
const MIN_COLUMN: u32 = 10;

/// How far (px) either side of the ruler a press still grabs it.
const GRAB_PX: f64 = 4.0;

/// The gap (px) between the ruler and its column hint.
const HINT_GAP: i32 = 8;

/// Pixel width of `n` underscores in `view`'s font: how GtkSourceView measures
/// the margin's offset, so ours lands on the very same pixel.
fn underscores_width(view: &sourceview5::View, n: u32) -> i32 {
    view.create_pango_layout(Some(&"_".repeat(n as usize)))
        .pixel_size()
        .0
}

/// The width of one column, averaged over many so the per-glyph rounding of a
/// pixel size doesn't accumulate across a wide view.
fn column_width(view: &sourceview5::View) -> f64 {
    (f64::from(underscores_width(view, 100)) / 100.0).max(1.0)
}

/// The ruler's x in `view`'s widget coordinates. GtkSourceView paints it the
/// margin position's worth of underscores past the left margin, relative to the
/// visible area rather than the buffer.
fn ruler_x(view: &sourceview5::View) -> f64 {
    let visible = view.visible_rect();
    let bx =
        visible.x() + view.left_margin() + underscores_width(view, view.right_margin_position());
    let (x, _) = view.buffer_to_window_coords(gtk::TextWindowType::Widget, bx, 0);
    f64::from(x)
}

/// The column nearest to widget x, clamped to what the view shows so a drag
/// can't take the ruler out of sight.
fn column_at(view: &sourceview5::View, x: f64) -> u32 {
    let cw = column_width(view);
    let visible = view.visible_rect();
    let (bx, _) = view.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, 0);
    let col = f64::from(bx - visible.x() - view.left_margin()) / cw;
    let max = (f64::from(visible.width() - view.left_margin()) / cw).floor();
    col.round()
        .clamp(f64::from(MIN_COLUMN), max.max(f64::from(MIN_COLUMN))) as u32
}

/// Whether a press at widget (x, y) takes the ruler: it must be close enough,
/// and the line there must end short of it. That is probed in the middle of the
/// column just left of the ruler; a line with a character there runs up to (or
/// past) the ruler, and a press is meant for the caret.
fn grabbable(view: &sourceview5::View, x: f64, y: f64) -> bool {
    let rx = ruler_x(view);
    if (x - rx).abs() > GRAB_PX {
        return false;
    }
    let probe = rx - column_width(view) / 2.0;
    let (bx, by) =
        view.window_to_buffer_coords(gtk::TextWindowType::Widget, probe as i32, y as i32);
    view.iter_at_location(bx, by).is_none()
}

/// Make `view`'s right margin draggable. `scroll` is the view's scrolled window;
/// it gets wrapped in an overlay carrying the column hint, and the returned
/// overlay is what to pack in its place.
pub(crate) fn attach(view: &sourceview5::View, scroll: &impl IsA<gtk::Widget>) -> gtk::Overlay {
    let hint = gtk::Label::new(None);
    hint.add_css_class("osd");
    hint.add_css_class("ruler-hint");
    // The bubble follows the pointer, so it must never take the pointer from
    // the view underneath: the drag and its motion keep going to the view.
    hint.set_can_target(false);
    hint.set_visible(false);
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(scroll));
    overlay.add_overlay(&hint);

    // Where the hint points, in overlay coordinates: the ruler's x, the
    // pointer's y.
    let anchor = Rc::new(Cell::new((0, 0)));
    overlay.connect_get_child_position({
        let anchor = anchor.clone();
        move |overlay, child| {
            let (_, w, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
            let (_, h, _, _) = child.measure(gtk::Orientation::Vertical, w);
            let (ax, ay) = anchor.get();
            // Right of the ruler, unless the bubble would run off the edge.
            let x = if ax + HINT_GAP + w <= overlay.width() {
                ax + HINT_GAP
            } else {
                ax - HINT_GAP - w
            };
            let y = (ay - h / 2).clamp(0, (overlay.height() - h).max(0));
            Some(gtk::gdk::Rectangle::new(x.max(0), y, w, h))
        }
    });

    // Show the hint for the ruler's current column, level with widget y.
    let show_hint = Rc::new({
        let (view, overlay, hint) = (view.clone(), overlay.clone(), hint.clone());
        move |y: f64| {
            hint.set_text(&format!("{} columns", view.right_margin_position()));
            let at = gtk::graphene::Point::new(ruler_x(&view) as f32, y as f32);
            if let Some(p) = view.compute_point(&overlay, &at) {
                anchor.set((p.x() as i32, p.y() as i32));
            }
            hint.set_visible(true);
            overlay.queue_allocate();
        }
    });

    // The column the ruler was picked up at, while it is held.
    let held: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
    // When the last click (a press that left the ruler where it was) let go,
    // to spot the second half of a double click.
    let last_click: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));

    let drag = gtk::GestureDrag::new();
    // Ahead of GtkTextView's own gestures: claiming the press keeps it from
    // placing the caret or starting a selection, denying it lets it through.
    drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    drag.connect_drag_begin({
        let (view, held, show_hint) = (view.clone(), held.clone(), show_hint.clone());
        move |gesture, x, y| {
            if !grabbable(&view, x, y) {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            held.set(Some(view.right_margin_position()));
            // The hover tooltip may still be up; a fresh query finds the
            // ruler held and takes it down, leaving the hint to take over.
            view.trigger_tooltip_query();
            show_hint(y);
        }
    });
    drag.connect_drag_update({
        let (view, held) = (view.clone(), held.clone());
        move |gesture, dx, dy| {
            if held.get().is_none() {
                return;
            }
            let Some((x, y)) = gesture.start_point() else {
                return;
            };
            let col = column_at(&view, x + dx);
            if col != view.right_margin_position() {
                view.set_right_margin_position(col);
            }
            show_hint(y + dy);
        }
    });
    drag.connect_drag_end({
        let (view, held) = (view.clone(), held.clone());
        move |gesture, _, _| {
            let Some(picked) = held.take() else {
                return;
            };
            hint.set_visible(false);
            if picked != view.right_margin_position() {
                last_click.set(None);
                return;
            }
            // Reset on the second release, not its press: that would leave the
            // pointer off the ruler, and the drag's next motion would take the
            // ruler right back to it.
            let now = gesture.current_event_time();
            let double_click = gtk::Settings::default()
                .map_or(400, |s| s.gtk_double_click_time())
                .max(0) as u32;
            if last_click
                .take()
                .is_some_and(|t| now.wrapping_sub(t) <= double_click)
            {
                view.set_right_margin_position(DEFAULT_COLUMN);
            } else {
                last_click.set(Some(now));
            }
        }
    });
    view.add_controller(drag);

    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion({
        let (view, held) = (view.clone(), held.clone());
        move |_, x, y| {
            let want = if held.get().is_some() || grabbable(&view, x, y) {
                "col-resize"
            } else {
                "text"
            };
            // Compare rather than track a hover state of our own: GtkTextView
            // swaps the cursor itself, hiding it while typing and restoring its
            // "text" one on the next motion.
            if view.cursor().and_then(|c| c.name()).as_deref() != Some(want) {
                view.set_cursor_from_name(Some(want));
            }
        }
    });
    view.add_controller(motion);

    view.set_has_tooltip(true);
    view.connect_query_tooltip(move |view, x, y, keyboard, tooltip| {
        if keyboard || held.get().is_some() || !grabbable(view, f64::from(x), f64::from(y)) {
            return false;
        }
        tooltip.set_text(Some(&format!(
            "Ruler at {} columns: drag to move, double-click to reset to {DEFAULT_COLUMN}",
            view.right_margin_position()
        )));
        true
    });

    overlay
}
