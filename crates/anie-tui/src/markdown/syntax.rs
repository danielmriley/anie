//! Code-block syntax highlighting via `syntect`.
//!
//! The syntax + theme sets are loaded lazily on first use so a
//! session that never renders a code block doesn't pay the cost.
//! `highlight_code` returns pre-styled ratatui spans; callers
//! decorate with the surrounding box.
//!
//! Matches pi's shape at a conceptual level — pi exposes
//! `highlightCode?: (code, lang?) => string[]` on the theme
//! (`packages/tui/src/components/markdown.ts:68`). Ours is a
//! free function that the layout layer calls directly; the
//! equivalent indirection would be a theme function pointer,
//! which we can add later if we need per-theme alternate
//! highlighters.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SyntectStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Bundled syntaxes + themes. Loaded once per process.
fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme_set() -> &'static ThemeSet {
    static SET: OnceLock<ThemeSet> = OnceLock::new();
    SET.get_or_init(ThemeSet::load_defaults)
}

/// Preferred theme for anie's dark TUI palette. Returns `None`
/// only if syntect's bundled theme set is somehow empty — in
/// practice it always ships `base16-eighties.dark` and several
/// others, but we treat the lookup as fallible so we can emit
/// plain-text code on exotic builds instead of panicking.
fn default_dark_theme() -> Option<&'static Theme> {
    let themes = theme_set();
    themes
        .themes
        .get("base16-eighties.dark")
        .or_else(|| themes.themes.get("Solarized (dark)"))
        .or_else(|| themes.themes.values().next())
}

/// Highlight `code` in `lang`, returning one ratatui `Line` per
/// source line. Unknown or missing `lang` falls back to a plain-
/// text render so the renderer degrades gracefully rather than
/// skipping the code block.
#[must_use]
pub fn highlight_code(code: &str, lang: Option<&str>) -> Vec<Line<'static>> {
    let syntaxes = syntax_set();
    let Some(theme) = default_dark_theme() else {
        return plain_text_lines(code);
    };

    let syntax = lang
        .and_then(|name| resolve_syntax(name, syntaxes))
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());

    let mut highlighter = HighlightLines::new(syntax, theme);

    let mut out: Vec<Line<'static>> = Vec::new();
    for raw_line in LinesWithEndings::from(code) {
        match highlighter.highlight_line(raw_line, syntaxes) {
            Ok(ranges) => out.push(syntect_ranges_to_line(&ranges)),
            Err(_) => {
                // Syntect errors are rare and indicate a malformed
                // grammar + input; fall back to plain text for
                // this line instead of aborting the whole block.
                out.push(Line::from(Span::raw(
                    raw_line.trim_end_matches('\n').to_string(),
                )));
            }
        }
    }
    // LinesWithEndings preserves newlines inside the token strings
    // — strip them from the last span so ratatui doesn't emit a
    // literal `\n` on screen.
    for line in &mut out {
        if let Some(last) = line.spans.last_mut() {
            let trimmed = last.content.trim_end_matches('\n').to_string();
            last.content = trimmed.into();
        }
    }
    out
}

/// Try several common aliases when looking up `lang`. syntect
/// keys syntaxes by *token* (`"rust"`, `"python"`) or by
/// *extension* (`"rs"`, `"py"`); LLM-emitted code fences can use
/// either convention, plus shorter aliases like `"sh"`.
fn resolve_syntax<'a>(
    lang: &str,
    set: &'a SyntaxSet,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    let name = lang.trim().to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let candidates: &[&str] = match name.as_str() {
        "rs" | "rust" => &["Rust"],
        "js" | "javascript" | "node" => &["JavaScript"],
        "ts" | "typescript" => &["TypeScript"],
        "tsx" => &["TypeScriptReact", "TypeScript"],
        "jsx" => &["JavaScriptReact", "JavaScript"],
        "py" | "python" => &["Python"],
        "sh" | "bash" | "shell" | "zsh" => &["Bash", "Shell-Unix-Generic"],
        "json" | "jsonl" => &["JSON"],
        "yaml" | "yml" => &["YAML"],
        "toml" => &["TOML"],
        "md" | "markdown" => &["Markdown"],
        "html" => &["HTML"],
        "css" => &["CSS"],
        "sql" => &["SQL"],
        "go" | "golang" => &["Go"],
        "c" => &["C"],
        "cpp" | "cxx" | "c++" => &["C++"],
        "java" => &["Java"],
        "ruby" | "rb" => &["Ruby"],
        _ => &[],
    };
    for candidate in candidates {
        if let Some(syntax) = set.find_syntax_by_name(candidate) {
            return Some(syntax);
        }
    }
    // Last resort: syntect's own token-by-extension lookup.
    set.find_syntax_by_token(&name)
        .or_else(|| set.find_syntax_by_extension(&name))
}

/// Plain-text fallback shaped like `highlight_code`'s output.
/// One ratatui `Line` per source line with no style applied.
fn plain_text_lines(code: &str) -> Vec<Line<'static>> {
    LinesWithEndings::from(code)
        .map(|line| Line::from(Span::raw(line.trim_end_matches('\n').to_string())))
        .collect()
}

fn syntect_ranges_to_line(ranges: &[(SyntectStyle, &str)]) -> Line<'static> {
    let spans = ranges
        .iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(style, text)| Span::styled(text.to_string(), convert_style(*style)))
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn convert_style(style: SyntectStyle) -> Style {
    let mut out = Style::default().fg(to_ratatui_color(style.foreground));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

fn to_ratatui_color(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_fallback_for_unknown_language() {
        let lines = highlight_code("arbitrary text\nsecond line", Some("nonesuch"));
        assert_eq!(lines.len(), 2);
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(joined.contains("arbitrary text"));
        assert!(joined.contains("second line"));
    }

    #[test]
    fn plain_text_fallback_for_missing_language() {
        let lines = highlight_code("just text", None);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn rust_keyword_gets_styled() {
        let lines = highlight_code("fn main() {}", Some("rust"));
        // `fn` should be a keyword → it gets a non-default fg.
        // We don't assert the exact colour because themes vary,
        // only that *some* span has a non-default colour.
        let styled = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|span| !matches!(span.style.fg, None | Some(Color::Reset)));
        assert!(styled, "expected at least one styled span: {lines:?}");
    }

    #[test]
    fn empty_code_yields_no_lines() {
        let lines = highlight_code("", Some("rust"));
        assert!(lines.is_empty());
    }

    #[test]
    fn each_source_line_becomes_one_ratatui_line() {
        let lines = highlight_code("let a = 1;\nlet b = 2;\nlet c = 3;\n", Some("rust"));
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn alias_py_resolves_to_python() {
        let set = syntax_set();
        assert!(resolve_syntax("py", set).is_some());
        assert!(resolve_syntax("python", set).is_some());
    }

    #[test]
    fn alias_ts_resolves_to_typescript() {
        let set = syntax_set();
        // syntect's built-in set may not include TypeScript on
        // default features; accept None gracefully and just
        // verify the alias doesn't panic.
        let _ = resolve_syntax("ts", set);
    }

    #[test]
    fn newlines_are_stripped_from_emitted_spans() {
        let lines = highlight_code("fn a() {}\nfn b() {}\n", Some("rust"));
        for line in lines {
            for span in line.spans {
                assert!(
                    !span.content.ends_with('\n'),
                    "span ends with \\n: {:?}",
                    span.content
                );
            }
        }
    }
}
