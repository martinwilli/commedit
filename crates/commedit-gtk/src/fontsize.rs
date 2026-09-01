//! Text scaling for the whole window — the levels behind the header's text-size
//! dropdown, and the stylesheet that applies one.
//!
//! GTK inherits `font-size` down the CSS node tree, and a *percentage* resolves
//! against the size the theme already computed, so a single rule on the `window`
//! node scales everything below it: list rows, labels, popovers, the message
//! editor and the diff view alike. Working in percent (rather than reading the
//! theme font back and emitting an absolute size) keeps whatever the user's font
//! and text-scaling settings arrive at as the 100% baseline.

use std::cell::Cell;

use gtk::glib;
use gtk::prelude::*;

/// The offered levels, in percent of the theme's own font size.
pub(crate) const LEVELS: [u32; 6] = [50, 75, 100, 125, 150, 200];

/// The dropdown's labels — one per [`LEVELS`] entry, same order.
pub(crate) const LABELS: [&str; 6] = ["50%", "75%", "100%", "125%", "150%", "200%"];

/// The unscaled level: where a fresh install starts, and the fallback for a level
/// we don't offer (a hand-edited config file).
pub(crate) const DEFAULT_PCT: u32 = 100;

/// Dropdown position of `pct`, falling back to [`DEFAULT_PCT`]'s position.
pub(crate) fn index_of(pct: u32) -> u32 {
    LEVELS
        .iter()
        .position(|&l| l == pct)
        .or_else(|| LEVELS.iter().position(|&l| l == DEFAULT_PCT))
        .unwrap_or(0) as u32
}

/// The level at dropdown position `idx` — [`DEFAULT_PCT`] when out of range.
pub(crate) fn level_at(idx: u32) -> u32 {
    LEVELS.get(idx as usize).copied().unwrap_or(DEFAULT_PCT)
}

/// The stylesheet scaling the window's text to `pct`. At 100% it is *empty*
/// rather than an explicit `100%`: the theme's own size then stays untouched
/// instead of being round-tripped through our rule.
pub(crate) fn css(pct: u32) -> String {
    if pct == DEFAULT_PCT {
        String::new()
    } else {
        format!("window {{ font-size: {pct}%; }}")
    }
}

/// Run `f` once a font-size change has actually reached the widgets' Pango
/// contexts. Reloading the provider does not restyle synchronously: GTK
/// revalidates the style in a later frame, so anything that *measures* the font
/// rather than inheriting it still reads the old metrics from an idle callback —
/// and from the first frame tick too, that being the frame the restyle is
/// scheduled in. The second tick is the first point the new metrics are readable,
/// so wait for it and then stop ticking.
pub(crate) fn after_restyle(widget: &impl IsA<gtk::Widget>, f: impl Fn() + 'static) {
    let ticks = Cell::new(0u32);
    widget.add_tick_callback(move |_, _| {
        ticks.set(ticks.get() + 1);
        if ticks.get() < 2 {
            return glib::ControlFlow::Continue;
        }
        f();
        glib::ControlFlow::Break
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_match_the_levels() {
        assert_eq!(LABELS.len(), LEVELS.len());
        for (label, level) in LABELS.iter().zip(LEVELS) {
            assert_eq!(label.trim_end_matches('%').parse::<u32>().unwrap(), level);
        }
    }

    #[test]
    fn levels_round_trip_through_the_dropdown_position() {
        for level in LEVELS {
            assert_eq!(level_at(index_of(level)), level);
        }
    }

    #[test]
    fn unknown_levels_and_positions_fall_back_to_the_default() {
        // A config file naming a level we no longer offer, and a position past the
        // end of the list, both land on 100%.
        assert_eq!(level_at(index_of(133)), DEFAULT_PCT);
        assert_eq!(level_at(LEVELS.len() as u32), DEFAULT_PCT);
    }

    #[test]
    fn the_default_level_carries_no_rule() {
        assert!(css(DEFAULT_PCT).is_empty());
        assert_eq!(css(150), "window { font-size: 150%; }");
    }
}
