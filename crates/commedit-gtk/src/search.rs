//! Commit-search matching: a GTK-free substring/term matcher and the
//! Pango-markup highlighter that paints the matched characters. The header's
//! search entry drives [`crate::rows::apply_search_highlight`], which builds on
//! these two pure helpers (so the matching logic is unit-testable headless).

use gtk::glib;

/// Case-fold a single char to one char. `char::to_lowercase` may expand to
/// several chars (e.g. `İ`), but commit subjects are overwhelmingly 1:1 here, and
/// folding to a single char keeps the returned positions aligned to the original
/// string's `char` indices — which the highlighter needs.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Case-insensitive, term-based substring match of `query` against `haystack` —
/// the "natural" find behaviour git tools use (gitk, tig, `git log --grep`),
/// **not** a fuzzy subsequence (which over-matches by accepting the typed chars
/// scattered anywhere). `query` is split on whitespace into terms; every term must
/// occur literally as a substring, **order-independent** (the AND-search of a
/// search box: `"fix login"` matches "Login: fix redirect"). Returns the matched
/// **character indices** in `haystack` (ascending, deduped — every occurrence of
/// every term) when all terms are found, else `None`. An all-whitespace/empty
/// query is "no active search" and yields `None`.
pub(crate) fn search_match(haystack: &str, query: &str) -> Option<Vec<usize>> {
    let terms: Vec<&str> = query.split_whitespace().collect();
    if terms.is_empty() {
        return None;
    }
    // Fold to single chars so comparison is case-insensitive while the indices
    // stay aligned to `haystack`'s own `char` offsets (the highlighter's space).
    let hay: Vec<char> = haystack.chars().map(fold).collect();
    let mut positions = Vec::new();
    for term in terms {
        let needle: Vec<char> = term.chars().map(fold).collect();
        // `split_whitespace` never yields an empty term, and an over-long needle
        // can't fit — either way it can't match, so the query fails.
        if needle.is_empty() || needle.len() > hay.len() {
            return None;
        }
        let mut found = false;
        for start in 0..=(hay.len() - needle.len()) {
            if hay[start..start + needle.len()] == needle[..] {
                found = true;
                positions.extend(start..start + needle.len());
            }
        }
        if !found {
            return None;
        }
    }
    positions.sort_unstable();
    positions.dedup();
    Some(positions)
}

/// Render `subject` as Pango markup with the characters at `positions` (ascending
/// char indices, e.g. from [`search_match`]) highlighted. Every char is escaped;
/// maximal runs of matched indices share one `<span>` to keep the markup compact.
pub(crate) fn highlight_markup(subject: &str, positions: &[usize]) -> String {
    let mut out = String::new();
    let mut next = positions.iter().copied().peekable();
    let mut in_span = false;
    for (i, ch) in subject.chars().enumerate() {
        let matched = next.peek() == Some(&i);
        if matched {
            next.next();
        }
        if matched && !in_span {
            // A find-in-page yellow with a forced black foreground so it stays
            // legible on both light and dark themes.
            out.push_str("<span background=\"#f6d32d\" foreground=\"#000000\" weight=\"bold\">");
            in_span = true;
        } else if !matched && in_span {
            out.push_str("</span>");
            in_span = false;
        }
        let mut buf = [0u8; 4];
        out.push_str(&glib::markup_escape_text(ch.encode_utf8(&mut buf)));
    }
    if in_span {
        out.push_str("</span>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_hit_returns_contiguous_indices() {
        // "oob" matches the contiguous run o(1) o(2) b(3) in "foobar".
        assert_eq!(search_match("foobar", "oob"), Some(vec![1, 2, 3]));
        assert_eq!(search_match("foobar", "foo"), Some(vec![0, 1, 2]));
    }

    #[test]
    fn scattered_chars_no_longer_match() {
        // The old fuzzy matcher accepted "fb" (f…b scattered); substring search
        // does not — this is the over-matching the change fixes.
        assert_eq!(search_match("foobar", "fb"), None);
        assert_eq!(search_match("foobar", "xyz"), None);
    }

    #[test]
    fn terms_are_order_independent_and_and_ed() {
        // Both terms present, in either order → union of their occurrences.
        assert_eq!(
            search_match("foo bar", "bar foo"),
            Some(vec![0, 1, 2, 4, 5, 6])
        );
        // One term missing → no match.
        assert_eq!(search_match("foo bar", "foo baz"), None);
    }

    #[test]
    fn every_occurrence_is_highlighted() {
        // "ab" occurs at 0..2 and 3..5 in "abxab"; both runs are returned.
        assert_eq!(search_match("abxab", "ab"), Some(vec![0, 1, 3, 4]));
    }

    #[test]
    fn empty_or_whitespace_query_is_no_match() {
        assert_eq!(search_match("foobar", ""), None);
        assert_eq!(search_match("foobar", "   "), None);
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert_eq!(
            search_match("FooBar", "foobar"),
            Some(vec![0, 1, 2, 3, 4, 5])
        );
        assert_eq!(search_match("foobar", "FOO"), Some(vec![0, 1, 2]));
    }

    #[test]
    fn highlight_escapes_and_groups_runs() {
        // Match the contiguous "a<" run: one span, with the '<' escaped.
        let subject = "a<b";
        let pos = search_match(subject, "a<").unwrap();
        assert_eq!(pos, vec![0, 1]);
        assert_eq!(
            highlight_markup(subject, &pos),
            "<span background=\"#f6d32d\" foreground=\"#000000\" weight=\"bold\">a&lt;</span>b"
        );
    }

    #[test]
    fn highlight_no_positions_just_escapes() {
        assert_eq!(highlight_markup("a&b", &[]), "a&amp;b");
    }
}
