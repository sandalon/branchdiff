use crate::store::{Comment, Side};
use std::collections::BTreeMap;

/// Enough to identify a line. A minified line could otherwise fill the prompt,
/// and the agent can read the rest from the file.
const MAX_QUOTED_CHARS: usize = 200;

const INTRO: &str = "Please address these review comments. Each gives a line number in the current file and quotes that line; if they disagree, trust the quote. A line marked (removed) is no longer in the file, and its number is from before this branch's changes.";

pub fn render(branch: &str, base: &str, comments: &[Comment]) -> String {
    if comments.is_empty() {
        return String::new();
    }
    let mut by_file: BTreeMap<&str, Vec<&Comment>> = BTreeMap::new();
    for c in comments {
        by_file.entry(&c.path).or_default().push(c);
    }
    let mut out = format!("## Review of {} against {}\n\n{INTRO}\n", code_span(branch), code_span(base));
    for (path, mut items) in by_file {
        // Removed before added, as in the diff. A removed line can share its
        // number with an added one, and these are stored in the order written.
        items.sort_by_key(|c| (c.line, c.side == Side::New));
        out.push_str(&format!("\n### {}\n", code_span(path)));
        for c in items {
            let removed = if c.side == Side::Old { " (removed)" } else { "" };
            out.push_str(&format!("\n- Line {}{removed}: {}\n  {}\n", c.line, quote(&c.line_text), c.body));
        }
    }
    out
}

fn quote(line: &str) -> String {
    let line = line.trim();
    if line.is_empty() {
        return "(blank line)".into();
    }
    match line.char_indices().nth(MAX_QUOTED_CHARS) {
        Some((end, _)) => code_span(&format!("{}…", &line[..end])),
        None => code_span(line),
    }
}

/// A code span ends at the first backtick run as long as its delimiter, so the
/// delimiter must be longer than any run inside. Markdown strips one space from
/// each end, which is what the padding is for, and would otherwise strip the
/// text's own.
fn code_span(text: &str) -> String {
    let longest_run = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let ticks = "`".repeat(longest_run + 1);
    let spaced = text.starts_with(' ') && text.ends_with(' ');
    let pad = if text.starts_with('`') || text.ends_with('`') || spaced { " " } else { "" };
    format!("{ticks}{pad}{text}{pad}{ticks}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(path: &str, line: u32, body: &str) -> Comment {
        Comment { path: path.into(), line, side: Side::New, line_text: "code".into(), body: body.into() }
    }

    #[test]
    fn groups_by_file_in_line_order() {
        let md = render("feat", "develop", &[c("b.rs", 9, "second"), c("a.rs", 7, "late"), c("a.rs", 2, "early")]);
        let (a, b) = (md.find("### `a.rs`").unwrap(), md.find("### `b.rs`").unwrap());
        assert!(a < b);
        assert!(md.find("early").unwrap() < md.find("late").unwrap());
    }

    #[test]
    fn includes_branch_base_line_and_code() {
        let md = render("feat", "develop", &[c("a.rs", 2, "fix this")]);
        assert!(md.contains("`feat`") && md.contains("`develop`"));
        assert!(md.contains("Line 2") && md.contains("fix this") && md.contains("code"));
    }

    #[test]
    fn marks_comments_on_removed_lines() {
        let mut comment = c("a.rs", 2, "why remove this");
        comment.side = Side::Old;
        assert!(render("feat", "develop", &[comment]).contains("Line 2 (removed)"));
    }

    #[test]
    fn empty_comments_render_empty() {
        assert_eq!(render("feat", "develop", &[]), "");
    }

    #[test]
    fn code_containing_backticks_stays_in_one_code_span() {
        let quoted = |text: &str| {
            let mut comment = c("a.rs", 2, "x");
            comment.line_text = text.into();
            render("feat", "develop", &[comment])
        };
        assert!(quoted("let s = `a`;").contains(": ``let s = `a`;``\n"));
        assert!(quoted("a `` b ` c").contains(": ```a `` b ` c```\n"));
        assert!(quoted("`").contains(": `` ` ``\n"));
        assert!(quoted("`a").contains(": `` `a ``\n"));
        assert!(quoted("a`").contains(": `` a` ``\n"));
    }

    #[test]
    fn names_containing_backticks_stay_in_one_code_span() {
        let md = render("fe`at", "dev`elop", &[c("odd`name.rs", 2, "x")]);
        assert!(md.contains("## Review of ``fe`at`` against ``dev`elop``\n"));
        assert!(md.contains("### ``odd`name.rs``\n"));
    }

    #[test]
    fn spaces_around_a_name_survive_markdown_stripping_them() {
        let md = render("feat", "develop", &[c(" a ", 2, "x")]);
        assert!(md.contains("### `  a  `\n"), "{md}");
    }

    #[test]
    fn says_what_the_line_numbers_count() {
        let md = render("feat", "develop", &[c("a.rs", 2, "x")]);
        assert!(md.contains("line number in the current file"));
        assert!(md.contains("from before this branch's changes"));
        assert!(md.contains("if they disagree, trust the quote"));
    }

    #[test]
    fn removed_comments_on_a_line_come_first_and_keep_their_order() {
        let at = |side, body| Comment { side, ..c("a.rs", 4, body) };
        let comments = [at(Side::New, "added 1"), at(Side::Old, "removed 1"), at(Side::New, "added 2"), at(Side::Old, "removed 2")];
        let md = render("feat", "develop", &comments);
        let positions = ["removed 1", "removed 2", "added 1", "added 2"].map(|body| md.find(body).unwrap());
        assert!(positions.is_sorted(), "{md}");
    }

    #[test]
    fn line_order_outranks_side_across_different_lines() {
        let removed = Comment { side: Side::Old, ..c("a.rs", 9, "removed later") };
        let md = render("feat", "develop", &[removed, c("a.rs", 2, "added earlier")]);
        assert!(md.find("added earlier").unwrap() < md.find("removed later").unwrap(), "{md}");
    }

    #[test]
    fn long_lines_are_cut_on_a_character_boundary() {
        let mut comment = c("a.rs", 2, "x");
        comment.line_text = "語".repeat(201);
        let md = render("feat", "develop", &[comment]);
        assert!(md.contains(&format!("`{}…`", "語".repeat(200))), "{md}");
    }

    #[test]
    fn a_line_of_exactly_the_limit_is_quoted_whole() {
        let mut comment = c("a.rs", 2, "x");
        comment.line_text = "x".repeat(200);
        assert!(!render("feat", "develop", &[comment]).contains('…'));
    }

    #[test]
    fn blank_lines_are_named_rather_than_quoted() {
        let mut comment = c("a.rs", 2, "x");
        comment.line_text = " \t\r".into();
        assert!(render("feat", "develop", &[comment]).contains("- Line 2: (blank line)\n"));
    }
}
