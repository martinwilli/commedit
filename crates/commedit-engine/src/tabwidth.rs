//! Resolve a file's display tab width from the repository's editor-config files.
//!
//! commedit's diff pane is a single text view that can only render one tab width
//! at a time, so the width is resolved *per file* as the user navigates (the file
//! at the top of the diff). This module reads the conventional config files a
//! repo uses to declare its indentation and returns the width to draw a tab
//! character at. It is GTK-free and unit-testable; the GTK side just calls
//! [`TabWidthResolver::tab_width`] and feeds the result to `set_tab_width`.
//!
//! Sources, in resolution order — the first that yields a value for the file
//! wins, so the more file-/language-specific config beats the global default:
//!
//!  1. `.editorconfig` — the editor-agnostic standard. Glob-matched per file and
//!     cascaded up parent directories (via the `ec4rs` crate); reads `tab_width`,
//!     falling back to a numeric `indent_size` per the EditorConfig spec.
//!  2. `.vscode/settings.json` *language-specific* — a `"[langId]": { ... }` block
//!     whose `editor.tabSize` applies to the file's language (matched by
//!     extension).
//!  3. `.clang-format` — `TabWidth` (or, absent that, `IndentWidth`), applied to
//!     the C family it governs (C / C++ / Objective-C / CUDA).
//!  4. `.vscode/settings.json` *global* — the top-level `editor.tabSize`.
//!
//! `.vscode/settings.json` and `.clang-format` are read once from the repository
//! root (the common single-config layout); `.editorconfig` is resolved per path
//! so its directory cascade and globs apply. Resolved widths are cached by path,
//! since the same files recur as the user scrolls the diff and switches commits.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The display tab width used when no config file specifies one — git's and
/// GtkSourceView's conventional default.
pub const DEFAULT_TAB_WIDTH: u32 = 8;

/// Resolved widths are clamped to this range so a typo in a config file can't
/// leave the diff view unusable.
const MIN_TAB_WIDTH: u32 = 1;
const MAX_TAB_WIDTH: u32 = 16;

/// Resolves the display tab width for a repo-relative file path from the repo's
/// editor-config files. Built once per session; see the module docs for the
/// sources and their precedence.
pub struct TabWidthResolver {
    /// The repository root that relative paths are joined onto.
    root: PathBuf,
    /// `.clang-format`'s `TabWidth`/`IndentWidth` (repo root), if present.
    clang_tab_width: Option<u32>,
    /// `.vscode/settings.json`'s global `editor.tabSize` (repo root), if present.
    vscode_global: Option<u32>,
    /// `.vscode/settings.json`'s per-language-id `editor.tabSize` overrides.
    vscode_by_lang: HashMap<String, u32>,
    /// Per-path resolution cache (paths recur across navigation / commits).
    cache: RefCell<HashMap<String, Option<u32>>>,
}

impl TabWidthResolver {
    /// Read the repo-root `.clang-format` and `.vscode/settings.json` once.
    /// `.editorconfig` is read lazily per path in [`Self::tab_width`].
    pub fn new(root: &Path) -> Self {
        let clang_tab_width = std::fs::read_to_string(root.join(".clang-format"))
            .ok()
            .and_then(|t| parse_clang_format_tab_width(&t));
        let (vscode_global, vscode_by_lang) =
            std::fs::read_to_string(root.join(".vscode/settings.json"))
                .ok()
                .map(|t| parse_vscode_settings(&t))
                .unwrap_or_default();
        Self {
            root: root.to_path_buf(),
            clang_tab_width,
            vscode_global,
            vscode_by_lang,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// The display tab width configured for `relative_path` (repo-relative), or
    /// `None` if no source configures one — the caller falls back to
    /// [`DEFAULT_TAB_WIDTH`]. Cached per path.
    pub fn tab_width(&self, relative_path: &str) -> Option<u32> {
        if let Some(cached) = self.cache.borrow().get(relative_path) {
            return *cached;
        }
        let resolved = self.resolve(relative_path).map(clamp_width);
        self.cache
            .borrow_mut()
            .insert(relative_path.to_owned(), resolved);
        resolved
    }

    fn resolve(&self, relative_path: &str) -> Option<u32> {
        // 1. .editorconfig — glob-matched and cascaded over the file's directory
        //    ancestry. ec4rs matches on the path string and never stats the file,
        //    so a path that no longer exists on disk (a deleted/renamed commit
        //    file) still resolves.
        if let Some(w) = editorconfig_tab_width(&self.root.join(relative_path)) {
            return Some(w);
        }
        let ext = Path::new(relative_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let ext = ext.as_deref();
        // 2. .vscode/settings.json — a language-specific block beats everything
        //    below it (mirrors VS Code's own precedence).
        if let Some(lang) = ext.and_then(vscode_lang_id) {
            if let Some(w) = self.vscode_by_lang.get(lang).copied() {
                return Some(w);
            }
        }
        // 3. .clang-format — for the C family it governs.
        if ext.is_some_and(is_c_family) {
            if let Some(w) = self.clang_tab_width {
                return Some(w);
            }
        }
        // 4. .vscode/settings.json — the global editor default.
        self.vscode_global
    }
}

fn clamp_width(w: u32) -> u32 {
    w.clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH)
}

/// Read the effective display tab width from the `.editorconfig` chain governing
/// `abs_path`. After `use_fallbacks`, ec4rs has applied the spec's relationship
/// between `indent_size` and `tab_width` (a numeric `indent_size` seeds an unset
/// `tab_width`), so reading `tab_width` yields the right value whether the file
/// set `tab_width` directly or only `indent_size`.
fn editorconfig_tab_width(abs_path: &Path) -> Option<u32> {
    use ec4rs::property::TabWidth;
    let mut props = ec4rs::properties_of(abs_path).ok()?;
    props.use_fallbacks();
    match props.get::<TabWidth>() {
        Ok(TabWidth::Value(n)) => u32::try_from(n).ok(),
        _ => None,
    }
}

/// Extract the display tab width from a `.clang-format` document. clang-format is
/// YAML, but we need only `TabWidth` (the tab-stop width), falling back to
/// `IndentWidth` — both plain `Key: N` scalars — so we scan for them rather than
/// pull in a YAML parser. The first match wins; a multi-`Language` file (rare) is
/// not resolved per-section.
fn parse_clang_format_tab_width(text: &str) -> Option<u32> {
    let scan = |key: &str| {
        text.lines().find_map(|line| {
            // The key must open the line (after indentation) and be followed by
            // optional spaces then `:`, so `IndentWidth` doesn't match
            // `ContinuationIndentWidth` and `# TabWidth` (a comment) doesn't match.
            let rest = line.trim_start().strip_prefix(key)?.trim_start();
            let rest = rest.strip_prefix(':')?;
            rest.split_whitespace().next()?.parse::<u32>().ok()
        })
    };
    scan("TabWidth").or_else(|| scan("IndentWidth"))
}

/// Parse a `.vscode/settings.json` (JSONC: comments + trailing commas) into its
/// global `editor.tabSize` and the per-language-id `editor.tabSize` overrides
/// found in `"[langId]": { ... }` blocks.
fn parse_vscode_settings(text: &str) -> (Option<u32>, HashMap<String, u32>) {
    use jsonc_parser::{parse_to_value, JsonValue, ParseOptions};

    let Ok(Some(JsonValue::Object(root))) = parse_to_value(text, &ParseOptions::default()) else {
        return (None, HashMap::new());
    };
    let global = root
        .get_number("editor.tabSize")
        .and_then(|n| n.trim().parse::<u32>().ok());
    let mut by_lang = HashMap::new();
    for (key, val) in root.take_inner() {
        let JsonValue::Object(obj) = val else { continue };
        let langs = lang_ids_in_key(&key);
        if langs.is_empty() {
            continue;
        }
        if let Some(size) = obj
            .get_number("editor.tabSize")
            .and_then(|n| n.trim().parse::<u32>().ok())
        {
            for lang in langs {
                by_lang.insert(lang, size);
            }
        }
    }
    (global, by_lang)
}

/// The language id(s) of a VS Code language-specific settings key. A key like
/// `"[rust]"` carries one id; `"[c][cpp]"` carries several (the block applies to
/// each). Non-language keys (e.g. `"editor.tabSize"`) yield an empty list.
fn lang_ids_in_key(key: &str) -> Vec<String> {
    let key = key.trim();
    let Some(inner) = key.strip_prefix('[').and_then(|k| k.strip_suffix(']')) else {
        return Vec::new();
    };
    inner
        .split("][")
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Map a (lowercased) file extension to the VS Code language id used in
/// `"[langId]"` setting blocks. Covers the languages a repo is likely to override
/// `editor.tabSize` for; unmapped extensions fall through to the global setting.
fn vscode_lang_id(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "inl" | "ipp" => "cpp",
        "m" => "objective-c",
        "mm" => "objective-cpp",
        "cu" | "cuh" => "cuda-cpp",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "typescriptreact",
        "java" => "java",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "scala" | "sc" => "scala",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "md" | "markdown" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "sh" | "bash" => "shellscript",
        "lua" => "lua",
        "dart" => "dart",
        "vue" => "vue",
        "sql" => "sql",
        "xml" => "xml",
        _ => return None,
    })
}

/// Whether a (lowercased) file extension is part of the C family clang-format's
/// `TabWidth` governs: C, C++, Objective-C/C++, and CUDA.
fn is_c_family(ext: &str) -> bool {
    matches!(
        ext,
        "c" | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "c++"
            | "cppm"
            | "hpp"
            | "hh"
            | "hxx"
            | "h++"
            | "inl"
            | "ipp"
            | "tpp"
            | "tcc"
            | "m"
            | "mm"
            | "cu"
            | "cuh"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn clang_format_reads_tab_width_then_indent_width() {
        assert_eq!(
            parse_clang_format_tab_width("---\nBasedOnStyle: LLVM\nTabWidth: 4\nUseTab: Always\n"),
            Some(4)
        );
        // TabWidth wins over IndentWidth.
        assert_eq!(
            parse_clang_format_tab_width("IndentWidth: 2\nTabWidth: 8\n"),
            Some(8)
        );
        // Falls back to IndentWidth when TabWidth is absent.
        assert_eq!(parse_clang_format_tab_width("IndentWidth: 2\n"), Some(2));
        // A comment and a longer key that merely contains the name don't match.
        assert_eq!(
            parse_clang_format_tab_width("# TabWidth: 4\nContinuationIndentWidth: 8\n"),
            None
        );
    }

    #[test]
    fn vscode_global_and_language_overrides() {
        let text = r#"{
            // editor settings
            "editor.tabSize": 4,
            "editor.insertSpaces": true,
            "[go]": { "editor.tabSize": 8 },
            "[c][cpp]": {
                "editor.tabSize": 2, // trailing comma below is tolerated
            },
        }"#;
        let (global, by_lang) = parse_vscode_settings(text);
        assert_eq!(global, Some(4));
        assert_eq!(by_lang.get("go"), Some(&8));
        assert_eq!(by_lang.get("c"), Some(&2));
        assert_eq!(by_lang.get("cpp"), Some(&2));
    }

    #[test]
    fn lang_ids_parses_single_and_multi() {
        assert_eq!(lang_ids_in_key("[rust]"), vec!["rust"]);
        assert_eq!(lang_ids_in_key("[c][cpp]"), vec!["c", "cpp"]);
        assert!(lang_ids_in_key("editor.tabSize").is_empty());
        assert!(lang_ids_in_key("[]").is_empty());
    }

    fn write(dir: &TempDir, rel: &str, content: &str) {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn editorconfig_uses_glob_and_indent_size_fallback() {
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            ".editorconfig",
            "root = true\n\n[*]\nindent_size = 4\n\n[Makefile]\nindent_style = tab\ntab_width = 8\n",
        );
        let r = TabWidthResolver::new(dir.path());
        // `[*]` sets indent_size = 4, which seeds tab_width.
        assert_eq!(r.tab_width("src/main.rs"), Some(4));
        // `[Makefile]` overrides with an explicit tab_width.
        assert_eq!(r.tab_width("Makefile"), Some(8));
    }

    #[test]
    fn precedence_editorconfig_over_clang_and_vscode() {
        let dir = TempDir::new().unwrap();
        write(&dir, ".editorconfig", "[*.c]\ntab_width = 2\n");
        write(&dir, ".clang-format", "TabWidth: 8\n");
        write(&dir, ".vscode/settings.json", r#"{ "editor.tabSize": 4 }"#);
        let r = TabWidthResolver::new(dir.path());
        // .editorconfig matches *.c and wins over both other sources.
        assert_eq!(r.tab_width("lib.c"), Some(2));
    }

    #[test]
    fn precedence_clang_over_vscode_global_for_c_family() {
        let dir = TempDir::new().unwrap();
        write(&dir, ".clang-format", "TabWidth: 8\n");
        write(&dir, ".vscode/settings.json", r#"{ "editor.tabSize": 4 }"#);
        let r = TabWidthResolver::new(dir.path());
        // A C file: clang-format (language-specific) beats the vscode global.
        assert_eq!(r.tab_width("lib.c"), Some(8));
        // A non-C file: clang-format doesn't apply, so the vscode global stands.
        assert_eq!(r.tab_width("app.py"), Some(4));
    }

    #[test]
    fn precedence_vscode_language_over_clang() {
        let dir = TempDir::new().unwrap();
        write(&dir, ".clang-format", "TabWidth: 8\n");
        write(
            &dir,
            ".vscode/settings.json",
            r#"{ "editor.tabSize": 4, "[cpp]": { "editor.tabSize": 2 } }"#,
        );
        let r = TabWidthResolver::new(dir.path());
        // A C++ file: the vscode language block beats clang-format.
        assert_eq!(r.tab_width("widget.cpp"), Some(2));
    }

    #[test]
    fn none_when_unconfigured_and_clamped_when_extreme() {
        let dir = TempDir::new().unwrap();
        let r = TabWidthResolver::new(dir.path());
        assert_eq!(r.tab_width("src/main.rs"), None);

        let dir = TempDir::new().unwrap();
        write(&dir, ".editorconfig", "[*]\ntab_width = 999\n");
        let r = TabWidthResolver::new(dir.path());
        assert_eq!(r.tab_width("x.txt"), Some(MAX_TAB_WIDTH));
    }
}
