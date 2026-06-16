//! Diff/conflict syntax highlighting: the static TextTag palette and syntect
//! language colouring of diff and conflict text (with parser state reset at each
//! region boundary). The diff/conflict *affordances* are gutter buttons now
//! (`diff_cues`), so no inline "pill" painting lives here any more.

use commedit_engine::diff::{
    classify_conflict_lines, parse_diff_lines, ConflictLineKind, DiffLineKind,
};
use gtk::prelude::*;
use gtk::TextTag;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, Theme};
use syntect::parsing::SyntaxSet;

use crate::conflict::conflict_header_path;
use crate::state::{CONFLICT_ELISION_LINE, CONFLICT_STRUCTURAL_NOTICE};

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
    // Trailing whitespace on added lines — a saturated red so the otherwise
    // invisible space/tab run reads as a block to fix. Added after the word
    // backgrounds so its character background outranks them (GTK tag priority
    // follows tag-table insertion order).
    add("trailing-ws", &|t| t.set_background(Some("#ff6b6b")));
    // Conflict-resolution pane: "our" side, "their" side, and the marker lines.
    add("ours-line", &|t| {
        t.set_paragraph_background(Some("#e6ffec"))
    });
    add("theirs-line", &|t| {
        t.set_paragraph_background(Some("#ddf4ff"))
    });
    add("base-line", &|t| {
        t.set_paragraph_background(Some("#fff8c5"))
    });
    add("conflict-marker", &|t| {
        t.set_paragraph_background(Some("#ffd7d5"));
        t.set_foreground(Some("#cf222e"));
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
pub(crate) fn highlight_diff(
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
    let mut syntax = path
        .map(pick)
        .unwrap_or_else(|| ps.find_syntax_plain_text());
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
                // Expand / revert actions for this hunk live in the gutter (see
                // `diff_cues`), so the `@@` header itself needs no inline painting.
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
            DiffLineKind::Meta => continue,
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
            apply_code_spans(buffer, li as i32, prefix, code, &spans);
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

        if line.kind == DiffLineKind::Added {
            apply_trailing_ws(buffer, li as i32, prefix, code);
        }
    }
}

/// Re-highlight a single diff line in place — its background, a fresh
/// single-line syntect pass, and the trailing-whitespace flag — leaving the rest
/// of the buffer's tags untouched. This is the instant-feedback path for an
/// in-place edit to an existing line (the `EditPlan::Allow` keystroke the
/// firewall lets through, which otherwise only repaints via the debounced full
/// [`highlight_diff`] and so trails the typing). It deliberately skips the
/// removed/added intra-line word diff (needs the paired line) and uses fresh
/// per-line parser state (no multi-line constructs); the debounced full pass
/// then corrects both. `path` only picks the syntect language — the viewport's
/// file, close enough for one line.
pub(crate) fn highlight_diff_line(
    buffer: &sourceview5::Buffer,
    li: i32,
    path: Option<&str>,
    ps: &SyntaxSet,
    theme: &Theme,
) {
    let Some(start) = buffer.iter_at_line(li) else {
        return;
    };
    let end = buffer
        .iter_at_line(li + 1)
        .unwrap_or_else(|| buffer.end_iter());
    let line = buffer.text(&start, &end, false).to_string();
    let raw = line.strip_suffix('\n').unwrap_or(&line);
    buffer.remove_all_tags(&start, &end);

    let kind = parse_diff_lines(raw)
        .first()
        .map_or(DiffLineKind::Context, |l| l.kind);
    if let Some(name) = line_bg_tag(kind) {
        apply_line_tag(buffer, li, name);
    }
    match kind {
        DiffLineKind::Hunk | DiffLineKind::Meta | DiffLineKind::Header => return,
        _ => {}
    }

    let syntax = path
        .and_then(|p| {
            std::path::Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(|ext| ps.find_syntax_by_extension(ext))
        })
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let prefix = if raw.is_empty() { 0 } else { 1 };
    let code = &raw[prefix..];
    let owned = format!("{code}\n");
    if let Ok(spans) = HighlightLines::new(syntax, theme).highlight_line(&owned, ps) {
        apply_code_spans(buffer, li, prefix, code, &spans);
    }
    if kind == DiffLineKind::Added {
        apply_trailing_ws(buffer, li, prefix, code);
    }
}

/// Paint a `highlight_line` result as syntect foreground-color tags over the
/// code portion (`code`, past the `prefix`-wide diff marker) of buffer line `li`.
/// Shared by the full and single-line diff highlighters.
fn apply_code_spans(
    buffer: &sourceview5::Buffer,
    li: i32,
    prefix: usize,
    code: &str,
    spans: &[(Style, &str)],
) {
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
            apply_cols(buffer, li, cs as i32, ce as i32, &fg_tag(buffer, &hex));
        }
        byte += plen;
    }
}

/// Flag the trailing space/tab run on an added line's `code` (past the
/// `prefix`-wide marker), if any — like `git diff --check`, only `+` lines (the
/// content actually written) get this, so the invisible characters surface.
fn apply_trailing_ws(buffer: &sourceview5::Buffer, li: i32, prefix: usize, code: &str) {
    let trimmed = code.trim_end_matches([' ', '\t']);
    if trimmed.len() < code.len() {
        if let Some(tag) = buffer.tag_table().lookup("trailing-ws") {
            let cs = prefix + trimmed.chars().count();
            let ce = prefix + code.chars().count();
            apply_cols(buffer, li, cs as i32, ce as i32, &tag);
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

    let pick = |p: &str| {
        std::path::Path::new(p)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(|ext| ps.find_syntax_by_extension(ext))
            .unwrap_or_else(|| ps.find_syntax_plain_text())
    };
    // `path` is only the fallback; the combined buffer holds several files and the
    // per-section language is re-derived from each `─── PATH ───` header.
    let mut syntax = path
        .map(pick)
        .unwrap_or_else(|| ps.find_syntax_plain_text());
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
        // The elision placeholder stands in for a hidden run; the gutter draws its
        // `↕` expand button. Paint it faint so it reads as a fold, not content.
        if raw == CONFLICT_ELISION_LINE {
            apply_line_tag(buffer, li as i32, "meta");
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
            // region starts clean, and don't language-color the marker itself. The
            // resolve action is a gutter button (`conflict_cue_cells`), so the
            // marker line stays a plain git-style marker.
            hl = HighlightLines::new(syntax, theme);
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
                    apply_cols(
                        buffer,
                        li as i32,
                        cs as i32,
                        ce as i32,
                        &fg_tag(buffer, &hex),
                    );
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
    let e = buffer
        .iter_at_line(li + 1)
        .unwrap_or_else(|| buffer.end_iter());
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
