//! Interactive spell checking for the commit-message editor, via GNOME
//! libspelling. libspelling is built for `GtkSourceView`: its
//! `TextBufferAdapter` attaches to a `GtkSourceBuffer` and drives the
//! red-squiggle underlines plus a right-click corrections menu itself, so this
//! is pure GTK glue — no checker logic of our own. Spell quality comes from the
//! system enchant dictionaries (no embedded wordlist). GTK-only; there is no
//! MCP/engine counterpart.

use gtk::prelude::{TextViewExt, WidgetExt};

/// Attach libspelling's interactive checker to `view`/`buffer` (the commit
/// message editor): live misspelling underlines and a right-click suggestions
/// menu. The view retains the adapter — it owns the inserted action group and
/// the extra menu model — so nothing needs to be stored by the caller.
pub fn attach(view: &sourceview5::View, buffer: &sourceview5::Buffer) {
    let adapter = libspelling::TextBufferAdapter::new(buffer, &libspelling::Checker::default());
    adapter.set_enabled(true);
    view.set_extra_menu(Some(&adapter.menu_model()));
    view.insert_action_group("spelling", Some(&adapter));
}
