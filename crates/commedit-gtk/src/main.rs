//! commedit GTK4 UI (Milestone 2): browse history, edit a commit message, and
//! edit the content of files a commit changes. Saving transparently rewrites the
//! commit and rebases descendants via the engine.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use commedit_engine::diff::{commit_changes, ChangeKind, FileChange};
use commedit_engine::history::{history, CommitInfo};
use commedit_engine::repo::Repo;
use gtk::prelude::*;
use gtk::{
    Application, ApplicationWindow, Box as GtkBox, Button, DropDown, HeaderBar, Label, ListBox,
    ListBoxRow, Orientation, Paned, PolicyType, ScrolledWindow, StringList,
};

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
        .width_request(320)
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
    let file_buffer = sourceview5::Buffer::new(None);
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
        .position(320)
        .build();

    let save_button = Button::with_label("Save");
    save_button.add_css_class("suggested-action");
    let header = HeaderBar::new();
    header.pack_start(&save_button);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("commedit")
        .default_width(1100)
        .default_height(750)
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
                (Some(text), _) => {
                    file_buffer.set_text(text);
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
    save_button.connect_clicked({
        let repo = repo.clone();
        let commits = commits.clone();
        let changes = changes.clone();
        let current_file = current_file.clone();
        let message_buffer = message_buffer.clone();
        let file_buffer = file_buffer.clone();
        let selected_change = selected_change.clone();
        let refresh = refresh.clone();
        move |_| {
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

            // File content edit (if a file is selected and changed).
            let path = current_file.borrow().clone();
            if let Some(path) = path {
                let original = changes
                    .borrow()
                    .iter()
                    .find(|c| c.path == path)
                    .and_then(|c| c.new_text.clone());
                let new_content = buffer_text(&file_buffer);
                // Only write editable (text) files that actually changed.
                if original.is_some() && Some(&new_content) != original.as_ref() {
                    if let Err(err) = repo.borrow_mut().rewrite_file(&commit_id, &path, &new_content)
                    {
                        eprintln!("commedit: file save failed: {err:?}");
                        return;
                    }
                }
            }

            refresh();
        }
    });

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
        let label = Label::builder()
            .label(format!("{short}  {subject}"))
            .xalign(0.0)
            .margin_start(8)
            .margin_end(8)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        let row = ListBoxRow::new();
        row.set_child(Some(&label));
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
