//! commedit GTK4 UI (Milestone 2): browse history, edit a commit message, and
//! edit the content of files a commit changes. Saving transparently rewrites the
//! commit and rebases descendants via the engine.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use commedit_engine::diff::{
    apply_patch, commit_changes, parse_diff_lines, unified_diff, ChangeKind, DiffLineKind,
    FileChange,
};
use commedit_engine::history::{history, CommitInfo};
use commedit_engine::repo::Repo;
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    Application, ApplicationWindow, Box as GtkBox, Button, CallbackAction, DropDown, HeaderBar,
    Label, ListBox, ListBoxRow, Orientation, Paned, PolicyType, ScrolledWindow, Shortcut,
    ShortcutController, ShortcutTrigger, StringList, TextTag,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

const APP_ID: &str = "net.willi.commedit";

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
    let message_box = labelled_editor("Commit message", &message_view);

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
    let file_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&file_view)
        .build();
    let files_box = GtkBox::new(Orientation::Vertical, 0);
    let files_header = Label::builder()
        .label("Changed files")
        .xalign(0.0)
        .margin_start(8)
        .margin_top(8)
        .margin_bottom(4)
        .build();
    file_dropdown.set_margin_start(8);
    file_dropdown.set_margin_end(8);
    file_dropdown.set_margin_bottom(4);
    files_box.append(&files_header);
    files_box.append(&file_dropdown);
    files_box.append(&file_scroll);

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

    let save_button = Button::with_label("Save");
    save_button.add_css_class("suggested-action");
    let header = HeaderBar::new();
    header.pack_start(&save_button);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("commedit")
        .default_width(1400)
        .default_height(900)
        .child(&paned)
        .build();
    window.set_titlebar(Some(&header));

    // Show the file at `idx` of the current changes in the editor.
    let show_file: Rc<dyn Fn(usize)> = {
        let changes = changes.clone();
        let current_file = current_file.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        Rc::new(move |idx: usize| {
            let change = changes.borrow().get(idx).cloned();
            let Some(change) = change else { return };
            *current_file.borrow_mut() = Some(change.path.clone());
            match (&change.new_text, change.is_binary) {
                (Some(new), _) => {
                    let old = change.old_text.as_deref().unwrap_or("");
                    file_buffer.set_text(&unified_diff(old, new, &change.path));
                    file_view.set_editable(true);
                }
                (None, true) => {
                    file_buffer.set_text("<binary file — not editable>");
                    file_view.set_editable(false);
                }
                (None, false) => {
                    file_buffer.set_text("<file removed by this commit>");
                    file_view.set_editable(false);
                }
            }
        })
    };

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

    // Re-highlight after edits, debounced/coalesced so typing stays responsive.
    // (Applying tags does not emit `changed`, so this can't loop.)
    let highlight_gen = Rc::new(RefCell::new(0u64));
    file_buffer.connect_changed({
        let highlight = highlight.clone();
        let highlight_gen = highlight_gen.clone();
        move |_| {
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

    file_dropdown.connect_selected_notify({
        let show_file = show_file.clone();
        move |dd| {
            let idx = dd.selected();
            if idx != gtk::INVALID_LIST_POSITION {
                show_file(idx as usize);
            }
        }
    });

    // Load the changed-files list for the selected commit into the dropdown.
    let load_changes: Rc<dyn Fn(&CommitInfo)> = {
        let repo = repo.clone();
        let changes = changes.clone();
        let current_file = current_file.clone();
        let file_dropdown = file_dropdown.clone();
        let file_buffer = file_buffer.clone();
        let file_view = file_view.clone();
        Rc::new(move |commit: &CommitInfo| {
            let loaded = commit_changes(&repo.borrow().repo, &commit.id).unwrap_or_default();
            *changes.borrow_mut() = loaded;
            *current_file.borrow_mut() = None;
            let labels: Vec<String> = changes.borrow().iter().map(change_label).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            file_dropdown.set_model(Some(&StringList::new(&refs)));
            if labels.is_empty() {
                file_buffer.set_text("");
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
        Rc::new(move || {
            let loaded = history(&repo.borrow().repo).unwrap_or_default();
            *commits.borrow_mut() = loaded;
            {
                let cs = commits.borrow();
                populate_list(&list, &cs);
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
                    eprintln!("commedit: message save failed: {err:?}");
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
                                        eprintln!("commedit: file save failed: {err:?}");
                                        return;
                                    }
                                }
                            }
                            Err(err) => {
                                eprintln!("commedit: cannot apply edited patch: {err:#}");
                                return;
                            }
                        }
                    }
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

fn labelled_editor(title: &str, view: &sourceview5::View) -> GtkBox {
    let scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(view)
        .build();
    let container = GtkBox::new(Orientation::Vertical, 0);
    let header = Label::builder()
        .label(title)
        .xalign(0.0)
        .margin_start(8)
        .margin_top(8)
        .margin_bottom(4)
        .build();
    container.append(&header);
    container.append(&scroll);
    container
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
