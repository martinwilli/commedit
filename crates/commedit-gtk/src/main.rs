//! commedit GTK4 UI (Milestone 1): browse history, edit a commit message, and
//! save — which transparently rewrites the commit and rebases descendants via
//! the engine.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use commedit_engine::history::{history, CommitInfo};
use commedit_engine::repo::Repo;
use gtk::prelude::*;
use gtk::{
    Application, ApplicationWindow, Box as GtkBox, Button, HeaderBar, Label, ListBox, ListBoxRow,
    Orientation, Paned, PolicyType, ScrolledWindow,
};

const APP_ID: &str = "net.willi.commedit";

fn main() {
    let repo_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, repo_path.clone()));
    // Ignore process args: we parsed our own path above and GTK would otherwise
    // try to interpret it as a file to open.
    app.run_with_args::<&str>(&[]);
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

    // --- History pane (left) ---
    let list = ListBox::new();
    let history_scroll = ScrolledWindow::builder()
        .hscrollbar_policy(PolicyType::Never)
        .vexpand(true)
        .width_request(320)
        .child(&list)
        .build();

    // --- Message pane (right) ---
    let buffer = sourceview5::Buffer::new(None);
    let view = sourceview5::View::with_buffer(&buffer);
    view.set_monospace(true);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_left_margin(8);
    view.set_top_margin(8);
    let message_scroll = ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&view)
        .build();

    let message_box = GtkBox::new(Orientation::Vertical, 0);
    let message_header = Label::builder()
        .label("Commit message")
        .xalign(0.0)
        .margin_start(8)
        .margin_top(8)
        .margin_bottom(4)
        .build();
    message_box.append(&message_header);
    message_box.append(&message_scroll);

    let paned = Paned::builder()
        .orientation(Orientation::Horizontal)
        .start_child(&history_scroll)
        .end_child(&message_box)
        .position(320)
        .build();

    // --- Header bar with Save ---
    let save_button = Button::with_label("Save");
    save_button.add_css_class("suggested-action");
    let header = HeaderBar::new();
    header.pack_start(&save_button);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("commedit")
        .default_width(1000)
        .default_height(700)
        .child(&paned)
        .build();
    window.set_titlebar(Some(&header));

    // Reload history from the engine and repopulate the list, preserving the
    // selected commit by its (rewrite-stable) change id.
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
            // Resolve the row to re-select WITHOUT holding any borrow across
            // select_row: it synchronously fires the row-selected handler, which
            // itself borrows these RefCells.
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

    // Selecting a commit loads its message into the editor.
    list.connect_row_selected({
        let commits = commits.clone();
        let buffer = buffer.clone();
        let selected_change = selected_change.clone();
        move |_list, row| {
            let Some(row) = row else { return };
            let idx = row.index();
            if idx < 0 {
                return;
            }
            if let Some(commit) = commits.borrow().get(idx as usize) {
                *selected_change.borrow_mut() = Some(commit.change_id_hex());
                buffer.set_text(&commit.description);
            }
        }
    });

    // Save: rewrite the selected commit's message, then reload.
    save_button.connect_clicked({
        let repo = repo.clone();
        let commits = commits.clone();
        let buffer = buffer.clone();
        let selected_change = selected_change.clone();
        let refresh = refresh.clone();
        move |_| {
            let Some(change) = selected_change.borrow().clone() else {
                return;
            };
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string();
            let target = commits
                .borrow()
                .iter()
                .find(|c| c.change_id_hex() == change)
                .map(|c| c.id.clone());
            let Some(target) = target else { return };
            if let Err(err) = repo.borrow_mut().rewrite_message(&target, &text) {
                eprintln!("commedit: save failed: {err:?}");
                return;
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

/// Replace the list's rows with one row per commit (subject + short id).
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
