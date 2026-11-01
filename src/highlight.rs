use crate::diff::{FileDiff, LineKind};
use ratatui::style::Color;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

pub const MAX_LINE_LEN: usize = 2_000;
pub const MAX_FILE_LINES: usize = 5_000;
/// Bounds how long one highlight can hold a worker, which nothing can interrupt
/// once it runs. The line limits don't: real source can take several seconds
/// under load, and minified code manages under 100 lines a second.
const MAX_HIGHLIGHT_TIME: Duration = Duration::from_secs(5);

pub type Span = (Color, String);

#[derive(Debug, Clone, Default)]
pub struct HighlightedFile {
    /// Spans for each line of each hunk, parallel to `FileDiff::hunks`.
    pub hunks: Vec<Vec<Vec<Span>>>,
}

fn assets() -> &'static (SyntaxSet, Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults();
        let theme = themes.themes.remove("base16-ocean.dark").expect("bundled theme");
        (two_face::syntax::extra_newlines(), theme)
    })
}

pub fn highlight_file(file: &FileDiff) -> HighlightedFile {
    highlight_within(file, MAX_HIGHLIGHT_TIME)
}

fn highlight_within(file: &FileDiff, budget: Duration) -> HighlightedFile {
    // Empty, not plain copies: the UI falls back to raw text, and copying every
    // line of a huge file would cost more than it saves.
    if file.hunks.iter().map(|h| h.lines.len()).sum::<usize>() > MAX_FILE_LINES {
        return HighlightedFile::default();
    }
    let started = Instant::now();
    let (syntaxes, theme) = assets();
    // Names like `Makefile` and `.bashrc` are listed whole, and a directory
    // must not count towards an extension.
    let name = file.path.rsplit('/').next().unwrap_or("");
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    // The bundled set lists none of these, and TSX is a superset of JSX.
    let ext = match ext.as_str() {
        "jsx" => "tsx",
        "mjs" | "cjs" => "js",
        other => other,
    };
    let syntax = syntaxes.find_syntax_by_extension(name).or_else(|| syntaxes.find_syntax_by_extension(ext));
    let hunks = file
        .hunks
        .iter()
        .map(|hunk| {
            // Each side is its own text, so a removed `/*` must not colour the lines
            // after it. Context is in both and shows as the new side has it.
            let mut old = syntax.map(|s| HighlightLines::new(s, theme));
            let mut new = syntax.map(|s| HighlightLines::new(s, theme));
            hunk.lines
                .iter()
                .map(|line| {
                    (started.elapsed() < budget).then(|| match line.kind {
                        LineKind::Removed => colour(&mut old, &line.text),
                        LineKind::Added => colour(&mut new, &line.text),
                        LineKind::Context => {
                            colour(&mut old, &line.text);
                            colour(&mut new, &line.text)
                        }
                    })
                })
                .collect::<Option<_>>()
        })
        .collect::<Option<_>>();
    hunks.map(|hunks| HighlightedFile { hunks }).unwrap_or_default()
}

fn colour(state: &mut Option<HighlightLines>, text: &str) -> Vec<Span> {
    if text.len() > MAX_LINE_LEN {
        return plain(text);
    }
    let (syntaxes, theme) = assets();
    let with_newline = format!("{text}\n");
    let spans = state.as_mut().and_then(|h| h.highlight_line(&with_newline, syntaxes).ok());
    match spans {
        Some(spans) => spans
            .iter()
            .map(|(style, text)| {
                // Only foregrounds are drawn, and an `invalid` token's is the theme's
                // background, meant to sit on red. In a hunk cut from the middle of
                // a file, such a token is usually just an unmatched closing brace.
                let c = if Some(style.foreground) == theme.settings.background {
                    theme.settings.foreground.unwrap_or(style.foreground)
                } else {
                    style.foreground
                };
                (Color::Rgb(c.r, c.g, c.b), text.trim_end_matches('\n').to_string())
            })
            .filter(|(_, t)| !t.is_empty())
            .collect(),
        None => plain(text),
    }
}

fn plain(text: &str) -> Vec<Span> {
    vec![(Color::Reset, text.to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Hunk, Line, Status};

    fn file(path: &str, text: &str) -> FileDiff {
        FileDiff {
            path: path.into(),
            old_path: None,
            status: Status::Added,
            binary: false,
            content_id: None,
            added: 1,
            removed: 0,
            hunks: vec![Hunk {
                header: "@@ -0,0 +1 @@".into(),
                lines: vec![Line { kind: LineKind::Added, old_no: None, new_no: Some(1), text: text.into() }],
            }],
        }
    }

    #[test]
    fn colours_known_languages_and_keeps_the_text() {
        let out = highlight_file(&file("a.rs", "fn main() {}"));
        let line = &out.hunks[0][0];
        assert!(line.len() > 1);
        assert_eq!(line.iter().map(|(_, s)| s.as_str()).collect::<String>(), "fn main() {}");
    }

    #[test]
    fn unknown_extension_yields_one_plain_span() {
        let out = highlight_file(&file("a.unknownext", "text"));
        assert_eq!(out.hunks[0][0].len(), 1);
    }

    #[test]
    fn files_over_the_line_limit_stay_plain() {
        let mut big = file("a.rs", "fn main() {}");
        let line = big.hunks[0].lines[0].clone();
        big.hunks[0].lines = vec![line; MAX_FILE_LINES + 1];
        assert!(highlight_file(&big).hunks.is_empty());
    }

    /// Real tokens, so that highlighting this line gives many spans.
    fn code_of_len(len: usize) -> String {
        let mut code = "let x = 1; ".repeat(len / 11 + 1);
        code.truncate(len);
        code
    }

    #[test]
    fn a_line_over_the_length_limit_stays_plain() {
        let code = code_of_len(MAX_LINE_LEN + 1);
        let out = highlight_file(&file("a.rs", &code));
        assert_eq!(out.hunks[0][0], vec![(Color::Reset, code)]);
    }

    #[test]
    fn a_line_at_the_length_limit_is_highlighted() {
        let out = highlight_file(&file("a.rs", &code_of_len(MAX_LINE_LEN)));
        assert!(out.hunks[0][0].len() > 100);
    }

    #[test]
    fn finds_the_language_from_a_whole_file_name_in_any_directory() {
        let files = [
            ("src/Makefile", "a: b"),
            ("docker/Dockerfile", "FROM rust"),
            (".bashrc", "export A=1"),
            ("home/.bashrc", "export A=1"),
        ];
        for (path, text) in files {
            assert!(highlight_file(&file(path, text)).hunks[0][0].len() > 1, "{path}");
        }
    }

    fn lines(path: &str, lines: &[(LineKind, &str)]) -> FileDiff {
        let lines =
            lines.iter().map(|&(kind, text)| Line { kind, old_no: None, new_no: None, text: text.into() }).collect();
        FileDiff { hunks: vec![Hunk { header: "@@".into(), lines }], ..file(path, "") }
    }

    #[test]
    fn a_removed_comment_opener_does_not_colour_the_lines_after_it() {
        let diff =
            lines("a.c", &[(LineKind::Removed, "/* old"), (LineKind::Added, "int b;"), (LineKind::Context, "int c;")]);
        let out = highlight_file(&diff);
        assert!(out.hunks[0][1].len() > 1);
        assert!(out.hunks[0][2].len() > 1);
    }

    #[test]
    fn an_added_comment_opener_colours_the_context_after_it() {
        let diff = lines("a.c", &[(LineKind::Added, "/* new"), (LineKind::Context, "int c;")]);
        assert_eq!(highlight_file(&diff).hunks[0][1].len(), 1);
    }

    #[test]
    fn a_context_comment_opener_colours_the_removed_lines_after_it() {
        let diff = lines("a.c", &[(LineKind::Context, "/* open"), (LineKind::Removed, "int c;")]);
        let out = highlight_file(&diff);
        assert_eq!(out.hunks[0][1].len(), 1);
        assert_eq!(out.hunks[0][1][0].0, out.hunks[0][0][0].0);
    }

    /// A hunk can open after the `{` that a closing brace belongs to.
    #[test]
    fn a_token_the_grammar_calls_invalid_stays_visible() {
        let out = highlight_file(&file("a.c", "}"));
        let background = assets().1.settings.background.unwrap();
        let hidden = Color::Rgb(background.r, background.g, background.b);
        assert_ne!(out.hunks[0][0][0].0, hidden);
    }

    #[test]
    fn a_file_over_the_time_budget_stays_plain() {
        let out = highlight_within(&file("a.rs", "fn main() {}"), Duration::ZERO);
        assert!(out.hunks.is_empty());
    }

    #[test]
    fn colours_jsx_with_the_tsx_syntax() {
        let text = "const a = <div className=\"b\">hi</div>;";
        let jsx = highlight_file(&file("src/a.jsx", text));
        assert!(jsx.hunks[0][0].len() > 1);
        assert_eq!(jsx.hunks[0][0], highlight_file(&file("src/a.tsx", text)).hunks[0][0]);
    }

    #[test]
    fn colours_mjs_and_cjs_with_the_javascript_syntax() {
        let text = "const a = 1;";
        let js = highlight_file(&file("src/a.js", text));
        assert!(js.hunks[0][0].len() > 1);
        for path in ["src/a.mjs", "src/a.cjs"] {
            assert_eq!(highlight_file(&file(path, text)).hunks[0][0], js.hunks[0][0], "{path}");
        }
    }

    #[test]
    fn colours_typescript_tsx_and_toml() {
        for (path, text) in
            [("a.ts", "const x: number = 1;"), ("a.tsx", "const a = <div>hi</div>;"), ("a.toml", "a = \"b\"")]
        {
            assert!(highlight_file(&file(path, text)).hunks[0][0].len() > 1, "{path}");
        }
    }
}
