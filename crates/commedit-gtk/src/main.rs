//! commedit GTK4 UI (Milestone 2): browse history, edit a commit message, and
//! edit the content of files a commit changes. Saving transparently rewrites the
//! commit and rebases descendants via the engine.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use commedit_engine::diff::{
    apply_patch, commit_changes, parse_diff_lines, render_diff, ChangeKind, ContextExpansion,
    DiffLineKind, FileChange, HunkInfo,
};
use commedit_engine::history::{history, plan_reorder, CommitInfo};
use commedit_engine::patch_edit::{
    deletion_is_safe, plan_edit, Cursor, EditGesture, EditPlan, PatchEdit, Selection,
};
use commedit_engine::repo::Repo;
use commedit_engine::rewrite::Identity;
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    gdk, Application, ApplicationWindow, Box as GtkBox, Button, Calendar, CallbackAction, DragSource,
    DropDown, DropTarget, Entry, EventControllerKey, EventControllerScroll,
    EventControllerScrollFlags, Grid, HeaderBar, Label, ListBox, ListBoxRow,
    MenuButton, Orientation, Paned, PolicyType, Popover, PropagationPhase, ScrolledWindow, Shortcut,
    ShortcutController, ShortcutTrigger, StringList, TextTag,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

const APP_ID: &str = "net.willi.commedit";

/// A reference-counted, re-entrant "render the current diff" callback. Boxed so
/// the embedded expand-context buttons can hold and invoke it after they widen a
/// hunk (the renderer rebuilds the buffer and the buttons themselves).
type Renderer = Rc<dyn Fn()>;

fn main() {
    let repo_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, repo_path.clone()));
    app.run_with_args::<&str>(&[]);
}

fn buffer_text(buffer: &sourceview5::Buffer) -> String {
    buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string()
}

/// Buffer iter at a structured-edit [`Cursor`] (line + character column).
fn iter_at(buffer: &sourceview5::Buffer, c: &Cursor) -> gtk::TextIter {
    buffer
        .iter_at_line_offset(c.line as i32, c.col as i32)
        .unwrap_or_else(|| buffer.end_iter())
}

/// The current selection (or a collapsed caret) as a structured-edit
/// [`Selection`].
fn buffer_selection(buffer: &sourceview5::Buffer) -> Selection {
    if let Some((s, e)) = buffer.selection_bounds() {
        Selection {
            anchor: Cursor {
                line: s.line() as usize,
                col: s.line_offset() as usize,
            },
            end: Cursor {
                line: e.line() as usize,
                col: e.line_offset() as usize,
            },
        }
    } else {
        let it = buffer.iter_at_offset(buffer.cursor_position());
        Selection::caret(Cursor {
            line: it.line() as usize,
            col: it.line_offset() as usize,
        })
    }
}

/// Apply a planned [`PatchEdit`] as a single undo step. The `editing` guard marks
/// the mutation as our own so the firewall signal handlers let it through.
fn apply_patch_edit(buffer: &sourceview5::Buffer, editing: &Rc<Cell<bool>>, edit: &PatchEdit) {
    editing.set(true);
    buffer.begin_user_action();
    let mut start = iter_at(buffer, &edit.start);
    let mut end = iter_at(buffer, &edit.end);
    buffer.delete(&mut start, &mut end);
    let mut at = iter_at(buffer, &edit.start);
    buffer.insert(&mut at, &edit.replacement);
    buffer.end_user_action();
    editing.set(false);
    let cursor = iter_at(buffer, &edit.cursor);
    buffer.place_cursor(&cursor);
}

/// The fraction of the visible height at which buffer `line`'s top currently
/// sits in `view` (0.0 = top edge, 1.0 = bottom). Used to keep a clicked hunk
/// header at the same place across a re-render. Falls back to a third down.
fn vertical_fraction_of_line(
    view: &sourceview5::View,
    buffer: &sourceview5::Buffer,
    line: usize,
) -> f64 {
    let Some(vadjustment) = view.vadjustment() else {
        return 0.3;
    };
    let page = vadjustment.page_size();
    if page <= 0.0 {
        return 0.3;
    }
    let Some(iter) = buffer.iter_at_line(line as i32) else {
        return 0.3;
    };
    let line_top = view.iter_location(&iter).y() as f64;
    ((line_top - vadjustment.value()) / page).clamp(0.0, 1.0)
}

fn change_label(change: &FileChange) -> String {
    let sigil = match change.kind {
        ChangeKind::Added => "+",
        ChangeKind::Modified => "~",
        ChangeKind::Removed => "-",
    };
    format!("{sigil} {}", change.path)
}

fn build_ui(app: &Application, repo_path: PathBuf) {
    let repo = match Repo::open(&repo_path) {
        Ok(repo) => Rc::new(RefCell::new(repo)),
        Err(err) => {
            present_error(app, &format!("Failed to open {repo_path:?}:\n{err:?}"));
            return;
        }
    };

    // Shared UI state.
    let commits: Rc<RefCell<Vec<CommitInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let selected_change: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let changes: Rc<RefCell<Vec<FileChange>>> = Rc::new(RefCell::new(Vec::new()));
    let current_file: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    // Per-file hunk context expansion, keyed by path. Reset when the selected
    // commit changes (see `load_changes`).
    let expansions: Rc<RefCell<HashMap<String, ContextExpansion>>> =
        Rc::new(RefCell::new(HashMap::new()));

    // Styling for drag-and-drop reordering: the insertion gap placeholder and the
    // dimmed row being dragged. Installed once for the display.
    if let Some(display) = gdk::Display::default() {
        let css = gtk::CssProvider::new();
        css.load_from_data(
            ".drop-placeholder { background-color: rgba(53, 132, 228, 0.22); \
             border: 1px dashed rgb(53, 132, 228); border-radius: 5px; margin: 1px 6px; } \
             row.commit-dragging { opacity: 0.35; }",
        );
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // --- History pane (left) ---
    let list = ListBox::new();
    let history_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vexpand(true)
        .width_request(480)
        .child(&list)
        .build();

    // --- Message pane (top-right) ---
    let message_buffer = sourceview5::Buffer::new(None);
    let message_view = sourceview5::View::with_buffer(&message_buffer);
    message_view.set_monospace(true);
    message_view.set_wrap_mode(gtk::WrapMode::WordChar);
    message_view.set_left_margin(8);
    message_view.set_top_margin(8);

    // Identity fields above the message editor: one combined "Name <email>"
    // field per role (with a built-in ▼ to pick an identity used elsewhere) and
    // a date field with a calendar button to its right.
    let author_id = identity_entry("Author — Name <email>");
    let author_date = identity_entry("YYYY-MM-DD HH:MM:SS ±HHMM");
    let committer_id = identity_entry("Committer — Name <email>");
    let committer_date = identity_entry("YYYY-MM-DD HH:MM:SS ±HHMM");
    author_date.set_width_chars(26);
    committer_date.set_width_chars(26);
    // Distinct identities harvested from history, offered by the in-field ▼.
    let identities: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    attach_identity_picker(&author_id, &identities);
    attach_identity_picker(&committer_id, &identities);

    let identity_grid = Grid::builder()
        .row_spacing(4)
        .column_spacing(6)
        .margin_start(8)
        .margin_top(8)
        .margin_end(8)
        .margin_bottom(4)
        .build();
    let author_label = Label::builder().label("Author").xalign(0.0).build();
    let committer_label = Label::builder().label("Committer").xalign(0.0).build();
    identity_grid.attach(&author_label, 0, 0, 1, 1);
    identity_grid.attach(&author_id, 1, 0, 1, 1);
    identity_grid.attach(&date_field(&author_date), 2, 0, 1, 1);
    identity_grid.attach(&committer_label, 0, 1, 1, 1);
    identity_grid.attach(&committer_id, 1, 1, 1, 1);
    identity_grid.attach(&date_field(&committer_date), 2, 1, 1, 1);

    let message_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&message_view)
        .build();
    let message_box = GtkBox::new(Orientation::Vertical, 0);
    message_box.append(&identity_grid);
    message_box.append(&message_scroll);

    // The original identity of the selected commit, to detect edits on save.
    let original_identity: Rc<RefCell<Option<Identity>>> = Rc::new(RefCell::new(None));
    // The identity entries in a fixed order (see `read_identity`):
    // [author "Name <email>", author date, committer "Name <email>", committer date].
    let identity_fields: [Entry; 4] = [
        author_id.clone(),
        author_date.clone(),
        committer_id.clone(),
        committer_date.clone(),
    ];

    // --- File / diff pane (bottom-right) ---
    let file_dropdown = DropDown::from_strings(&[]);
    // We render the diff with our own text tags (line backgrounds, syntect
    // language coloring, intra-line emphasis) rather than a GtkSourceView
    // grammar, so no language is set on the buffer.
    let file_buffer = sourceview5::Buffer::new(None);
    install_diff_tags(&file_buffer);
    let file_view = sourceview5::View::with_buffer(&file_buffer);
    file_view.set_monospace(true);
    file_view.set_left_margin(8);
    file_view.set_top_margin(8);
    // Set while we mutate the diff buffer ourselves (loading a file, or applying
    // a structured edit) so the firewall signal handlers below let it through
    // instead of treating it as an interactive edit.
    let editing = Rc::new(Cell::new(false));
    let file_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&file_view)
        .build();
    let files_box = GtkBox::new(Orientation::Vertical, 0);
    file_dropdown.set_margin_start(8);
    file_dropdown.set_margin_top(0);
    file_dropdown.set_margin_end(8);
    file_dropdown.set_margin_bottom(4);
    // Transient feedback line for blocked edits and save errors.
    let status_label = Label::builder()
        .xalign(0.0)
        .margin_start(8)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(4)
        .wrap(true)
        .build();
    status_label.add_css_class("dim-label");
    status_label.set_visible(false);

    // Action bar along the bottom of the file pane. The Save button is
    // right-aligned behind a flexible spacer, which leaves room for more
    // buttons later. Living inside `files_box` keeps it only as wide as the
    // file/diff editing field rather than spanning the whole window.
    let save_button = Button::with_label("Save");
    save_button.add_css_class("suggested-action");
    let bottom_bar = GtkBox::new(Orientation::Horizontal, 4);
    bottom_bar.set_margin_start(8);
    bottom_bar.set_margin_end(8);
    bottom_bar.set_margin_top(4);
    bottom_bar.set_margin_bottom(8);
    let bottom_spacer = GtkBox::new(Orientation::Horizontal, 0);
    bottom_spacer.set_hexpand(true);
    bottom_bar.append(&bottom_spacer);
    bottom_bar.append(&save_button);

    files_box.append(&file_dropdown);
    files_box.append(&file_scroll);
    files_box.append(&status_label);
    files_box.append(&bottom_bar);

    let right_paned = Paned::builder()
        .orientation(Orientation::Vertical)
        .start_child(&message_box)
        .end_child(&files_box)
        .position(200)
        .build();

    let paned = Paned::builder()
        .orientation(Orientation::Horizontal)
        .start_child(&history_scroll)
        .end_child(&right_paned)
        .position(480)
        .build();

    let root = GtkBox::new(Orientation::Vertical, 0);
    paned.set_vexpand(true);
    root.append(&paned);

    // An (otherwise empty) header bar keeps the window title and the window
    // controls; the Save action now lives in the bottom action bar.
    let header = HeaderBar::new();

    // Title with the repository folder name, e.g. "Commit editor - commedit".
    let folder = repo
        .borrow()
        .workspace
        .workspace_root()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "commedit".to_string());
    let window = ApplicationWindow::builder()
        .application(app)
        .title(format!("Commit editor - {folder}"))
        .default_width(1400)
        .default_height(900)
        .child(&root)
        .build();
    window.set_titlebar(Some(&header));

    // Syntax-highlighting resources (loaded once). A light theme to sit on the
    // light diff line backgrounds, like GitHub/delta.
    let syntax_set = Rc::new(SyntaxSet::load_defaults_newlines());
    let theme: Rc<Theme> = {
        let themes = ThemeSet::load_defaults().themes;
        let chosen = themes
            .get("InspiredGitHub")
            .or_else(|| themes.values().next())
            .cloned()
            .expect("at least one default theme");
        Rc::new(chosen)
    };

    // Re-render the diff highlighting for whatever is currently in the buffer.
    let highlight: Rc<dyn Fn()> = {
        let file_buffer = file_buffer.clone();
        let current_file = current_file.clone();
        let syntax_set = syntax_set.clone();
        let theme = theme.clone();
        Rc::new(move || {
            let path = current_file.borrow().clone();
            highlight_diff(&file_buffer, path.as_deref(), &syntax_set, &theme);
        })
    };

    // Render the current file's diff into the buffer with its per-hunk context
    // expansion, appending a click-to-expand cue to each expandable `@@` header.
    // Self-referential (via `render_cell`) so a click can request a re-render
    // after widening a hunk.
    let render_cell: Rc<RefCell<Option<Renderer>>> = Rc::new(RefCell::new(None));
    // Hunks of the diff currently in the buffer, so an expand click can scroll
    // its (now re-rendered) hunk back into view instead of jumping to the top.
    let rendered_hunks: Rc<RefCell<Vec<HunkInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let render_diff_view: Renderer = {
        let changes = changes.clone();
        let current_file = current_file.clone();
        let file_buffer = file_buffer.clone();
        let editing = editing.clone();
        let expansions = expansions.clone();
        let rendered_hunks = rendered_hunks.clone();
        let highlight = highlight.clone();
        Rc::new(move || {
            let Some(path) = current_file.borrow().clone() else {
                return;
            };
            let change = changes.borrow().iter().find(|c| c.path == path).cloned();
            let Some(change) = change else { return };
            let Some(new) = change.new_text.as_deref() else {
                return;
            };
            let old = change.old_text.as_deref().unwrap_or("");
            let rendered = {
                let mut map = expansions.borrow_mut();
                let exp = map.entry(path.clone()).or_default();
                render_diff(old, new, &path, exp)
            };
            editing.set(true);
            file_buffer.set_text(&rendered.text);
            // Append a click-to-expand cue to each expandable @@ header. The
            // click is handled by a GestureClick on the view (see below); we must
            // not embed a real widget in the buffer, because removing it during
            // the next `set_text` crashes GTK.
            for hunk in &rendered.hunks {
                let cue = match (hunk.can_expand_up, hunk.can_expand_down) {
                    (true, true) => "    ↕ expand context",
                    (true, false) => "    ↑ expand context",
                    (false, true) => "    ↓ expand context",
                    (false, false) => continue,
                };
                if let Some(mut iter) = file_buffer.iter_at_line(hunk.header_line as i32) {
                    iter.forward_to_line_end();
                    file_buffer.insert(&mut iter, cue);
                }
            }
            *rendered_hunks.borrow_mut() = rendered.hunks.clone();
            // Highlight in this same main-loop turn, before GTK paints, so the
            // diff appears once fully colored instead of flashing plain first and
            // then re-highlighting via the debounced `changed` handler (which is
            // suppressed below while `editing` is set).
            highlight();
            editing.set(false);
        })
    };
    *render_cell.borrow_mut() = Some(render_diff_view.clone());

    // Clicking a @@ header line (anywhere on it, including the "expand context"
    // cue) widens that hunk's context. The re-render is deferred to an idle so it
    // runs outside the gesture's event handling.
    let expand_click = gtk::GestureClick::new();
    expand_click.set_button(gdk::BUTTON_PRIMARY);
    expand_click.set_propagation_phase(PropagationPhase::Capture);
    expand_click.connect_pressed({
        let file_view = file_view.clone();
        let file_buffer = file_buffer.clone();
        let rendered_hunks = rendered_hunks.clone();
        let expansions = expansions.clone();
        let render_cell = render_cell.clone();
        let current_file = current_file.clone();
        move |gesture, _n_press, x, y| {
            let (bx, by) = file_view.window_to_buffer_coords(
                gtk::TextWindowType::Widget,
                x as i32,
                y as i32,
            );
            let Some(iter) = file_view.iter_at_location(bx, by) else {
                return;
            };
            let line = iter.line() as usize;
            let hit = rendered_hunks
                .borrow()
                .iter()
                .find(|h| h.header_line == line && (h.can_expand_up || h.can_expand_down))
                .map(|h| (h.first_group, h.last_group));
            let Some((first, last)) = hit else { return };
            let Some(path) = current_file.borrow().clone() else {
                return;
            };
            // We own this click: don't let the view also place the caret.
            gesture.set_state(gtk::EventSequenceState::Claimed);

            // Record where the clicked header sits in the viewport now, so that
            // after re-rendering (which resets the scroll and shifts lines down
            // as context appears above) we can pin that same header back to the
            // same spot — expansion then grows around it instead of jumping.
            let frac = vertical_fraction_of_line(&file_view, &file_buffer, line);
            // The view is monospace and never wraps, so every line is the same
            // height. Measure it now (layout is valid) to compute the post-render
            // scroll offset arithmetically — see the idle below.
            let line_height = file_view
                .iter_location(&file_buffer.start_iter())
                .height() as f64;

            let expansions = expansions.clone();
            let render_cell = render_cell.clone();
            let rendered_hunks = rendered_hunks.clone();
            let file_buffer = file_buffer.clone();
            let file_view = file_view.clone();
            glib::idle_add_local_once(move || {
                expansions
                    .borrow_mut()
                    .entry(path)
                    .or_default()
                    .expand(first, last);
                if let Some(render) = render_cell.borrow().clone() {
                    render();
                }
                // Pin the (possibly moved or merged) hunk header to its prior
                // viewport position. set_text reset the scroll to the top; the
                // deferred scrollers (scroll_to_mark / scroll_to_iter) only run on
                // a later frame, so the top is painted first and *then* corrected —
                // the visible jump-to-top flash. Instead, set the scroll offset
                // synchronously here, before GTK paints, so the next paint already
                // shows the final position. Line height is uniform (monospace, no
                // wrap), so the header's offset is just `line * line_height`, and
                // the document height is `lines * line_height + margins`. We set
                // the adjustment's upper too, so set_value isn't clamped against
                // the stale (pre-render) range; GTK's own validation later sets the
                // same values, leaving the position unchanged.
                let header = rendered_hunks
                    .borrow()
                    .iter()
                    .find(|h| h.first_group <= first && last <= h.last_group)
                    .map(|h| h.header_line);
                if let (Some(line), Some(vadj)) = (header, file_view.vadjustment()) {
                    let page = vadj.page_size();
                    if line_height > 0.0 && page > 0.0 {
                        let top = file_view.top_margin() as f64;
                        let bottom = file_view.bottom_margin() as f64;
                        let height = file_buffer.line_count() as f64 * line_height + top + bottom;
                        let upper = height.max(page);
                        let target = (line as f64 * line_height + top - frac * page)
                            .clamp(0.0, (upper - page).max(0.0));
                        vadj.set_upper(upper);
                        vadj.set_value(target);
                        // Keep the cursor on the now-visible header so GTK's
                        // validation doesn't scroll the (offset-0) caret back into
                        // view and undo the offset we just set.
                        if let Some(iter) = file_buffer.iter_at_line(line as i32) {
                            file_buffer.place_cursor(&iter);
                        }
                    }
                }
            });
        }
    });
    file_view.add_controller(expand_click);

    // Show the file at `idx` of the current changes in the editor.
    let show_file: Rc<dyn Fn(usize)> = {
        let changes = changes.clone();
        let current_file = current_file.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let editing = editing.clone();
        let render_diff_view = render_diff_view.clone();
        Rc::new(move |idx: usize| {
            let change = changes.borrow().get(idx).cloned();
            let Some(change) = change else { return };
            *current_file.borrow_mut() = Some(change.path.clone());
            match (&change.new_text, change.is_binary) {
                (Some(_), _) => {
                    file_view.set_editable(true);
                    render_diff_view();
                }
                (None, binary) => {
                    editing.set(true);
                    file_buffer.set_text(if binary {
                        "<binary file — not editable>"
                    } else {
                        "<file removed by this commit>"
                    });
                    editing.set(false);
                    file_view.set_editable(false);
                }
            }
        })
    };

    // Re-highlight after edits, debounced/coalesced so typing stays responsive.
    // (Applying tags does not emit `changed`, so this can't loop.)
    let highlight_gen = Rc::new(RefCell::new(0u64));
    file_buffer.connect_changed({
        let highlight = highlight.clone();
        let highlight_gen = highlight_gen.clone();
        let editing = editing.clone();
        move |_| {
            // A full programmatic render highlights itself synchronously; don't
            // also schedule a redundant (and flash-inducing) debounced pass.
            if editing.get() {
                return;
            }
            let mine = {
                let mut g = highlight_gen.borrow_mut();
                *g = g.wrapping_add(1);
                *g
            };
            let highlight = highlight.clone();
            let highlight_gen = highlight_gen.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
                if *highlight_gen.borrow() == mine {
                    highlight();
                }
            });
        }
    });

    // Show a message in the status line for a few seconds, then clear it (a
    // generation counter coalesces rapid messages so only the latest clears).
    let status_gen = Rc::new(RefCell::new(0u64));
    let show_status: Rc<dyn Fn(&str)> = {
        let status_label = status_label.clone();
        let status_gen = status_gen.clone();
        Rc::new(move |msg: &str| {
            status_label.set_text(msg);
            status_label.set_visible(true);
            let mine = {
                let mut g = status_gen.borrow_mut();
                *g = g.wrapping_add(1);
                *g
            };
            let status_label = status_label.clone();
            let status_gen = status_gen.clone();
            glib::timeout_add_local_once(std::time::Duration::from_secs(4), move || {
                if *status_gen.borrow() == mine {
                    status_label.set_text("");
                    status_label.set_visible(false);
                }
            });
        })
    };

    const READ_ONLY_HINT: &str = "Edit blocked — only added (+) lines are freely editable.";

    // Firewall: every interactive mutation of the diff buffer goes through the
    // structured-edit planner so it can never produce a patch that fails to
    // apply. Programmatic loads/edits set the `editing` guard and pass straight
    // through. `insert-text` covers typing and paste; `delete-range` covers cut,
    // drag and selection deletes.
    file_buffer.connect_insert_text({
        let editing = editing.clone();
        let show_status = show_status.clone();
        move |buffer, iter, text| {
            if editing.get() {
                return;
            }
            let caret = Selection::caret(Cursor {
                line: iter.line() as usize,
                col: iter.line_offset() as usize,
            });
            match plan_edit(&buffer_text(buffer), caret, EditGesture::Insert(text.to_string())) {
                EditPlan::Allow => {}
                EditPlan::Block => {
                    buffer.stop_signal_emission_by_name("insert-text");
                    show_status(READ_ONLY_HINT);
                }
                EditPlan::Edit(edit) => {
                    buffer.stop_signal_emission_by_name("insert-text");
                    apply_patch_edit(buffer, &editing, &edit);
                }
            }
        }
    });
    file_buffer.connect_delete_range({
        let editing = editing.clone();
        let show_status = show_status.clone();
        move |buffer, start, end| {
            if editing.get() {
                return;
            }
            let s = Cursor {
                line: start.line() as usize,
                col: start.line_offset() as usize,
            };
            let e = Cursor {
                line: end.line() as usize,
                col: end.line_offset() as usize,
            };
            if !deletion_is_safe(&buffer_text(buffer), s, e) {
                buffer.stop_signal_emission_by_name("delete-range");
                show_status(READ_ONLY_HINT);
            }
        }
    });

    // Enter / Backspace / Delete carry structural intent that a plain
    // insert/delete can't express (split a line, un-remove a line, join added
    // lines), so handle them as gestures via the planner. Printable keys are left
    // alone so input methods keep working — they flow through `insert-text`. The
    // controller runs in the capture phase to pre-empt the view's own handling.
    let key_controller = EventControllerKey::new();
    key_controller.set_propagation_phase(PropagationPhase::Capture);
    key_controller.connect_key_pressed({
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let editing = editing.clone();
        let show_status = show_status.clone();
        move |_, keyval, _, _| {
            if !file_view.is_editable() {
                return glib::Propagation::Proceed;
            }
            let gesture = match keyval {
                gdk::Key::Return | gdk::Key::KP_Enter => EditGesture::Newline,
                gdk::Key::BackSpace => EditGesture::Backspace,
                gdk::Key::Delete | gdk::Key::KP_Delete => EditGesture::Delete,
                _ => return glib::Propagation::Proceed,
            };
            match plan_edit(&buffer_text(&file_buffer), buffer_selection(&file_buffer), gesture) {
                EditPlan::Allow => glib::Propagation::Proceed,
                EditPlan::Block => {
                    show_status(READ_ONLY_HINT);
                    glib::Propagation::Stop
                }
                EditPlan::Edit(edit) => {
                    apply_patch_edit(&file_buffer, &editing, &edit);
                    glib::Propagation::Stop
                }
            }
        }
    });
    file_view.add_controller(key_controller);

    file_dropdown.connect_selected_notify({
        let show_file = show_file.clone();
        move |dd| {
            let idx = dd.selected();
            if idx != gtk::INVALID_LIST_POSITION {
                show_file(idx as usize);
            }
        }
    });

    // Scrolling over the (closed) drop-down steps through the files of the diff,
    // so flipping between files doesn't require opening the popover each time.
    let scroll_controller = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
    scroll_controller.connect_scroll({
        let file_dropdown = file_dropdown.clone();
        move |_, _, dy| {
            let n = file_dropdown.model().map_or(0, |m| m.n_items());
            if n == 0 {
                return glib::Propagation::Proceed;
            }
            let cur = file_dropdown.selected();
            if cur == gtk::INVALID_LIST_POSITION {
                return glib::Propagation::Proceed;
            }
            // Scroll down (dy > 0) advances to the next file; up goes back.
            // Clamp at the ends rather than wrapping.
            let next = if dy > 0.0 {
                (cur + 1).min(n - 1)
            } else if dy < 0.0 {
                cur.saturating_sub(1)
            } else {
                cur
            };
            if next != cur {
                file_dropdown.set_selected(next);
            }
            glib::Propagation::Stop
        }
    });
    file_dropdown.add_controller(scroll_controller);

    // Load the changed-files list for the selected commit into the dropdown.
    let load_changes: Rc<dyn Fn(&CommitInfo)> = {
        let repo = repo.clone();
        let changes = changes.clone();
        let current_file = current_file.clone();
        let file_dropdown = file_dropdown.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let editing = editing.clone();
        let expansions = expansions.clone();
        Rc::new(move |commit: &CommitInfo| {
            let loaded = commit_changes(&repo.borrow().repo, &commit.id).unwrap_or_default();
            *changes.borrow_mut() = loaded;
            *current_file.borrow_mut() = None;
            expansions.borrow_mut().clear();
            let labels: Vec<String> = changes.borrow().iter().map(change_label).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            file_dropdown.set_model(Some(&StringList::new(&refs)));
            if labels.is_empty() {
                editing.set(true);
                file_buffer.set_text("");
                editing.set(false);
                file_view.set_editable(false);
            } else {
                // Triggers selected-notify -> show_file(0).
                file_dropdown.set_selected(0);
            }
        })
    };

    // Selecting a commit loads its message and changed files.
    list.connect_row_selected({
        let commits = commits.clone();
        let message_buffer = message_buffer.clone();
        let selected_change = selected_change.clone();
        let load_changes = load_changes.clone();
        let identity_fields = identity_fields.clone();
        let original_identity = original_identity.clone();
        move |_list, row| {
            let Some(row) = row else { return };
            let idx = row.index();
            if idx < 0 {
                return;
            }
            let info = commits.borrow().get(idx as usize).cloned();
            let Some(info) = info else { return };
            *selected_change.borrow_mut() = Some(info.change_id_hex());
            message_buffer.set_text(&info.description);
            set_identity_fields(&identity_fields, &info);
            *original_identity.borrow_mut() = Some(read_identity(&identity_fields));
            load_changes(&info);
        }
    });

    // Reload history from the engine, preserving the selected commit by its
    // (rewrite-stable) change id.
    let refresh: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let commits = commits.clone();
        let list = list.clone();
        let selected_change = selected_change.clone();
        let identities = identities.clone();
        Rc::new(move || {
            let loaded = history(&repo.borrow().repo).unwrap_or_default();
            *commits.borrow_mut() = loaded;
            {
                let cs = commits.borrow();
                populate_list(&list, &cs);
                // Harvest the distinct identities seen across history, offered by
                // the in-field ▼ picker.
                let mut ids: Vec<(String, String)> = Vec::new();
                for c in cs.iter() {
                    for pair in [
                        (c.author_name.clone(), c.author_email.clone()),
                        (c.committer_name.clone(), c.committer_email.clone()),
                    ] {
                        if !ids.contains(&pair) {
                            ids.push(pair);
                        }
                    }
                }
                ids.sort();
                *identities.borrow_mut() = ids;
            }
            let target_row = selected_change.borrow().clone().and_then(|change| {
                commits
                    .borrow()
                    .iter()
                    .position(|c| c.change_id_hex() == change)
            });
            if let Some(idx) = target_row {
                if let Some(row) = list.row_at_index(idx as i32) {
                    list.select_row(Some(&row));
                }
            }
        })
    };

    // Drag-and-drop to reorder commits. While dragging, a placeholder row opens a
    // gap at the hover position (the surrounding commits slide to make room) and
    // the dragged row is dimmed; dropping rebases the commit into that slot via
    // the engine and reloads. The reorder is applied immediately — there is no
    // separate Save step for it.
    let placeholder = ListBoxRow::new();
    placeholder.set_selectable(false);
    placeholder.set_activatable(false);
    placeholder.set_height_request(28);
    placeholder.add_css_class("drop-placeholder");
    // The row currently being dragged, so it can be un-dimmed when the drag ends.
    let drag_row: Rc<RefCell<Option<ListBoxRow>>> = Rc::new(RefCell::new(None));
    // The insertion gap (newest-first index, 0..=len) the placeholder marks.
    let drop_gap: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));

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
        let drop_gap = drop_gap.clone();
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

    let drag_source = DragSource::new();
    drag_source.set_actions(gdk::DragAction::MOVE);
    drag_source.connect_prepare({
        let list = list.clone();
        let drag_row = drag_row.clone();
        move |source, _x, y| {
            let row = list.row_at_y(y as i32)?;
            // Show the dragged row under the cursor for feedback.
            let paintable = gtk::WidgetPaintable::new(Some(&row));
            source.set_icon(Some(&paintable), 0, 0);
            *drag_row.borrow_mut() = Some(row.clone());
            Some(gdk::ContentProvider::for_value(&row.index().to_value()))
        }
    });
    drag_source.connect_drag_begin({
        let drag_row = drag_row.clone();
        move |_source, _drag| {
            if let Some(row) = drag_row.borrow().as_ref() {
                row.add_css_class("commit-dragging");
            }
        }
    });
    drag_source.connect_drag_end({
        let drag_row = drag_row.clone();
        let clear_gap = clear_gap.clone();
        move |_source, _drag, _delete| {
            if let Some(row) = drag_row.borrow_mut().take() {
                row.remove_css_class("commit-dragging");
            }
            clear_gap();
        }
    });
    list.add_controller(drag_source);

    let drop_target = DropTarget::new(i32::static_type(), gdk::DragAction::MOVE);
    drop_target.connect_enter({
        let show_gap = show_gap.clone();
        move |_target, _x, y| {
            show_gap(y);
            gdk::DragAction::MOVE
        }
    });
    drop_target.connect_motion({
        let show_gap = show_gap.clone();
        move |_target, _x, y| {
            show_gap(y);
            gdk::DragAction::MOVE
        }
    });
    drop_target.connect_leave({
        let clear_gap = clear_gap.clone();
        move |_target| clear_gap()
    });
    drop_target.connect_drop({
        let commits = commits.clone();
        let repo = repo.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let gap_at = gap_at.clone();
        let clear_gap = clear_gap.clone();
        let drop_gap = drop_gap.clone();
        move |_target, value, _x, y| {
            let Ok(from) = value.get::<i32>() else {
                return false;
            };
            // Prefer the gap the placeholder marked; fall back to the drop point.
            let to = match drop_gap.get() {
                Some(to) => to,
                None => gap_at(y),
            };
            clear_gap();
            let Some(mv) = plan_reorder(&commits.borrow(), from as usize, to) else {
                return false;
            };
            if let Err(err) = repo.borrow_mut().reorder_commit(
                &mv.target,
                mv.new_parents,
                mv.new_children,
                &mv.new_tip,
            ) {
                show_status(&format!("Reorder failed: {err}"));
                return false;
            }
            refresh();
            true
        }
    });
    list.add_controller(drop_target);

    // Save: rewrite the message and/or the selected file's content, then reload.
    // Reloading re-selects the commit, which cascades through `row-selected` ->
    // `load_changes` and resets the file dropdown to index 0 with the cursor at
    // the start. We capture the selected file and cursor offset beforehand and
    // restore them afterwards so a save is invisible to the user's place in the
    // diff.
    let save: Rc<dyn Fn()> = {
        let repo = repo.clone();
        let commits = commits.clone();
        let changes = changes.clone();
        let current_file = current_file.clone();
        let message_buffer = message_buffer.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        let file_dropdown = file_dropdown.clone();
        let selected_change = selected_change.clone();
        let refresh = refresh.clone();
        let show_status = show_status.clone();
        let identity_fields = identity_fields.clone();
        let original_identity = original_identity.clone();
        Rc::new(move || {
            let Some(change_id) = selected_change.borrow().clone() else {
                return;
            };
            let target = commits
                .borrow()
                .iter()
                .find(|c| c.change_id_hex() == change_id)
                .map(|c| (c.id.clone(), c.description.clone()));
            let Some((mut commit_id, original_message)) = target else {
                return;
            };

            // Remember where the user is so we can restore it after the reload.
            let saved_file = current_file.borrow().clone();
            let saved_cursor = file_buffer.cursor_position();
            let file_had_focus = file_view.has_focus();

            // Message edit (if changed).
            let new_message = buffer_text(&message_buffer);
            if new_message != original_message {
                if let Err(err) = repo.borrow_mut().rewrite_message(&commit_id, &new_message) {
                    show_status(&format!("Message save failed: {err}"));
                    return;
                }
                // The commit id changed; re-resolve by change id.
                if let Some(info) = resolve_commit(&repo, &change_id) {
                    commit_id = info.id;
                }
            }

            // File content edit (if an editable file is selected and changed).
            if let Some(path) = saved_file.clone() {
                let change = changes.borrow().iter().find(|c| c.path == path).cloned();
                if let Some(change) = change {
                    if let Some(original) = change.new_text {
                        let old = change.old_text.as_deref().unwrap_or("");
                        match apply_patch(old, &buffer_text(&file_buffer)) {
                            Ok(mut content) => {
                                // Preserve the original file's trailing-newline style.
                                if !original.is_empty()
                                    && !original.ends_with('\n')
                                    && content.ends_with('\n')
                                {
                                    content.pop();
                                }
                                if content != original {
                                    if let Err(err) =
                                        repo.borrow_mut().rewrite_file(&commit_id, &path, &content)
                                    {
                                        show_status(&format!("File save failed: {err}"));
                                        return;
                                    }
                                }
                            }
                            Err(err) => {
                                // The firewall should make this unreachable; if
                                // it ever fires, surface it instead of silently
                                // dropping the save.
                                show_status(&format!("Cannot apply edited patch: {err}"));
                                return;
                            }
                        }
                    }
                }
            }

            // Identity / date edit (if changed). Run last so the explicitly set
            // committer survives jj stamping it to "now" on the edits above.
            let new_identity = read_identity(&identity_fields);
            if original_identity.borrow().as_ref() != Some(&new_identity) {
                // Prior rewrites may have changed the commit id; re-resolve it.
                if let Some(info) = resolve_commit(&repo, &change_id) {
                    commit_id = info.id;
                }
                if let Err(err) = repo.borrow_mut().rewrite_identity(&commit_id, &new_identity) {
                    show_status(&format!("Identity save failed: {err}"));
                    return;
                }
            }

            refresh();

            // Restore the selected file and cursor (refresh reset both).
            if let Some(path) = saved_file {
                if let Some(idx) = changes.borrow().iter().position(|c| c.path == path) {
                    file_dropdown.set_selected(idx as u32);
                }
            }
            let offset = saved_cursor.min(file_buffer.char_count());
            let cursor = file_buffer.iter_at_offset(offset);
            file_buffer.place_cursor(&cursor);
            file_view.scroll_to_mark(&file_buffer.get_insert(), 0.0, false, 0.0, 0.0);
            if file_had_focus {
                file_view.grab_focus();
            }
        })
    };

    save_button.connect_clicked({
        let save = save.clone();
        move |_| save()
    });

    // Ctrl+S triggers the same save.
    let save_shortcut = {
        let save = save.clone();
        let action = CallbackAction::new(move |_, _| {
            save();
            glib::Propagation::Stop
        });
        let trigger = ShortcutTrigger::parse_string("<Control>s")
            .expect("valid shortcut trigger");
        Shortcut::new(Some(trigger), Some(action))
    };
    let shortcuts = ShortcutController::new();
    shortcuts.add_shortcut(save_shortcut);
    window.add_controller(shortcuts);

    // Initial population and selection.
    refresh();
    if selected_change.borrow().is_none() {
        if let Some(row) = list.row_at_index(0) {
            list.select_row(Some(&row));
        }
    }

    window.present();
}

/// Re-resolve a commit's current id from its rewrite-stable change id.
fn resolve_commit(
    repo: &Rc<RefCell<Repo>>,
    change_id: &str,
) -> Option<commedit_engine::history::CommitInfo> {
    history(&repo.borrow().repo)
        .ok()?
        .into_iter()
        .find(|c| c.change_id_hex() == change_id)
}

/// Create the static, named tags used for diff line backgrounds and intra-line
/// emphasis (idempotent). Per-syntax foreground tags are created lazily in
/// [`fg_tag`]. Colors follow GitHub's light diff palette.
fn install_diff_tags(buffer: &sourceview5::Buffer) {
    let table = buffer.tag_table();
    let add = |name: &str, build: &dyn Fn(&TextTag)| {
        if table.lookup(name).is_none() {
            let tag = TextTag::new(Some(name));
            build(&tag);
            table.add(&tag);
        }
    };
    add("add-line", &|t| t.set_paragraph_background(Some("#e6ffec")));
    add("del-line", &|t| t.set_paragraph_background(Some("#ffebe9")));
    add("hunk", &|t| {
        t.set_paragraph_background(Some("#ddf4ff"));
        t.set_foreground(Some("#0550ae"));
    });
    add("meta", &|t| t.set_foreground(Some("#6e7781")));
    add("add-word", &|t| t.set_background(Some("#abf2bc")));
    add("del-word", &|t| t.set_background(Some("#ffc0bd")));
    // The clickable "expand context" cue appended to expandable @@ headers.
    add("expand-hint", &|t| {
        t.set_foreground(Some("#0969da"));
        t.set_weight(700);
    });
}

/// Look up (or lazily create and cache, via the buffer's tag table) a foreground
/// color tag for a `#rrggbb` value produced by syntect.
fn fg_tag(buffer: &sourceview5::Buffer, hex: &str) -> TextTag {
    let name = format!("fg{hex}");
    if let Some(tag) = buffer.tag_table().lookup(&name) {
        return tag;
    }
    let tag = TextTag::new(Some(&name));
    tag.set_foreground(Some(hex));
    buffer.tag_table().add(&tag);
    tag
}

/// Re-apply all diff highlighting tags to `buffer` for the unified diff it
/// currently holds: line backgrounds by kind, syntect language coloring of the
/// code portion (keeping separate parser state for the removed/added sides so
/// multi-line constructs stay correct), and intra-line change emphasis.
fn highlight_diff(buffer: &sourceview5::Buffer, path: Option<&str>, ps: &SyntaxSet, theme: &Theme) {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    let text = buffer.text(&start, &end, false).to_string();
    buffer.remove_all_tags(&start, &end);

    let raw_lines: Vec<&str> = text.split('\n').collect();
    let parsed = parse_diff_lines(&text);

    let syntax = path
        .and_then(|p| std::path::Path::new(p).extension())
        .and_then(|e| e.to_str())
        .and_then(|ext| ps.find_syntax_by_extension(ext))
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let mut old_hl = HighlightLines::new(syntax, theme);
    let mut new_hl = HighlightLines::new(syntax, theme);

    for (li, line) in parsed.iter().enumerate() {
        let raw = raw_lines[li];
        if let Some(name) = line_bg_tag(line.kind) {
            apply_line_tag(buffer, li as i32, name);
        }
        match line.kind {
            // Hunk boundary: reset both parser states (the shown regions are
            // discontiguous, so state must not leak across the gap).
            DiffLineKind::Hunk => {
                old_hl = HighlightLines::new(syntax, theme);
                new_hl = HighlightLines::new(syntax, theme);
                // Accent the trailing "expand context" cue (everything past the
                // closing `@@`) so it reads as a clickable control.
                if let Some(pos) = raw.rfind("@@") {
                    let cue_start = pos + 2;
                    if cue_start < raw.len() {
                        if let Some(tag) = buffer.tag_table().lookup("expand-hint") {
                            let cs = raw[..cue_start].chars().count() as i32;
                            let ce = raw.chars().count() as i32;
                            apply_cols(buffer, li as i32, cs, ce, &tag);
                        }
                    }
                }
                continue;
            }
            DiffLineKind::Header | DiffLineKind::Meta => continue,
            _ => {}
        }

        let prefix = if raw.is_empty() { 0 } else { 1 };
        let code = &raw[prefix..];
        let owned = format!("{code}\n");
        let spans = match line.kind {
            DiffLineKind::Removed => old_hl.highlight_line(&owned, ps),
            DiffLineKind::Added => new_hl.highlight_line(&owned, ps),
            // Context advances both sides; color from the (identical) new side.
            DiffLineKind::Context => {
                let _ = old_hl.highlight_line(&owned, ps);
                new_hl.highlight_line(&owned, ps)
            }
            _ => continue,
        };
        if let Ok(spans) = spans {
            let mut byte = 0usize;
            for (style, piece) in spans {
                if byte >= code.len() {
                    break;
                }
                let plen = piece.len().min(code.len() - byte); // clip the trailing '\n'
                if plen > 0 {
                    let cs = prefix + code[..byte].chars().count();
                    let ce = prefix + code[..byte + plen].chars().count();
                    let fg = style.foreground;
                    let hex = format!("#{:02x}{:02x}{:02x}", fg.r, fg.g, fg.b);
                    apply_cols(buffer, li as i32, cs as i32, ce as i32, &fg_tag(buffer, &hex));
                }
                byte += plen;
            }
        }

        if !line.intra.is_empty() {
            let word_tag = if line.kind == DiffLineKind::Added {
                "add-word"
            } else {
                "del-word"
            };
            if let Some(tag) = buffer.tag_table().lookup(word_tag) {
                for &(s, e) in &line.intra {
                    let cs = prefix + code[..s].chars().count();
                    let ce = prefix + code[..e].chars().count();
                    apply_cols(buffer, li as i32, cs as i32, ce as i32, &tag);
                }
            }
        }
    }
}

/// The line-background tag name for a diff line kind (`None` = context, no bg).
fn line_bg_tag(kind: DiffLineKind) -> Option<&'static str> {
    match kind {
        DiffLineKind::Added => Some("add-line"),
        DiffLineKind::Removed => Some("del-line"),
        DiffLineKind::Hunk => Some("hunk"),
        DiffLineKind::Header | DiffLineKind::Meta => Some("meta"),
        DiffLineKind::Context => None,
    }
}

/// Apply a named tag across the whole of buffer line `li` (including its newline,
/// so paragraph backgrounds fill the row).
fn apply_line_tag(buffer: &sourceview5::Buffer, li: i32, name: &str) {
    let Some(tag) = buffer.tag_table().lookup(name) else {
        return;
    };
    let Some(s) = buffer.iter_at_line(li) else {
        return;
    };
    let e = buffer.iter_at_line(li + 1).unwrap_or_else(|| buffer.end_iter());
    buffer.apply_tag(&tag, &s, &e);
}

/// Apply `tag` over the character-column range `[cs, ce)` of buffer line `li`.
fn apply_cols(buffer: &sourceview5::Buffer, li: i32, cs: i32, ce: i32, tag: &TextTag) {
    if let (Some(s), Some(e)) = (
        buffer.iter_at_line_offset(li, cs),
        buffer.iter_at_line_offset(li, ce),
    ) {
        buffer.apply_tag(tag, &s, &e);
    }
}

/// A horizontally-expanding text entry with placeholder text, for an identity
/// name/email/date field.
fn identity_entry(placeholder: &str) -> Entry {
    let entry = Entry::new();
    entry.set_placeholder_text(Some(placeholder));
    entry.set_hexpand(true);
    entry
}

/// A date entry with a calendar button to its right, as a single grid cell.
fn date_field(date: &Entry) -> GtkBox {
    let date_box = GtkBox::new(Orientation::Horizontal, 4);
    date_box.append(date);
    date_box.append(&calendar_button(date));
    date_box
}

/// A 📅 menu button whose popover holds a [`Calendar`] that edits the date
/// portion of `entry`, preserving its time-of-day and timezone suffix.
fn calendar_button(entry: &Entry) -> MenuButton {
    let calendar = Calendar::new();
    let popover = Popover::new();
    popover.set_child(Some(&calendar));
    let button = MenuButton::new();
    button.set_icon_name("x-office-calendar-symbolic");
    button.set_popover(Some(&popover));
    button.set_tooltip_text(Some("Pick the date"));

    // Open the calendar on the date currently in the field.
    calendar.connect_map({
        let entry = entry.clone();
        move |cal| {
            if let Some((y, m, d)) = entry_date_parts(&entry.text()) {
                if let Ok(dt) = glib::DateTime::from_local(y, m, d, 0, 0, 0.0) {
                    cal.select_day(&dt);
                }
            }
        }
    });
    calendar.connect_day_selected({
        let entry = entry.clone();
        move |cal| {
            let date = cal.date();
            set_entry_date(&entry, date.year(), date.month(), date.day_of_month());
        }
    });
    button
}

/// Parse the leading `YYYY-MM-DD` of a timestamp field into `(year, month, day)`.
fn entry_date_parts(text: &str) -> Option<(i32, i32, i32)> {
    let date = text.split_whitespace().next()?;
    let mut parts = date.split('-');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    Some((year, month, day))
}

/// Replace the date portion of `entry`, keeping its `HH:MM:SS ±HHMM` suffix (or
/// a sensible default when the field has none yet).
fn set_entry_date(entry: &Entry, year: i32, month: i32, day: i32) {
    let text = entry.text();
    let rest = text
        .trim()
        .split_once(' ')
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_else(|| "00:00:00 +0000".to_string());
    entry.set_text(&format!("{year:04}-{month:02}-{day:02} {rest}"));
}

/// Give `entry` a built-in ▼ icon that opens a popover of `identities`; picking
/// one sets the field to its `Name <email>` form.
fn attach_identity_picker(entry: &Entry, identities: &Rc<RefCell<Vec<(String, String)>>>) {
    entry.set_icon_from_icon_name(gtk::EntryIconPosition::Secondary, Some("pan-down-symbolic"));
    entry.set_icon_activatable(gtk::EntryIconPosition::Secondary, true);
    entry.set_icon_tooltip_text(
        gtk::EntryIconPosition::Secondary,
        Some("Use an identity from another commit"),
    );

    let list = ListBox::new();
    let scroll = ScrolledWindow::builder()
        .propagate_natural_height(true)
        .propagate_natural_width(true)
        .min_content_width(280)
        .max_content_height(280)
        .hscrollbar_policy(PolicyType::Never)
        .child(&list)
        .build();
    let popover = Popover::new();
    popover.set_child(Some(&scroll));
    popover.set_parent(entry);

    entry.connect_icon_press({
        let identities = identities.clone();
        let list = list.clone();
        let popover = popover.clone();
        move |_, pos| {
            if pos != gtk::EntryIconPosition::Secondary {
                return;
            }
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            for (name, email) in identities.borrow().iter() {
                let label = Label::builder()
                    .label(join_name_email(name, email))
                    .xalign(0.0)
                    .margin_start(6)
                    .margin_end(6)
                    .build();
                list.append(&label);
            }
            popover.popup();
        }
    });
    list.connect_row_activated({
        let entry = entry.clone();
        let popover = popover.clone();
        move |_, row| {
            if let Some(label) = row.child().and_downcast::<Label>() {
                entry.set_text(&label.text());
            }
            popover.popdown();
        }
    });
}

/// Split a `Name <email>` field into its `(name, email)` parts; an unbracketed
/// value is taken as a bare name.
fn split_name_email(text: &str) -> (String, String) {
    let text = text.trim();
    if let Some(open) = text.rfind('<') {
        if let Some(close) = text[open..].find('>') {
            let name = text[..open].trim().to_string();
            let email = text[open + 1..open + close].trim().to_string();
            return (name, email);
        }
    }
    (text.to_string(), String::new())
}

/// Format `(name, email)` as a `Name <email>` field value.
fn join_name_email(name: &str, email: &str) -> String {
    if email.is_empty() {
        name.to_string()
    } else {
        format!("{name} <{email}>")
    }
}

/// Read the identity entry fields into an [`Identity`]. Field order is
/// `[author "Name <email>", author date, committer "Name <email>", committer date]`.
fn read_identity(fields: &[Entry; 4]) -> Identity {
    let (author_name, author_email) = split_name_email(&fields[0].text());
    let (committer_name, committer_email) = split_name_email(&fields[2].text());
    Identity {
        author_name,
        author_email,
        author_time: fields[1].text().to_string(),
        committer_name,
        committer_email,
        committer_time: fields[3].text().to_string(),
    }
}

/// Populate the identity entry fields from a commit (see [`read_identity`] for
/// the field order).
fn set_identity_fields(fields: &[Entry; 4], commit: &CommitInfo) {
    fields[0].set_text(&join_name_email(&commit.author_name, &commit.author_email));
    fields[1].set_text(&commit.author_time);
    fields[2].set_text(&join_name_email(&commit.committer_name, &commit.committer_email));
    fields[3].set_text(&commit.committer_time);
}

/// Replace the list's rows with one row per commit (short id + subject).
fn populate_list(list: &ListBox, commits: &[CommitInfo]) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    for commit in commits {
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
        let row = ListBoxRow::new();
        row.set_child(Some(&row_box));
        list.append(&row);
    }
}

fn present_error(app: &Application, message: &str) {
    let label = Label::builder()
        .label(message)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(16)
        .wrap(true)
        .build();
    let window = ApplicationWindow::builder()
        .application(app)
        .title("commedit — error")
        .default_width(600)
        .default_height(200)
        .child(&label)
        .build();
    window.present();
}
