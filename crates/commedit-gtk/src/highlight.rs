//! Diff/conflict syntax highlighting and the inline "pill" banner buttons: the
//! static TextTag palette, syntect language colouring of diff and conflict text
//! (with parser state reset at each region boundary), and the cue geometry the
//! painters share with the click/hover hit-tests.

use commedit_engine::diff::{
    classify_conflict_lines, parse_diff_lines, ConflictLineKind, DiffLineKind,
};
use gtk::prelude::*;
use gtk::TextTag;
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::SyntaxSet;

use crate::conflict_header_path;
use crate::state::{CONFLICT_CUE_LABEL, CONFLICT_STRUCTURAL_NOTICE, CUE_CAP_L, CUE_CAP_R};

/// Wrap a cue label in the banner caps, e.g. `↕ expand context` -> `◀ ↕ expand context ▶`.
pub(crate) fn pill(label: &str) -> String {
    format!("{CUE_CAP_L} {label} {CUE_CAP_R}")
}

/// The inline pills (`◀ … ▶`) on `raw`, as `(left_cap, right_cap, label)` where
/// the caps are *character* offsets (matching GTK's `line_offset`) and `label` is
/// the trimmed text between them. A diff `@@` line can carry two pills (expand +
/// revert); a `diff --git` line one (revert file); conflict lines exactly one.
/// Shared by the painting and the click/hover hit-test so they always agree.
pub(crate) fn pills_on_line(raw: &str) -> Vec<(usize, usize, String)> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == CUE_CAP_L {
            if let Some(off) = chars[i + 1..].iter().position(|&c| c == CUE_CAP_R) {
                let j = i + 1 + off;
                let label: String = chars[i + 1..j].iter().collect();
                out.push((i, j, label.trim().to_string()));
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Paint one inline banner button spanning character columns `[lc, rc]` (the two
/// caps) on buffer line `line`: the end-caps get `cap_tag` (a coloured triangle on
/// the line background), the run between them `body_tag` (the solid button fill).
fn paint_pill_span(
    buffer: &sourceview5::Buffer,
    line: i32,
    lc: i32,
    rc: i32,
    cap_tag: &str,
    body_tag: &str,
) {
    let table = buffer.tag_table();
    let (Some(cap), Some(body)) = (table.lookup(cap_tag), table.lookup(body_tag)) else {
        return;
    };
    apply_cols(buffer, line, lc, lc + 1, &cap);
    if rc > lc + 1 {
        apply_cols(buffer, line, lc + 1, rc, &body);
    }
    apply_cols(buffer, line, rc, rc + 1, &cap);
}

/// Paint the first inline banner button on `raw` with the given tags. Used for the
/// single-pill conflict cues (the "use …" and elision cues). No-op if none.
fn paint_pill(buffer: &sourceview5::Buffer, line: i32, raw: &str, cap_tag: &str, body_tag: &str) {
    if let Some((lc, rc, _)) = pills_on_line(raw).into_iter().next() {
        paint_pill_span(buffer, line, lc as i32, rc as i32, cap_tag, body_tag);
    }
}

/// Paint every pill on a diff `@@` / `diff --git` line, colouring each by its
/// label: the revert cues use the warning palette, the "expand context" cue the
/// blue accent. The single-pill [`paint_pill`] can't handle the two pills a
/// reverted hunk's header carries.
fn paint_diff_pills(buffer: &sourceview5::Buffer, line: i32, raw: &str) {
    for (lc, rc, label) in pills_on_line(raw) {
        let (cap, body) = if label.contains("revert") {
            ("revert-cue-cap", "revert-cue")
        } else {
            ("expand-cue-cap", "expand-cue")
        };
        paint_pill_span(buffer, line, lc as i32, rc as i32, cap, body);
    }
}

/// Create the static, named tags used for diff line backgrounds and intra-line
/// emphasis (idempotent). Per-syntax foreground tags are created lazily in
/// [`fg_tag`]. Colors follow GitHub's light diff palette.
pub(crate) fn install_diff_tags(buffer: &sourceview5::Buffer) {
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
    // Conflict-resolution pane: "our" side, "their" side, and the marker lines.
    add("ours-line", &|t| t.set_paragraph_background(Some("#e6ffec")));
    add("theirs-line", &|t| t.set_paragraph_background(Some("#ddf4ff")));
    add("base-line", &|t| t.set_paragraph_background(Some("#fff8c5")));
    add("conflict-marker", &|t| {
        t.set_paragraph_background(Some("#ffd7d5"));
        t.set_foreground(Some("#cf222e"));
        t.set_weight(700);
    });
    // Inline banner buttons (the conflict "use …" cues and the diff "expand
    // context" cues). Each is an inverse of its host line: a solid body filled in
    // the line's accent colour with the line's background colour as text, end-
    // capped by full-height triangles drawn in the body colour on the bare line
    // background so the ends point outward and stay flush. Added last so the
    // body's text colour outranks the host line's own foreground (GTK tag
    // priority follows tag-table insertion order).
    add("resolve-cue", &|t| {
        t.set_background(Some("#cf222e"));
        t.set_foreground(Some("#ffd7d5"));
        t.set_weight(700);
    });
    add("resolve-cue-cap", &|t| {
        t.set_foreground(Some("#cf222e"));
        t.set_weight(700);
    });
    add("expand-cue", &|t| {
        t.set_background(Some("#0550ae"));
        t.set_foreground(Some("#ddf4ff"));
        t.set_weight(700);
    });
    add("expand-cue-cap", &|t| {
        t.set_foreground(Some("#0550ae"));
        t.set_weight(700);
    });
    // The "revert hunk"/"revert file" cues — a destructive action, so the warning
    // amber palette sets them apart from the blue expand cue sharing the line.
    add("revert-cue", &|t| {
        t.set_background(Some("#9a6700"));
        t.set_foreground(Some("#fff8c5"));
        t.set_weight(700);
    });
    add("revert-cue-cap", &|t| {
        t.set_foreground(Some("#9a6700"));
        t.set_weight(700);
    });
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
pub(crate) fn highlight_diff(buffer: &sourceview5::Buffer, path: Option<&str>, ps: &SyntaxSet, theme: &Theme) {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    let text = buffer.text(&start, &end, false).to_string();
    buffer.remove_all_tags(&start, &end);

    let raw_lines: Vec<&str> = text.split('\n').collect();
    let parsed = parse_diff_lines(&text);

    // Pick a syntect syntax from a file extension, falling back to plain text.
    let pick = |p: &str| {
        std::path::Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| ps.find_syntax_by_extension(ext))
            .unwrap_or_else(|| ps.find_syntax_plain_text())
    };
    // The combined buffer holds several files; `path` is only the fallback. The
    // per-section language is re-derived from each `--- a/PATH` header below.
    let mut syntax = path.map(pick).unwrap_or_else(|| ps.find_syntax_plain_text());
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
                // Paint the trailing "expand context" / "revert hunk" cues. A hunk
                // header can carry both, so paint each pill by its label.
                paint_diff_pills(buffer, li as i32, raw);
                continue;
            }
            DiffLineKind::Header => {
                // A new file section starts at `--- a/PATH`: switch language and
                // reset the parser state so the previous file doesn't bleed in.
                if let Some(p) = raw.strip_prefix("--- a/") {
                    syntax = pick(p);
                    old_hl = HighlightLines::new(syntax, theme);
                    new_hl = HighlightLines::new(syntax, theme);
                }
                continue;
            }
            DiffLineKind::Meta => {
                // The `diff --git` separator carries the "revert file" cue.
                if raw.starts_with("diff --git ") {
                    paint_diff_pills(buffer, li as i32, raw);
                }
                continue;
            }
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

/// Highlight a *conflicted* file (whole-file content with 2-way markers) in
/// `buffer`: a colored background per region (ours/theirs/base), the marker lines
/// emphasized, and syntect language coloring of the code, with the parser state
/// reset at each marker so the discontiguous regions don't bleed into each other.
pub(crate) fn highlight_conflict(
    buffer: &sourceview5::Buffer,
    path: Option<&str>,
    ps: &SyntaxSet,
    theme: &Theme,
) {
    let start = buffer.start_iter();
    let end = buffer.end_iter();
    let text = buffer.text(&start, &end, false).to_string();
    buffer.remove_all_tags(&start, &end);

    let raw_lines: Vec<&str> = text.split('\n').collect();
    let kinds = classify_conflict_lines(&text);
    let cue = pill(CONFLICT_CUE_LABEL);

    let pick = |p: &str| {
        std::path::Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| ps.find_syntax_by_extension(ext))
            .unwrap_or_else(|| ps.find_syntax_plain_text())
    };
    // `path` is only the fallback; the combined buffer holds several files and the
    // per-section language is re-derived from each `─── PATH ───` header.
    let mut syntax = path.map(pick).unwrap_or_else(|| ps.find_syntax_plain_text());
    let mut hl = HighlightLines::new(syntax, theme);

    for (li, &kind) in kinds.iter().enumerate() {
        let raw = raw_lines.get(li).copied().unwrap_or("");
        // A file header starts a new section: switch language, reset state, paint
        // it as a header, and skip the content coloring.
        if let Some(p) = conflict_header_path(raw) {
            syntax = pick(p);
            hl = HighlightLines::new(syntax, theme);
            apply_line_tag(buffer, li as i32, "hunk");
            continue;
        }
        // The elision cue is a pill button standing in for a hidden run.
        if raw == cue {
            apply_line_tag(buffer, li as i32, "hunk");
            paint_pill(buffer, li as i32, raw, "expand-cue-cap", "expand-cue");
            continue;
        }
        if raw == CONFLICT_STRUCTURAL_NOTICE {
            apply_line_tag(buffer, li as i32, "meta");
            continue;
        }
        if let Some(name) = conflict_bg_tag(kind) {
            apply_line_tag(buffer, li as i32, name);
        }
        if kind.is_marker() {
            // A marker line is structural; reset the syntax parser so the next
            // region starts clean, and don't language-color the marker itself.
            hl = HighlightLines::new(syntax, theme);
            // Paint the trailing "use ours/theirs/both" cue as a pill button.
            paint_pill(buffer, li as i32, raw, "resolve-cue-cap", "resolve-cue");
            continue;
        }
        // Unlike a unified diff, conflict lines carry no prefix char — column 0
        // is real content.
        let owned = format!("{raw}\n");
        if let Ok(spans) = hl.highlight_line(&owned, ps) {
            let mut byte = 0usize;
            for (style, piece) in spans {
                if byte >= raw.len() {
                    break;
                }
                let plen = piece.len().min(raw.len() - byte);
                if plen > 0 {
                    let cs = raw[..byte].chars().count();
                    let ce = raw[..byte + plen].chars().count();
                    let fg = style.foreground;
                    let hex = format!("#{:02x}{:02x}{:02x}", fg.r, fg.g, fg.b);
                    apply_cols(buffer, li as i32, cs as i32, ce as i32, &fg_tag(buffer, &hex));
                }
                byte += plen;
            }
        }
    }
}

/// The line-background tag name for a conflict line kind (`None` = plain content).
fn conflict_bg_tag(kind: ConflictLineKind) -> Option<&'static str> {
    match kind {
        ConflictLineKind::Ours => Some("ours-line"),
        ConflictLineKind::Theirs => Some("theirs-line"),
        ConflictLineKind::Base => Some("base-line"),
        ConflictLineKind::MarkerOurs
        | ConflictLineKind::MarkerBase
        | ConflictLineKind::MarkerSep
        | ConflictLineKind::MarkerTheirs => Some("conflict-marker"),
        ConflictLineKind::Plain => None,
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
