//! Persist the window geometry across sessions, mirroring `spelling.rs`: a tiny
//! hand-rolled `key=value` file under the user config dir
//! (`~/.config/commedit/window.conf`), read with `std::fs` — no serde, no
//! GSettings schema. We remember the window size, its maximized state, and the
//! two paned divider positions (the commit-list width and the message-pane
//! height), and restore them on the next launch.
//!
//! We deliberately do **not** store the window *position*. GTK4 removed the
//! position-setting APIs, and under Wayland a client cannot read or set its own
//! placement at all — the compositor owns it; the GNOME HIG advises against
//! restoring position for the same reason. So the persisted set matches the
//! canonical gtk4-rs "save window state" recipe: size + maximized, here plus the
//! divider positions.

use std::fs;
use std::path::PathBuf;

use gtk::glib;

/// Persisted window geometry. The defaults are the values `build_ui` used as
/// hard-coded literals before this existed, so a fresh install opens identically.
pub struct WindowState {
    /// Window default (un-maximized) width.
    pub width: i32,
    /// Window default (un-maximized) height.
    pub height: i32,
    /// Whether the window was maximized on close.
    pub maximized: bool,
    /// Horizontal divider position — the commit-list (left pane) width.
    pub list_width: i32,
    /// Vertical divider position — the commit-message (top-right pane) height.
    pub message_height: i32,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: 1400,
            height: 900,
            maximized: false,
            list_width: 480,
            message_height: 200,
        }
    }
}

impl WindowState {
    fn config_path() -> PathBuf {
        glib::user_config_dir().join("commedit").join("window.conf")
    }

    /// Read the geometry back, falling back to the defaults when the file is
    /// absent or unreadable.
    pub fn load() -> Self {
        match fs::read_to_string(Self::config_path()) {
            Ok(text) => Self::from_text(&text),
            Err(_) => Self::default(),
        }
    }

    /// Write the geometry, creating `~/.config/commedit/` if missing. Errors are
    /// ignored — losing the geometry is a cosmetic regression, not worth a prompt.
    pub fn save(&self) {
        let path = Self::config_path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(path, self.to_text());
    }

    /// Parse the config text, seeded from [`Default`] so a partial/garbage file
    /// degrades gracefully per field — each line overrides its field only when it
    /// parses. Non-positive sizes are ignored so a corrupt file can never produce
    /// a degenerate window. Pure (no I/O) so it can be unit-tested.
    fn from_text(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "width" => set_positive(&mut s.width, value),
                "height" => set_positive(&mut s.height, value),
                "maximized" => s.maximized = value == "true",
                "list_width" => set_positive(&mut s.list_width, value),
                "message_height" => set_positive(&mut s.message_height, value),
                _ => {}
            }
        }
        s
    }

    /// Render the geometry as the config file's `key=value` text. Pure (no I/O).
    fn to_text(&self) -> String {
        format!(
            "width={}\nheight={}\nmaximized={}\nlist_width={}\nmessage_height={}\n",
            self.width, self.height, self.maximized, self.list_width, self.message_height,
        )
    }
}

/// Override `slot` with `value` only when it parses to a positive integer — a
/// missing, non-numeric or non-positive value leaves the default in place.
fn set_positive(slot: &mut i32, value: &str) {
    if let Ok(v) = value.parse::<i32>() {
        if v > 0 {
            *slot = v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_text() {
        let s = WindowState {
            width: 1234,
            height: 567,
            maximized: true,
            list_width: 321,
            message_height: 89,
        };
        let back = WindowState::from_text(&s.to_text());
        assert_eq!(back.width, 1234);
        assert_eq!(back.height, 567);
        assert!(back.maximized);
        assert_eq!(back.list_width, 321);
        assert_eq!(back.message_height, 89);
    }

    #[test]
    fn missing_keys_keep_defaults() {
        let d = WindowState::default();
        // Only the width is present; every other field falls back to its default.
        let s = WindowState::from_text("width=1000\n");
        assert_eq!(s.width, 1000);
        assert_eq!(s.height, d.height);
        assert_eq!(s.list_width, d.list_width);
        assert_eq!(s.message_height, d.message_height);
        assert_eq!(s.maximized, d.maximized);
    }

    #[test]
    fn garbage_and_non_positive_values_are_ignored() {
        let d = WindowState::default();
        let s = WindowState::from_text(
            "width=oops\nheight=0\nlist_width=-5\nmessage_height=\nnonsense\n=42\n",
        );
        assert_eq!(s.width, d.width);
        assert_eq!(s.height, d.height);
        assert_eq!(s.list_width, d.list_width);
        assert_eq!(s.message_height, d.message_height);
    }

    #[test]
    fn maximized_is_strict_about_true() {
        assert!(WindowState::from_text("maximized=true\n").maximized);
        assert!(!WindowState::from_text("maximized=false\n").maximized);
        assert!(!WindowState::from_text("maximized=1\n").maximized);
        assert!(!WindowState::default().maximized);
    }
}
