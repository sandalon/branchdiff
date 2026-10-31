use crate::diff::FileDiff;
use ratatui::style::Color;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

pub const MAX_LINE_LEN: usize = 2_000;
/// Syntect manages about 1,000 lines a second on real TypeScript, so this
/// bounds how long one highlight can hold a worker.
pub const MAX_FILE_LINES: usize = 5_000;

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
    // Empty, not plain copies: the UI falls back to raw text, and copying every
    // line of a huge file would cost more than it saves.
    if file.hunks.iter().map(|h| h.lines.len()).sum::<usize>() > MAX_FILE_LINES {
        return HighlightedFile::default();
    }
    let (syntaxes, theme) = assets();
    let ext = file.path.rsplit('.').next().unwrap_or("");
    let syntax = syntaxes.find_syntax_by_extension(ext);
    let hunks = file.hunks.iter().map(|hunk| {
        let mut state = syntax.map(|s| HighlightLines::new(s, theme));
        hunk.lines.iter().map(|line| {
            if line.text.len() > MAX_LINE_LEN {
                return plain(&line.text);
            }
            let with_newline = format!("{}\n", line.text);
            let spans = state.as_mut().and_then(|h| {
                h.highlight_line(&with_newline, syntaxes).ok()
            });
            match spans {
                Some(spans) => spans.iter().map(|(style, text)| {
                    let c = style.foreground;
                    (Color::Rgb(c.r, c.g, c.b), text.trim_end_matches('\n').to_string())
                }).filter(|(_, t)| !t.is_empty()).collect(),
                None => plain(&line.text),
            }
        }).collect()
    }).collect();
    HighlightedFile { hunks }
}

fn plain(text: &str) -> Vec<Span> {
    vec![(Color::Reset, text.to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{Hunk, Line, LineKind, Status};

    fn file(path: &str, text: &str) -> FileDiff {
        FileDiff {
            path: path.into(), old_path: None, status: Status::Added, binary: false, content_id: None, added: 1, removed: 0,
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

    #[test]
    fn oversized_files_are_not_highlighted() {
        let long = "x".repeat(MAX_LINE_LEN + 1);
        let out = highlight_file(&file("a.rs", &long));
        assert_eq!(out.hunks[0][0].len(), 1);
    }

    #[test]
    fn colours_typescript_tsx_and_toml() {
        for (path, text) in [("a.ts", "const x: number = 1;"), ("a.tsx", "const a = <div>hi</div>;"), ("a.toml", "a = \"b\"")] {
            assert!(highlight_file(&file(path, text)).hunks[0][0].len() > 1, "{path}");
        }
    }
}
