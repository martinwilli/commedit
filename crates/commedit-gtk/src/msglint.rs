//! Commit-message linting against the repository's **own** de-facto conventions —
//! a GTK-free, inline-tested core, the same pure-module shape as [`crate::search`].
//!
//! commedit is a general tool used on any repo, so it must not impose one house
//! style. Instead [`RepoStyle::learn`] infers the conventions a repo *already*
//! follows from its history (do its subjects carry a `type:` prefix? how is each
//! prefix token cased? capitalize the summary? avoid a trailing period? how long do
//! they run?), and [`lint_subject`] flags a subject that drifts from that learned
//! norm — never from a fixed ideal. With too small a sample, or no clear majority on
//! an axis, the linter simply has no opinion (an empty [`RepoStyle`] field), so it
//! stays quiet rather than nag. Prefix casing is learned **per token** so a
//! legitimately-uppercase proper-noun prefix (`NEWS:`, `README:`) is its own norm,
//! not a deviation from a global lowercase one.
//!
//! [`autofix_subject`] applies the *mechanical* corrections (re-case the prefix to
//! the repo's spelling of that token, flip the summary's first letter, strip a stray
//! trailing period) that can be made safely without guessing intent; the judgment
//! calls (a missing prefix, an over-long summary) are left for a human.

use std::collections::HashMap;

/// Minimum number of human-written subjects needed before the linter forms any
/// opinion at all. Below this a repo hasn't shown a convention to be consistent
/// with, so [`RepoStyle::learn`] returns the empty (no-opinion) style.
const MIN_SAMPLE: usize = 5;

/// Fraction of the sample that must agree before a convention is adopted. A repo
/// with no clear majority on an axis gets no lint on that axis — tier-2 linting is
/// "be consistent with this repo", not "follow a rule".
const MAJORITY: f64 = 0.75;

/// Absolute lower bound (chars) below which an over-long summary is never flagged,
/// regardless of the repo's own norm — git's conventional hard wrap. Pairs with the
/// learned [`RepoStyle::long_cutoff`] as a double gate, so a repo of terse subjects
/// isn't nagged about a merely-above-average one that's still fine by git standards.
const LONG_ABS_FLOOR: usize = 72;

/// How many times a prefix's casing must recur before it counts as the *canonical*
/// spelling of that token (see [`RepoStyle::prefix_casings`]). A prefix seen only
/// once is never the basis for flagging a differently-cased one — this is what keeps
/// a legitimately-uppercase proper-noun prefix (`NEWS:` for the NEWS file, `README:`
/// for README.md) from being "corrected": it's simply that token's own canonical
/// casing, while a one-off `GTK:` against fifty `gtk:` is flagged because `gtk` is.
const MIN_PREFIX_OCCURRENCES: usize = 2;

/// What a single lint flags. Carries no fix itself — [`autofix_subject`] handles the
/// mechanical ones; this is what the badge tooltip explains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LintKind {
    /// Most commits here carry a `type:`/`subsystem:` prefix; this one has none.
    /// Not auto-fixable — we can't guess which prefix it should be.
    MissingPrefix,
    /// The prefix's case is the wrong way for this repo's norm (e.g. `Doc:`/`GTK:`
    /// in a repo of lowercase `doc:`/`gtk:` prefixes). Auto-fixable (re-case it).
    PrefixCapitalization,
    /// The summary's first letter is the wrong case for this repo's norm.
    /// Auto-fixable (flip the case).
    Capitalization,
    /// The summary ends with a period, against this repo's norm. Auto-fixable (strip).
    TrailingPeriod,
    /// The summary runs much longer than this repo's subjects (and past the git wrap
    /// floor). Not auto-fixable — prose can't be shortened mechanically.
    TooLong,
}

impl LintKind {
    /// Whether [`autofix_subject`] can correct this kind without guessing intent.
    pub(crate) fn auto_fixable(self) -> bool {
        matches!(
            self,
            LintKind::PrefixCapitalization | LintKind::Capitalization | LintKind::TrailingPeriod
        )
    }
}

/// A single finding for one subject: the kind plus a human-readable explanation
/// (shown in the badge tooltip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lint {
    pub(crate) kind: LintKind,
    pub(crate) message: String,
}

impl Lint {
    fn new(kind: LintKind, message: &str) -> Self {
        Lint {
            kind,
            message: message.to_string(),
        }
    }

    pub(crate) fn auto_fixable(&self) -> bool {
        self.kind.auto_fixable()
    }
}

/// The conventions a repository de-facto follows, inferred from its own history.
/// Every field is "no opinion" by default ([`Default`]), so an unlearned or
/// undecided axis yields no lint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RepoStyle {
    /// The repo predominantly prefixes subjects with `type:`/`subsystem:`.
    requires_prefix: bool,
    /// The **canonical casing** of each prefix token the repo uses, keyed by the
    /// token lowercased: `"news" → "NEWS"`, `"gtk" → "gtk"`. Learned *per token* (not
    /// as one global lower/upper norm) so a legitimately-capitalized proper-noun
    /// prefix is its own canonical form and never flagged, while a one-off
    /// mis-casing of an established token is. Only tokens whose dominant casing is
    /// clear (≥ [`MAJORITY`]) and established (≥ [`MIN_PREFIX_OCCURRENCES`]) appear.
    prefix_casings: HashMap<String, String>,
    /// The repo's summary capitalization norm: `Some(true)` = capitalized first
    /// letter, `Some(false)` = lowercase, `None` = no clear convention.
    capitalized: Option<bool>,
    /// The repo predominantly avoids a trailing period on the summary.
    no_trailing_period: bool,
    /// Summaries longer than this (chars) are outliers for this repo; `None` when the
    /// sample is too small to judge.
    long_cutoff: Option<usize>,
}

impl RepoStyle {
    /// Learn the de-facto conventions from a repo's subjects (one per commit, in any
    /// order). Auto-generated subjects (merges, reverts, un-squashed fixups, the
    /// initial commit) carry no authorial style, so they're excluded from the
    /// sample. With fewer than [`MIN_SAMPLE`] human subjects the result is the empty
    /// style (no opinions).
    pub(crate) fn learn(subjects: &[&str]) -> RepoStyle {
        let samples: Vec<&str> = subjects
            .iter()
            .copied()
            .filter(|s| !s.trim().is_empty() && !is_autogen(s))
            .collect();
        if samples.len() < MIN_SAMPLE {
            return RepoStyle::default();
        }
        let n = samples.len() as f64;

        // Prefix: adopted only if a strong majority carry one. The "no prefix" norm
        // needs no field — we simply never flag a missing prefix unless it's required.
        let with_prefix = samples.iter().filter(|s| has_prefix(s)).count();
        let requires_prefix = with_prefix as f64 / n >= MAJORITY;

        // Capitalization of the *description* (after any prefix), voting only over
        // subjects whose description starts with a letter (others carry no case).
        let mut upper = 0usize;
        let mut cap_total = 0usize;
        for s in &samples {
            if let Some(is_upper) = first_alpha_upper(description_part(s)) {
                cap_total += 1;
                upper += usize::from(is_upper);
            }
        }
        let capitalized = majority_case(upper, cap_total);

        // Canonical casing of each prefix token, learned per token (not as one global
        // lower/upper norm). Tally each exact casing per lowercased key, then keep the
        // dominant one when it's both clear (≥ MAJORITY) and established
        // (≥ MIN_PREFIX_OCCURRENCES) — so `NEWS:`/`README:` are their own canonical
        // form, a one-off `GTK:` against many `gtk:` is not.
        let mut tally: HashMap<String, HashMap<&str, usize>> = HashMap::new();
        for s in &samples {
            if let Some(token) = prefix_token(s) {
                *tally
                    .entry(token.to_lowercase())
                    .or_default()
                    .entry(token)
                    .or_insert(0) += 1;
            }
        }
        let prefix_casings: HashMap<String, String> = tally
            .into_iter()
            .filter_map(|(key, casings)| {
                let total: usize = casings.values().sum();
                let (token, &count) = casings.iter().max_by_key(|(_, &c)| c)?;
                ((count >= MIN_PREFIX_OCCURRENCES) && (count as f64 / total as f64 >= MAJORITY))
                    .then(|| (key, token.to_string()))
            })
            .collect();

        // Trailing period: adopted only if a strong majority avoid one.
        let without_period = samples
            .iter()
            .filter(|s| !s.trim_end().ends_with('.'))
            .count();
        let no_trailing_period = without_period as f64 / n >= MAJORITY;

        // Length: flag well past the 90th percentile of this repo's subject lengths,
        // gated additionally by the absolute git floor at lint time.
        let mut lens: Vec<usize> = samples.iter().map(|s| s.chars().count()).collect();
        lens.sort_unstable();
        let p90 = lens[((lens.len() as f64 * 0.9) as usize).min(lens.len() - 1)];
        let long_cutoff = Some((p90 as f64 * 1.5) as usize);

        RepoStyle {
            requires_prefix,
            prefix_casings,
            capitalized,
            no_trailing_period,
            long_cutoff,
        }
    }
}

/// Decide a majority case from a tally of `upper` uppercase out of `total` voters:
/// `Some(true)` if a [`MAJORITY`] are uppercase, `Some(false)` if a majority are
/// lowercase, `None` if neither side dominates or the tally is below [`MIN_SAMPLE`].
/// Shared by the summary- and prefix-casing axes.
fn majority_case(upper: usize, total: usize) -> Option<bool> {
    if total < MIN_SAMPLE {
        return None;
    }
    let frac_upper = upper as f64 / total as f64;
    if frac_upper >= MAJORITY {
        Some(true)
    } else if 1.0 - frac_upper >= MAJORITY {
        Some(false)
    } else {
        None
    }
}

/// Flag the ways `subject` drifts from the repo's learned `style`. Empty when the
/// subject conforms, when it's auto-generated (merges/reverts/fixups carry no
/// authorial style), or when `style` has no opinion (small sample / undecided).
pub(crate) fn lint_subject(subject: &str, style: &RepoStyle) -> Vec<Lint> {
    let mut lints = Vec::new();
    if subject.trim().is_empty() || is_autogen(subject) {
        return lints;
    }

    if style.requires_prefix && !has_prefix(subject) {
        lints.push(Lint::new(
            LintKind::MissingPrefix,
            "Most commits here start the summary with a “type:” prefix — this one doesn't.",
        ));
    }

    if let Some(token) = prefix_token(subject) {
        if let Some(canonical) = style.prefix_casings.get(&token.to_lowercase()) {
            if token != canonical {
                lints.push(Lint::new(
                    LintKind::PrefixCapitalization,
                    &format!("Most commits here write this prefix “{canonical}:”, not “{token}:”."),
                ));
            }
        }
    }

    if let Some(want_upper) = style.capitalized {
        if let Some(is_upper) = first_alpha_upper(description_part(subject)) {
            if want_upper && !is_upper {
                lints.push(Lint::new(
                    LintKind::Capitalization,
                    "Most commits here capitalize the summary — this one starts lowercase.",
                ));
            } else if !want_upper && is_upper {
                lints.push(Lint::new(
                    LintKind::Capitalization,
                    "Most commits here start the summary lowercase — this one is capitalized.",
                ));
            }
        }
    }

    if style.no_trailing_period && has_lone_trailing_period(subject) {
        lints.push(Lint::new(
            LintKind::TrailingPeriod,
            "Most commits here don't end the summary with a period.",
        ));
    }

    if let Some(cutoff) = style.long_cutoff {
        let len = subject.chars().count();
        if len > cutoff && len > LONG_ABS_FLOOR {
            lints.push(Lint::new(
                LintKind::TooLong,
                "This summary runs much longer than most here — consider trimming it.",
            ));
        }
    }

    lints
}

/// Apply the mechanical fixes (trailing period, prefix and summary casing) `style`
/// warrants to `subject`, returning the corrected subject — or `None` when nothing
/// safely auto-fixable applies. Never touches the prose itself, so a missing prefix
/// or an over-long summary is deliberately left for the user.
pub(crate) fn autofix_subject(subject: &str, style: &RepoStyle) -> Option<String> {
    if subject.trim().is_empty() || is_autogen(subject) {
        return None;
    }
    let mut s = subject.to_string();
    let mut changed = false;

    // Strip a lone trailing period (an intentional "…" ellipsis is left alone).
    if style.no_trailing_period && has_lone_trailing_period(&s) {
        s = s.trim_end().to_string();
        s.pop(); // the '.'
        s = s.trim_end().to_string();
        changed = true;
    }

    // Flip the description's first letter to the repo's case norm.
    if let Some(want_upper) = style.capitalized {
        let start = description_start(&s);
        if let Some(first) = s[start..].chars().next() {
            if first.is_alphabetic() && first.is_uppercase() != want_upper {
                let flipped: String = if want_upper {
                    first.to_uppercase().collect()
                } else {
                    first.to_lowercase().collect()
                };
                let prefix = s[..start].to_string();
                let rest = s[start + first.len_utf8()..].to_string();
                s = format!("{prefix}{flipped}{rest}");
                changed = true;
            }
        }
    }

    // Re-case the prefix token to the repo's canonical spelling of that token (e.g.
    // `GTK:` → `gtk:`, or `news:` → `NEWS:` if that's how the repo writes it). Done
    // last so it can't invalidate the offsets used above.
    if let Some(colon) = prefix_colon(&s) {
        let token = &s[..colon];
        if let Some(canonical) = style.prefix_casings.get(&token.to_lowercase()) {
            if token != canonical {
                let rest = s[colon..].to_string();
                s = format!("{canonical}{rest}");
                changed = true;
            }
        }
    }

    changed.then_some(s)
}

/// Replace the first line (subject) of a full commit `description` with
/// `new_subject`, preserving the body. Used to splice an auto-fixed summary back
/// into the message handed to the engine's `rewrite_message`.
pub(crate) fn replace_subject(description: &str, new_subject: &str) -> String {
    match description.split_once('\n') {
        Some((_, rest)) => format!("{new_subject}\n{rest}"),
        None => new_subject.to_string(),
    }
}

/// Whether a subject is auto-generated boilerplate rather than authored prose —
/// merges, reverts, un-squashed autosquash fixups, the initial commit. Excluded
/// from both the learned sample and from linting (it carries no house style).
fn is_autogen(subject: &str) -> bool {
    let s = subject.trim_start();
    s == "Initial commit"
        || [
            "Merge ", "Revert ", "Reapply ", "fixup! ", "squash! ", "amend! ",
        ]
        .iter()
        .any(|p| s.starts_with(p))
}

/// The byte offset where the description begins: just past a `type:`/`scope:` prefix
/// and its following space, or 0 when there's no prefix.
fn description_start(subject: &str) -> usize {
    prefix_desc_start(subject).unwrap_or(0)
}

/// The description part of a subject — everything after a prefix, or the whole
/// subject when unprefixed.
fn description_part(subject: &str) -> &str {
    &subject[description_start(subject)..]
}

/// The byte index of the colon ending a valid `type:` prefix, or `None` when the
/// subject has no prefix.
fn prefix_colon(subject: &str) -> Option<usize> {
    prefix_desc_start(subject)?;
    subject.find(':')
}

/// The prefix token (the `type`/`type(scope)` before the colon) when `subject` has a
/// valid prefix, else `None`.
fn prefix_token(subject: &str) -> Option<&str> {
    prefix_colon(subject).map(|colon| &subject[..colon])
}

/// If `subject` opens with a conventional `type:` / `type(scope):` / `subsystem:`
/// prefix followed by a space, return the byte offset of the description after it
/// (`"doc: Foo"` → 5). A mid-sentence colon (`"Fix bug: detail"` — the part before
/// the colon has a space) or a non-space colon (`"https://…"`) is not a prefix.
fn prefix_desc_start(subject: &str) -> Option<usize> {
    let colon = subject.find(':')?;
    let token = &subject[..colon];
    if !is_prefix_token(token) {
        return None;
    }
    match subject[colon + 1..].chars().next() {
        None => Some(subject.len()),  // "wip:" — empty description
        Some(' ') => Some(colon + 2), // skip ": "
        Some(_) => None,
    }
}

/// Does the part before the colon look like a single prefix token — one whitespace-
/// free word of identifier-ish chars, optionally trailed by a `(scope)`? The `+`
/// admits multi-subsystem prefixes some repos use (`history+mcp:`, `gtk+repo:`); the
/// no-whitespace rule is what actually keeps a mid-sentence colon from passing.
fn is_prefix_token(token: &str) -> bool {
    if token.is_empty() || token.contains(char::is_whitespace) {
        return false;
    }
    let core = token.split('(').next().unwrap_or(token);
    !core.is_empty()
        && core
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '+'))
}

fn has_prefix(subject: &str) -> bool {
    prefix_desc_start(subject).is_some()
}

/// The case of the first letter of `desc`: `Some(true)` upper, `Some(false)` lower,
/// `None` when it's empty or starts with a non-letter (no case to learn from).
fn first_alpha_upper(desc: &str) -> Option<bool> {
    let c = desc.trim_start().chars().next()?;
    c.is_alphabetic().then(|| c.is_uppercase())
}

/// Whether `subject` ends in a single sentence-style period — not a deliberate
/// "…" ellipsis (which we neither flag nor strip).
fn has_lone_trailing_period(subject: &str) -> bool {
    let t = subject.trim_end();
    t.ends_with('.') && !t.ends_with("..")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repo whose history clearly uses lowercase `prefix: Capitalized` summaries
    /// with no trailing period — like this one. Prefixes recur so they have an
    /// established canonical casing (`gtk`, `mcp`, `doc`).
    fn prefixed_capitalized_repo() -> RepoStyle {
        RepoStyle::learn(&[
            "gtk: Add the parser",
            "gtk: Fix the crash",
            "mcp: Add a tool",
            "mcp: Wire the handler",
            "doc: Document the API",
            "doc: Note the caveat",
            "engine: Refactor the planner",
            "test: Cover the edge case",
        ])
    }

    #[test]
    fn small_sample_has_no_opinion() {
        let style = RepoStyle::learn(&["fix: a", "feat: b"]);
        assert_eq!(style, RepoStyle::default());
        // And so nothing is ever flagged.
        assert!(lint_subject("whatever lowercase no prefix.", &style).is_empty());
    }

    #[test]
    fn learns_prefix_summary_caps_and_period() {
        let style = prefixed_capitalized_repo();
        assert!(style.requires_prefix);
        assert_eq!(
            style.prefix_casings.get("gtk").map(String::as_str),
            Some("gtk")
        );
        assert_eq!(style.capitalized, Some(true)); // Capitalized summary
        assert!(style.no_trailing_period);
        // A prefix seen only once has no established canonical casing.
        assert!(!style.prefix_casings.contains_key("engine"));
    }

    #[test]
    fn flags_missing_prefix_against_a_prefixed_repo() {
        let style = prefixed_capitalized_repo();
        let kinds: Vec<_> = lint_subject("Tidy the thing", &style)
            .iter()
            .map(|l| l.kind)
            .collect();
        assert_eq!(kinds, vec![LintKind::MissingPrefix]);
    }

    #[test]
    fn flags_lowercase_and_trailing_period() {
        let style = prefixed_capitalized_repo();
        let kinds: Vec<_> = lint_subject("gtk: tidy the thing.", &style)
            .iter()
            .map(|l| l.kind)
            .collect();
        assert_eq!(
            kinds,
            vec![LintKind::Capitalization, LintKind::TrailingPeriod]
        );
    }

    #[test]
    fn conforming_subject_is_clean() {
        let style = prefixed_capitalized_repo();
        assert!(lint_subject("gtk: Tidy the thing", &style).is_empty());
    }

    #[test]
    fn flags_and_fixes_an_uppercase_prefix() {
        let style = prefixed_capitalized_repo();
        // The summary conforms; only the prefix is shouted.
        let kinds: Vec<_> = lint_subject("GTK: Tidy the thing", &style)
            .iter()
            .map(|l| l.kind)
            .collect();
        assert_eq!(kinds, vec![LintKind::PrefixCapitalization]);
        // The whole prefix token is lowercased (an acronym included), not just its
        // first letter — so `MCP:` becomes `mcp:`, never `mCP:`.
        assert_eq!(
            autofix_subject("GTK: Tidy the thing", &style).as_deref(),
            Some("gtk: Tidy the thing")
        );
        assert_eq!(
            autofix_subject("MCP: Add a tool", &style).as_deref(),
            Some("mcp: Add a tool")
        );
    }

    #[test]
    fn capitalized_prefix_repo_flags_and_fixes_a_lowercase_prefix() {
        // Title-case prefixes that recur become the canonical casing.
        let style = RepoStyle::learn(&[
            "Gtk: Add the parser",
            "Gtk: Fix the crash",
            "Mcp: Add a tool",
            "Mcp: Wire the handler",
            "Doc: Document the API",
            "Doc: Note the caveat",
        ]);
        assert_eq!(
            style.prefix_casings.get("gtk").map(String::as_str),
            Some("Gtk")
        );
        let kinds: Vec<_> = lint_subject("gtk: Fix it", &style)
            .iter()
            .map(|l| l.kind)
            .collect();
        assert_eq!(kinds, vec![LintKind::PrefixCapitalization]);
        assert_eq!(
            autofix_subject("gtk: Fix it", &style).as_deref(),
            Some("Gtk: Fix it")
        );
    }

    #[test]
    fn proper_noun_prefixes_are_not_flagged() {
        // A lowercase-prefix repo that *also* uses legitimately-uppercase proper-noun
        // prefixes for specific files (the davici-work case). Each established casing
        // is its own canonical form, learned per token.
        let style = RepoStyle::learn(&[
            "gtk: Add the parser",
            "gtk: Fix the crash",
            "mcp: Add a tool",
            "engine: Refactor it",
            "doc: Document it",
            "NEWS: Note the 1.2 release",
            "NEWS: Note the 1.3 release",
            "README: Document the build",
            "README: Update the badges",
        ]);
        // Proper-noun prefixes match their own canonical casing → never flagged.
        assert!(lint_subject("NEWS: Note the 1.4 release", &style).is_empty());
        assert!(lint_subject("README: Fix a typo", &style).is_empty());
        // A one-off mis-casing of an *established* lowercase token still is.
        assert_eq!(
            lint_subject("GTK: Add a widget", &style)
                .iter()
                .map(|l| l.kind)
                .collect::<Vec<_>>(),
            vec![LintKind::PrefixCapitalization]
        );
        assert_eq!(
            autofix_subject("GTK: Add a widget", &style).as_deref(),
            Some("gtk: Add a widget")
        );
        // A brand-new prefix seen once is left alone — it could be a new proper noun.
        assert!(lint_subject("Makefile: Add a target", &style).is_empty());
    }

    #[test]
    fn autofix_recases_prefix_summary_and_period_together() {
        let style = prefixed_capitalized_repo();
        assert_eq!(
            autofix_subject("GTK: handle it.", &style).as_deref(),
            Some("gtk: Handle it")
        );
    }

    #[test]
    fn lowercase_repo_flags_a_capitalized_summary() {
        // A conventional-commits repo that keeps the summary lowercase.
        let style = RepoStyle::learn(&[
            "fix: handle the null case",
            "feat: add the widget",
            "docs: update the readme",
            "chore: bump deps",
            "refactor: split the module",
            "test: cover the path",
        ]);
        assert_eq!(style.capitalized, Some(false));
        let kinds: Vec<_> = lint_subject("fix: Handle the null case", &style)
            .iter()
            .map(|l| l.kind)
            .collect();
        assert_eq!(kinds, vec![LintKind::Capitalization]);
    }

    #[test]
    fn merges_and_reverts_are_exempt() {
        let style = prefixed_capitalized_repo();
        // Auto-generated, lowercase-ish, no prefix — yet never flagged.
        assert!(lint_subject("Merge branch 'feature/x'", &style).is_empty());
        assert!(lint_subject("Revert \"gtk: Add a thing\"", &style).is_empty());
        assert!(lint_subject("fixup! gtk: Add a thing", &style).is_empty());
    }

    #[test]
    fn mid_sentence_colon_is_not_a_prefix() {
        assert!(!has_prefix("Fix bug: it crashed")); // "Fix bug" has a space
        assert!(!has_prefix("See https://example.com")); // colon not followed by space
        assert!(has_prefix("doc: Something"));
        assert!(has_prefix("feat(api): Something"));
        assert!(has_prefix("wip:")); // empty description, still a prefix
                                     // Multi-subsystem prefixes (the `+` form this repo uses) are real prefixes,
                                     // and the description starts *after* them.
        assert!(has_prefix("history+mcp: Abbreviate emitted commit ids"));
        assert_eq!(
            description_part("gtk+repo: Drop the message"),
            "Drop the message"
        );
    }

    #[test]
    fn autofix_strips_period_and_fixes_case() {
        let style = prefixed_capitalized_repo();
        assert_eq!(
            autofix_subject("gtk: tidy the thing.", &style).as_deref(),
            Some("gtk: Tidy the thing")
        );
    }

    #[test]
    fn autofix_leaves_ellipsis_and_returns_none_when_clean() {
        let style = prefixed_capitalized_repo();
        // An intentional ellipsis is not a stray period.
        assert_eq!(autofix_subject("gtk: More to come...", &style), None);
        // Already conforming → nothing to do.
        assert_eq!(autofix_subject("gtk: Tidy the thing", &style), None);
        // Only a missing prefix (not mechanically fixable) → None.
        assert_eq!(autofix_subject("Tidy the thing", &style), None);
    }

    #[test]
    fn autofix_lowercases_for_a_lowercase_repo() {
        let style = RepoStyle::learn(&[
            "fix: handle the null case",
            "feat: add the widget",
            "docs: update the readme",
            "chore: bump deps",
            "refactor: split the module",
            "test: cover the path",
        ]);
        assert_eq!(
            autofix_subject("fix: Handle it", &style).as_deref(),
            Some("fix: handle it")
        );
    }

    #[test]
    fn too_long_needs_both_repo_outlier_and_git_floor() {
        // A repo of short subjects: cutoff ≈ p90*1.5 but the absolute floor (72)
        // keeps a merely-longer-than-average subject from being flagged.
        let style = RepoStyle::learn(&[
            "doc: Short one",
            "mcp: Another short",
            "gtk: Brief",
            "engine: Tiny",
            "ci: Mini",
            "test: Small",
        ]);
        assert!(style.long_cutoff.is_some());
        // 60 chars: longer than the repo norm but under the git floor → not flagged.
        let medium = format!("gtk: {}", "x".repeat(55));
        assert!(!lint_subject(&medium, &style)
            .iter()
            .any(|l| l.kind == LintKind::TooLong));
        // 90 chars: past both gates → flagged.
        let long = format!("gtk: {}", "x".repeat(85));
        assert!(lint_subject(&long, &style)
            .iter()
            .any(|l| l.kind == LintKind::TooLong));
    }

    #[test]
    fn replace_subject_preserves_the_body() {
        assert_eq!(
            replace_subject("old subject\n\nbody line\n", "new subject"),
            "new subject\n\nbody line\n"
        );
        assert_eq!(replace_subject("only subject", "new"), "new");
    }
}
