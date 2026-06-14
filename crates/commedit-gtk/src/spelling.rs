//! Interactive spell checking for the commit-message editor, via GNOME
//! libspelling. libspelling is built for `GtkSourceView`: its
//! `TextBufferAdapter` attaches to a `GtkSourceBuffer` and drives the
//! red-squiggle underlines plus a right-click corrections menu itself, so this
//! is mostly GTK glue — no checker logic of our own. Spell quality comes from
//! the system enchant dictionaries (no embedded wordlist). GTK-only; there is no
//! MCP/engine counterpart.
//!
//! libspelling owns the *runtime* but not the *preferences*: the personal
//! dictionary ("Add to Dictionary") is a per-language enchant file, and the
//! on/off state is just a property on the adapter. So we **pin the language**
//! (derived once from the locale) — otherwise libspelling re-derives it each
//! launch and the same user's added words land under a shifting tag (`en` one
//! run, `en_US` the next), so they seem to vanish — and persist both the
//! language and the enabled flag in a small file under the user config dir.

use std::fs;
use std::path::PathBuf;

use gtk::glib;
use gtk::prelude::{TextViewExt, WidgetExt};

/// Persisted spell-check preferences (see the module docs for why we keep them).
struct SpellSettings {
    enabled: bool,
    language: String,
}

impl SpellSettings {
    fn config_path() -> PathBuf {
        glib::user_config_dir()
            .join("commedit")
            .join("spelling.conf")
    }

    /// Read both fields back; `None` if the file is absent or incomplete (so we
    /// re-derive a fresh one rather than honour a half-written config).
    fn load() -> Option<Self> {
        let text = fs::read_to_string(Self::config_path()).ok()?;
        let mut enabled = None;
        let mut language = None;
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("enabled=") {
                enabled = Some(v.trim() == "true");
            } else if let Some(v) = line.strip_prefix("language=") {
                let v = v.trim();
                if !v.is_empty() {
                    language = Some(v.to_string());
                }
            }
        }
        Some(Self {
            enabled: enabled?,
            language: language?,
        })
    }

    fn save(&self) {
        let path = Self::config_path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(
            path,
            format!("enabled={}\nlanguage={}\n", self.enabled, self.language),
        );
    }
}

/// Pick a spell-check language for this user from the locale, validated against
/// the installed dictionaries so we never pin a tag with no dictionary (which
/// would silently disable checking). Tries the most specific locale tag first
/// (`en_US`), then less specific (`en`), then falls back to the provider's own
/// default code.
fn derive_language() -> String {
    let provider = libspelling::Provider::default();
    for name in glib::language_names() {
        let name = name.to_string();
        if name.is_empty() || name == "C" || name == "POSIX" {
            continue;
        }
        if provider.supports_language(&name) {
            return name;
        }
    }
    provider
        .default_code()
        .map(|g| g.to_string())
        .unwrap_or_else(|| "en_US".to_string())
}

/// Write the adapter's current language + enabled state back to disk, fired
/// whenever the user changes either through the right-click menu.
fn persist_from_adapter(adapter: &libspelling::TextBufferAdapter) {
    let language = adapter
        .language()
        .map(|g| g.to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(derive_language);
    SpellSettings {
        enabled: adapter.is_enabled(),
        language,
    }
    .save();
}

/// Attach libspelling's interactive checker to `view`/`buffer` (the commit
/// message editor): live misspelling underlines and a right-click suggestions
/// menu, with the language and enabled state restored from (and saved back to)
/// the user config. The view retains the adapter — it owns the inserted action
/// group and the extra menu model — so nothing needs to be stored by the caller.
pub fn attach(view: &sourceview5::View, buffer: &sourceview5::Buffer) {
    let settings = SpellSettings::load().unwrap_or_else(|| {
        let initial = SpellSettings {
            enabled: true,
            language: derive_language(),
        };
        initial.save();
        initial
    });

    // Pin the language (rather than `Checker::default()`'s unstable per-launch
    // derivation) so the personal dictionary file enchant writes is consistent.
    let checker = libspelling::Checker::new(None, Some(settings.language.as_str()));
    let adapter = libspelling::TextBufferAdapter::new(buffer, &checker);
    adapter.set_enabled(settings.enabled);
    view.set_extra_menu(Some(&adapter.menu_model()));
    view.insert_action_group("spelling", Some(&adapter));

    // The right-click menu lets the user toggle checking and switch language;
    // persist either change so it survives across sessions.
    adapter.connect_enabled_notify(persist_from_adapter);
    adapter.connect_language_notify(persist_from_adapter);
}
