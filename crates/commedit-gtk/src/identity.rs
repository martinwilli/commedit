//! The author/committer identity fields above the message editor: the combined
//! "Name <email>" entries with their in-field identity picker, the date entries
//! with a calendar popover, and the conversions between an [`Identity`] and the
//! four entry widgets.

use std::cell::RefCell;
use std::rc::Rc;

use commedit_engine::history::CommitInfo;
use commedit_engine::rewrite::Identity;
use gtk::glib;
use gtk::prelude::*;
use gtk::{
    Box as GtkBox, Calendar, Entry, Label, ListBox, MenuButton, Orientation, PolicyType, Popover,
    ScrolledWindow,
};

/// A horizontally-expanding text entry with placeholder text, for an identity
/// name/email/date field.
pub(crate) fn identity_entry(placeholder: &str) -> Entry {
    let entry = Entry::new();
    entry.set_placeholder_text(Some(placeholder));
    entry.set_hexpand(true);
    entry
}

/// A date entry with a calendar button to its right, as a single grid cell.
pub(crate) fn date_field(date: &Entry) -> GtkBox {
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
pub(crate) fn attach_identity_picker(entry: &Entry, identities: &Rc<RefCell<Vec<(String, String)>>>) {
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
pub(crate) fn read_identity(fields: &[Entry; 4]) -> Identity {
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
pub(crate) fn set_identity_fields(fields: &[Entry; 4], commit: &CommitInfo) {
    fields[0].set_text(&join_name_email(&commit.author_name, &commit.author_email));
    fields[1].set_text(&commit.author_time);
    fields[2].set_text(&join_name_email(&commit.committer_name, &commit.committer_email));
    fields[3].set_text(&commit.committer_time);
}
