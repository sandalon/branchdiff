#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status { Modified, Added, Deleted, Renamed, Untracked }

impl Status {
    pub fn letter(self) -> char {
        match self {
            Status::Modified => 'M',
            Status::Added => 'A',
            Status::Deleted => 'D',
            Status::Renamed => 'R',
            Status::Untracked => '?',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind { Context, Added, Removed }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: Status,
    pub binary: bool,
    pub added: u32,
    pub removed: u32,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    fn empty() -> Self {
        Self { path: String::new(), old_path: None, status: Status::Modified, binary: false, added: 0, removed: 0, hunks: Vec::new() }
    }
}

pub fn parse(input: &[u8]) -> Vec<FileDiff> {
    let mut files = Vec::new();
    let mut cur: Option<FileDiff> = None;
    let (mut old_left, mut new_left) = (0u32, 0u32);
    let (mut old_no, mut new_no) = (0u32, 0u32);

    for raw in input.split(|b| *b == b'\n') {
        let line = String::from_utf8_lossy(raw);
        // Inside a hunk the counts decide, so body lines that look like
        // headers are never misread. Every body line starts with one of these
        // bytes, so anything else means the counts were wrong: the hunk is
        // over and the line falls through to the header handling.
        if old_left > 0 || new_left > 0 {
            let body = match line.as_bytes().first() {
                Some(b'+') => Some((LineKind::Added, &line[1..])),
                Some(b'-') => Some((LineKind::Removed, &line[1..])),
                Some(b' ') => Some((LineKind::Context, &line[1..])),
                Some(b'\\') => continue,
                _ => None,
            };
            if let Some((kind, text)) = body {
                let Some(file) = cur.as_mut() else { continue };
                let Some(hunk) = file.hunks.last_mut() else { continue };
                let (o, n) = match kind {
                    LineKind::Added => (None, Some(new_no)),
                    LineKind::Removed => (Some(old_no), None),
                    LineKind::Context => (Some(old_no), Some(new_no)),
                };
                if o.is_some() {
                    old_no = old_no.saturating_add(1);
                    old_left = old_left.saturating_sub(1);
                }
                if n.is_some() {
                    new_no = new_no.saturating_add(1);
                    new_left = new_left.saturating_sub(1);
                }
                match kind {
                    LineKind::Added => file.added = file.added.saturating_add(1),
                    LineKind::Removed => file.removed = file.removed.saturating_add(1),
                    LineKind::Context => {}
                }
                hunk.lines.push(Line { kind, old_no: o, new_no: n, text: text.to_string() });
                continue;
            }
            (old_left, new_left) = (0, 0);
        }

        if line.starts_with("diff --git ") {
            files.extend(cur.take());
            let mut f = FileDiff::empty();
            f.path = path_from_git_header(&line).unwrap_or_default();
            cur = Some(f);
            continue;
        }
        let Some(file) = cur.as_mut() else { continue };
        if line.starts_with("new file mode") {
            file.status = Status::Added;
        } else if line.starts_with("deleted file mode") {
            file.status = Status::Deleted;
        } else if let Some(p) = line.strip_prefix("rename from ") {
            file.status = Status::Renamed;
            file.old_path = Some(unquote(p));
        } else if let Some(p) = line.strip_prefix("rename to ") {
            file.path = unquote(p);
        } else if line.starts_with("Binary files ") {
            file.binary = true;
        } else if let Some(p) = line.strip_prefix("--- ").or_else(|| line.strip_prefix("+++ ")) {
            if let Some(p) = side_path(p) { file.path = p; }
        } else if line.starts_with("@@ ") {
            if let Some((o, ol, n, nl)) = parse_hunk_header(&line) {
                (old_no, old_left, new_no, new_left) = (o, ol, n, nl);
                file.hunks.push(Hunk { header: line.to_string(), lines: Vec::new() });
            }
        }
    }
    files.extend(cur);
    files
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let mut parts = line.split(' ');
    parts.next()?;
    let (o, ol) = parse_range(parts.next()?.strip_prefix('-')?)?;
    let (n, nl) = parse_range(parts.next()?.strip_prefix('+')?)?;
    Some((o, ol, n, nl))
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// The path on a `---` or `+++` line, or `None` for `/dev/null`. Git appends a
/// tab when the name contains a space. A tab in the name itself is quoted, so
/// a bare trailing tab is always that marker.
fn side_path(p: &str) -> Option<String> {
    let p = p.strip_suffix('\t').unwrap_or(p);
    (p != "/dev/null").then(|| strip_side(&unquote(p)))
}

fn strip_side(p: &str) -> String {
    p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_string()
}

/// Only used until `---`, `+++` or `rename to` lines give an exact path, which
/// matters for binary and mode-only changes. Both sides match in those cases,
/// so the split point is the middle of the string.
fn path_from_git_header(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    if rest.starts_with('"') {
        let close = closing_quote(rest)?;
        return Some(strip_side(&unquote(rest.get(close + 2..)?)));
    }
    let mid = rest.len() / 2;
    // `get` because a rename's midpoint can land inside a multi-byte character.
    Some(strip_side(rest.get(mid + 1..)?))
}

/// Index of the quote that closes the one at the start of `s`.
fn closing_quote(s: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, b) in s.bytes().enumerate().skip(1) {
        match b {
            _ if escaped => escaped = false,
            b'\\' => escaped = true,
            b'"' => return Some(i),
            _ => {}
        }
    }
    None
}

/// Reverses git's C-style quoting. Unquoted input is returned as is.
fn unquote(s: &str) -> String {
    let Some(inner) = s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) else {
        return s.to_string();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut it = inner.bytes().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match it.next() {
            Some(b'a') => bytes.push(0x07),
            Some(b'b') => bytes.push(0x08),
            Some(b'f') => bytes.push(0x0c),
            Some(b'v') => bytes.push(0x0b),
            Some(b'n') => bytes.push(b'\n'),
            Some(b't') => bytes.push(b'\t'),
            Some(b'r') => bytes.push(b'\r'),
            Some(d @ b'0'..=b'7') => {
                let mut v = (d - b'0') as u32;
                for _ in 0..2 {
                    if let Some(&n @ b'0'..=b'7') = it.peek() {
                        v = v * 8 + (n - b'0') as u32;
                        it.next();
                    }
                }
                bytes.push(v as u8);
            }
            Some(other) => bytes.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODIFY: &str = "diff --git a/a.txt b/a.txt\nindex 814f4a4..475bbc3 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,3 @@\n one\n-two\n+TWO\n+three\n";

    #[test]
    fn parses_modified_file() {
        let files = parse(MODIFY.as_bytes());
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!((f.path.as_str(), f.status), ("a.txt", Status::Modified));
        assert_eq!((f.added, f.removed), (2, 1));
        let lines = &f.hunks[0].lines;
        assert_eq!(lines[0].kind, LineKind::Context);
        assert_eq!((lines[0].old_no, lines[0].new_no), (Some(1), Some(1)));
        assert_eq!((lines[1].old_no, lines[1].new_no), (Some(2), None));
        assert_eq!((lines[2].old_no, lines[2].new_no), (None, Some(2)));
        assert_eq!(lines[3].new_no, Some(3));
    }

    #[test]
    fn parses_pure_rename_with_no_hunks() {
        let p = "diff --git a/big.txt b/moved.txt\nsimilarity index 100%\nrename from big.txt\nrename to moved.txt\n";
        let f = &parse(p.as_bytes())[0];
        assert_eq!((f.status, f.path.as_str(), f.old_path.as_deref()), (Status::Renamed, "moved.txt", Some("big.txt")));
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn parses_binary_file() {
        let p = "diff --git a/bin.dat b/bin.dat\nindex 8352675..9388380 100644\nBinary files a/bin.dat and b/bin.dat differ\n";
        assert!(parse(p.as_bytes())[0].binary);
    }

    #[test]
    fn parses_added_and_deleted_files() {
        let p = "diff --git a/n.txt b/n.txt\nnew file mode 100644\nindex 0000000..3e75765\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+new\ndiff --git a/o.txt b/o.txt\ndeleted file mode 100644\nindex 3e75765..0000000\n--- a/o.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n";
        let files = parse(p.as_bytes());
        assert_eq!((files[0].status, files[0].path.as_str()), (Status::Added, "n.txt"));
        assert_eq!((files[1].status, files[1].path.as_str()), (Status::Deleted, "o.txt"));
    }

    #[test]
    fn keeps_paths_with_spaces() {
        let p = "diff --git a/my file.txt b/my file.txt\n--- a/my file.txt\n+++ b/my file.txt\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(parse(p.as_bytes())[0].path, "my file.txt");
    }

    #[test]
    fn unquotes_special_paths() {
        let p = "diff --git \"a/t\\tb.txt\" \"b/t\\tb.txt\"\n--- \"a/t\\tb.txt\"\n+++ \"b/t\\tb.txt\"\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(parse(p.as_bytes())[0].path, "t\tb.txt");
    }

    #[test]
    fn drops_the_tab_git_appends_to_paths_with_spaces() {
        let p = "diff --git a/a b/c.txt b/a b/c.txt\n--- a/a b/c.txt\t\n+++ b/a b/c.txt\t\n@@ -1 +1 @@\n-a\n+b\ndiff --git \"a/t\\tb c\" \"b/t\\tb c\"\n--- \"a/t\\tb c\"\t\n+++ \"b/t\\tb c\"\t\n@@ -1 +1 @@\n-a\n+b\n";
        let files = parse(p.as_bytes());
        assert_eq!(files[0].path, "a b/c.txt");
        assert_eq!(files[1].path, "t\tb c");
    }

    #[test]
    fn reads_header_only_paths_with_a_b_and_quotes() {
        let p = "diff --git a/x b/new.txt b/x b/new.txt\nnew file mode 100644\nindex 0000000..e69de29\ndiff --git \"a/e\\\" y\" \"b/e\\\" y\"\nnew file mode 100644\nindex 0000000..e69de29\n";
        let files = parse(p.as_bytes());
        assert_eq!((files[0].status, files[0].path.as_str()), (Status::Added, "x b/new.txt"));
        assert_eq!(files[1].path, "e\" y");
    }

    #[test]
    fn unquotes_every_c_escape_git_writes() {
        assert_eq!(unquote(r#""a\ab\bc\td\ne\vf\fg\rh\"i\\j\303\251""#), "a\x07b\x08c\td\ne\x0bf\x0cg\rh\"i\\jé");
    }

    #[test]
    fn body_lines_that_look_like_headers_stay_in_the_hunk() {
        let p = "diff --git a/s.md b/s.md\n--- a/s.md\n+++ b/s.md\n@@ -1,2 +1,2 @@\n--- old heading\n+++ new heading\n context\n";
        let f = &parse(p.as_bytes())[0];
        assert_eq!((f.added, f.removed), (1, 1));
    }

    #[test]
    fn keeps_invalid_utf8_lines() {
        let mut p = b"diff --git a/l.txt b/l.txt\n--- a/l.txt\n+++ b/l.txt\n@@ -1 +1 @@\n-a\n+".to_vec();
        p.extend([0xff, 0xfe, b'\n']);
        assert_eq!(parse(&p)[0].added, 1);
    }

    #[test]
    fn no_newline_marker_is_not_a_line() {
        let p = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-x\n\\ No newline at end of file\n+y\n";
        assert_eq!(parse(p.as_bytes())[0].hunks[0].lines.len(), 2);
    }

    #[test]
    fn empty_input_yields_no_files() {
        assert!(parse(b"").is_empty());
    }

    const NEXT_FILE: &str = "diff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-x\n+y\n";

    fn assert_next_file_survives(hunk: &str, kept_lines: usize) {
        let p = format!("diff --git a/a b/a\n--- a/a\n+++ b/a\n{hunk}{NEXT_FILE}");
        let files = parse(p.as_bytes());
        assert_eq!(files.len(), 2, "{hunk:?}");
        assert_eq!(files[0].path, "a", "{hunk:?}");
        assert_eq!(files[0].hunks[0].lines.len(), kept_lines, "{hunk:?}");
        assert_eq!((files[1].path.as_str(), files[1].added, files[1].removed), ("b", 1, 1), "{hunk:?}");
    }

    #[test]
    fn hunk_body_shorter_than_its_counts_does_not_panic() {
        let f = &parse(b"diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,2 +1,1 @@\n+a\n")[0];
        assert_eq!(f.added, 1);
        assert_eq!(f.hunks[0].lines.len(), 1);
    }

    #[test]
    fn hunk_body_longer_than_its_counts_does_not_panic() {
        assert_next_file_survives("@@ -1 +1 @@\n-a\n+b\n+c\n-d\n e\n", 2);
    }

    #[test]
    fn line_side_exceeding_its_count_does_not_panic() {
        assert_next_file_survives("@@ -1,3 +1,1 @@\n+a\n+b\n c\n", 3);
    }

    #[test]
    fn diff_header_ends_a_miscounted_hunk() {
        assert_next_file_survives("@@ -1,9 +1,9 @@\n one\n", 1);
    }

    #[test]
    fn hunk_starting_at_the_largest_line_number_does_not_panic() {
        let p = format!("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -{0},2 +{0},2 @@\n a\n b\n", u32::MAX);
        assert_eq!(parse(p.as_bytes())[0].hunks[0].lines.len(), 2);
    }

    fn realistic_patch() -> Vec<u8> {
        let mut p = "diff --git a/src/é中🦀.rs b/src/é中🦀.rs\nindex 814f4a4..475bbc3 100644\n--- a/src/é中🦀.rs\n+++ b/src/é中🦀.rs\n@@ -1,3 +1,3 @@ fn é() {\n é\n-中\n+🦀\n \n\\ No newline at end of file\n".as_bytes().to_vec();
        p.extend(b"diff --git a/ol\xc3\xa9 x.txt b/n\xc3\xa9w x.txt\nsimilarity index 80%\nrename from ol\xc3\xa9 x.txt\nrename to n\xc3\xa9w x.txt\n--- a/ol\xc3\xa9 x.txt\t\n+++ b/n\xc3\xa9w x.txt\t\n@@ -1 +1 @@\n-a\xff\n+b\r\n");
        p.extend(b"diff --git \"a/q\\\" \\303\\251\\t.txt\" \"b/q\\\" \\303\\251\\t.txt\"\nnew file mode 100644\nindex 0000000..e69de29\n");
        p.extend("diff --git a/é.bin b/é.bin\nBinary files a/é.bin and b/é.bin differ\n".as_bytes());
        p.extend("diff --git a/é b/é\ndeleted file mode 100644\n--- a/é\n+++ /dev/null\n@@ -1 +0,0 @@\n-é\n".as_bytes());
        p
    }

    #[test]
    fn realistic_patch_parses_in_full() {
        let files = parse(&realistic_patch());
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["src/é中🦀.rs", "néw x.txt", "q\" é\t.txt", "é.bin", "é"]);
    }

    #[test]
    fn truncating_a_patch_at_any_byte_does_not_panic() {
        let p = realistic_patch();
        for cut in 0..=p.len() {
            parse(&p[..cut]);
            parse(&p[cut..]);
        }
    }

    // Truncation alone never puts a non-ASCII byte first on a line, which is
    // the case the `&line[1..]` slices rely on.
    #[test]
    fn corrupting_any_byte_of_a_patch_does_not_panic() {
        let p = realistic_patch();
        for i in 0..p.len() {
            for b in [0x00, 0x80, 0xc3, 0xff, b'\n', b'"', b'\\', b' ', b'+', b'-', b'@', b'9'] {
                let mut bad = p.clone();
                bad[i] = b;
                parse(&bad);
            }
        }
    }
}
