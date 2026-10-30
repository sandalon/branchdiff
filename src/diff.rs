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
    /// The `a..b` blob pair from the `index` line. Both sides are kept so a
    /// moved fork point changes the ID. `None` when git writes no `index` line,
    /// as for mode-only changes and pure renames.
    pub content_id: Option<String>,
    pub added: u32,
    pub removed: u32,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    fn empty() -> Self {
        Self { path: String::new(), old_path: None, status: Status::Modified, binary: false, content_id: None, added: 0, removed: 0, hunks: Vec::new() }
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
            // Not a body line, so the hunk ends here.
            (old_left, new_left) = (0, 0);
        }

        if line.starts_with("diff --git ") {
            finish_file(&mut files, cur.take());
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
        } else if let Some(rest) = line.strip_prefix("index ") {
            file.content_id = rest.split(' ').next().map(str::to_string);
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
    finish_file(&mut files, cur);
    files
}

fn finish_file(files: &mut Vec<FileDiff>, file: Option<FileDiff>) {
    let Some(mut file) = file else { return };
    file.hunks.iter_mut().for_each(|h| h.lines.shrink_to_fit());
    files.push(file);
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

use crate::git::{self, GitError};
use crate::store::fnv1a;
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Untracked files above this size are listed but not read, and show as binary.
/// Holding one in memory costs about 3 times its size for log lines and 40 times
/// for very short ones, on every reload. This bounds a single file, and
/// `MAX_UNTRACKED_TOTAL_BYTES` bounds what many of them cost together.
pub const MAX_UNTRACKED_BYTES: u64 = 16 * 1024 * 1024;

/// What one load may spend on untracked files, charged in `git ls-files` order
/// so the same files are read every time. Once it is spent, the remaining files
/// show as over `MAX_UNTRACKED_BYTES` without being read. The file that spends
/// the last of it is still kept, so one file's cost can come on top of this.
pub const MAX_UNTRACKED_TOTAL_BYTES: usize = 512 * 1024 * 1024;

/// Charged per line on top of its bytes: `size_of::<Line>()` plus the smallest heap block.
const LINE_OVERHEAD_BYTES: usize = 64;

/// Everything the worktree changed since `merge_base`, including untracked
/// files. `cwd` must be the worktree root, as for `git::untracked`.
pub fn load(cwd: &Path, merge_base: &str) -> Result<Vec<FileDiff>, GitError> {
    let mut files = parse(&git::diff(cwd, merge_base)?);
    files.extend(untracked_files(cwd, MAX_UNTRACKED_TOTAL_BYTES)?);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn untracked_files(cwd: &Path, mut budget: usize) -> Result<Vec<FileDiff>, GitError> {
    Ok(git::untracked(cwd)?.into_iter().map(|path| untracked_file(cwd, path, &mut budget)).collect())
}

/// Stands in for a hash of a file too large to read. Saved reviews depend on
/// content IDs, so these must not change between builds.
fn metadata_id(m: &Metadata) -> u64 {
    let nanos = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    fnv1a(format!("{} {nanos}", m.len()).as_bytes())
}

fn untracked_file(cwd: &Path, path: String, budget: &mut usize) -> FileDiff {
    let mut f = FileDiff::empty();
    f.status = Status::Untracked;
    let full = cwd.join(&path);
    let mut over_cap_id = None;
    // A symlink shows its target, as it does in a patch. Everything else goes
    // through one handle, and only a regular file is read. Other paths, such as
    // a nested repository's `nest/`, have no content.
    let bytes = match std::fs::symlink_metadata(&full) {
        Ok(m) if m.is_symlink() => {
            std::fs::read_link(&full).map(|t| t.into_os_string().into_encoded_bytes()).unwrap_or_default()
        }
        _ => match read_regular(&full, budget) {
            Ok(Regular::Bytes(bytes)) => bytes,
            Ok(Regular::OverCap(m)) => {
                over_cap_id = Some(metadata_id(&m));
                f.binary = true;
                Vec::new()
            }
            Err(_) => Vec::new(),
        },
    };
    f.content_id = Some(format!("{:x}", over_cap_id.unwrap_or_else(|| fnv1a(&bytes))));
    // Same heuristic git uses: a NUL in the first 8000 bytes means binary.
    f.binary |= bytes.iter().take(8000).any(|b| *b == 0);
    if !f.binary {
        let text = String::from_utf8_lossy(&bytes);
        // Not `lines()`, which drops the `\r` of CRLF that patch lines keep.
        let mut lines: Vec<Line> = text.split_terminator('\n').enumerate().map(|(i, l)| Line {
            kind: LineKind::Added,
            old_no: None,
            new_no: Some(i as u32 + 1),
            text: l.to_string(),
        }).collect();
        lines.shrink_to_fit();
        *budget = budget.saturating_sub(lines.len().saturating_mul(LINE_OVERHEAD_BYTES));
        f.added = lines.len() as u32;
        if !lines.is_empty() {
            // Like git, which leaves out a count of 1.
            let header = if lines.len() == 1 { "@@ -0,0 +1 @@".to_string() } else { format!("@@ -0,0 +1,{} @@", lines.len()) };
            f.hunks.push(Hunk { header, lines });
        }
    }
    f.path = path;
    f
}

enum Regular {
    /// The content of a file within the cap, or nothing for any other kind of path.
    Bytes(Vec<u8>),
    /// Too big to keep, with metadata to identify this version of the file by.
    OverCap(Metadata),
}

fn read_regular(path: &Path, budget: &mut usize) -> io::Result<Regular> {
    let file = open_untracked(path)?;
    let meta = file.metadata()?;
    read_handle(file, meta, budget)
}

/// `meta` comes from `file` itself, not from the path, which can change at any time.
fn read_handle(mut file: File, meta: Metadata, budget: &mut usize) -> io::Result<Regular> {
    if !meta.is_file() {
        return Ok(Regular::Bytes(Vec::new()));
    }
    if meta.len() > MAX_UNTRACKED_BYTES || *budget == 0 {
        return Ok(Regular::OverCap(meta));
    }
    // Bounded because the file can grow after `meta` was taken. What was read of
    // a file that did is thrown away, so its ID never depends on where it stopped.
    let mut bytes = Vec::new();
    file.by_ref().take(MAX_UNTRACKED_BYTES + 1).read_to_end(&mut bytes)?;
    *budget = budget.saturating_sub(bytes.len());
    if bytes.len() as u64 > MAX_UNTRACKED_BYTES {
        return Ok(Regular::OverCap(file.metadata().unwrap_or(meta)));
    }
    Ok(Regular::Bytes(bytes))
}

/// The path may have been replaced since it was listed, so opening must neither
/// wait for a FIFO's writer, which would hang the worker, nor follow a symlink.
#[cfg(unix)]
fn open_untracked(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    File::options().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW).open(path)
}

#[cfg(not(unix))]
fn open_untracked(path: &Path) -> io::Result<File> {
    File::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mkfifo(path: &Path) {
        let status = std::process::Command::new("mkfifo").arg(path).status().unwrap();
        assert!(status.success());
    }

    /// On a thread so that a regression fails here instead of hanging the run.
    #[cfg(unix)]
    #[test]
    fn read_regular_does_not_wait_for_a_fifo_writer() {
        let dir = tempfile::TempDir::new().unwrap();
        let fifo = dir.path().join("pipe");
        mkfifo(&fifo);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read_regular(&fifo, &mut usize::MAX.clone()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("blocked on a FIFO");
        assert!(matches!(result, Ok(Regular::Bytes(b)) if b.is_empty()));
    }

    /// The path is checked for a symlink before this runs, but one can be
    /// swapped in afterwards.
    #[cfg(unix)]
    #[test]
    fn read_regular_refuses_a_symlink_swapped_in_after_the_check() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("secret"), "x\n").unwrap();
        std::os::unix::fs::symlink("secret", dir.path().join("link")).unwrap();
        assert!(read_regular(&dir.path().join("link"), &mut usize::MAX.clone()).is_err());
    }

    #[test]
    fn a_file_that_grows_past_the_cap_after_its_size_is_taken_counts_as_over_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "x\n").unwrap();
        let file = File::open(&path).unwrap();
        let stale = file.metadata().unwrap();
        File::options().write(true).open(&path).unwrap().set_len(MAX_UNTRACKED_BYTES + 1).unwrap();
        let mut budget = usize::MAX;
        let result = read_handle(file, stale, &mut budget).unwrap();
        assert!(matches!(result, Regular::OverCap(m) if m.len() == MAX_UNTRACKED_BYTES + 1));
        assert_eq!(usize::MAX - budget, MAX_UNTRACKED_BYTES as usize + 1, "what was read still costs");
    }

    #[test]
    fn a_file_exactly_at_the_cap_is_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("log");
        let file = File::create(&path).unwrap();
        file.set_len(MAX_UNTRACKED_BYTES).unwrap();
        let file = File::open(&path).unwrap();
        let meta = file.metadata().unwrap();
        let result = read_handle(file, meta, &mut usize::MAX.clone()).unwrap();
        assert!(matches!(result, Regular::Bytes(b) if b.len() as u64 == MAX_UNTRACKED_BYTES));
    }

    /// An unborn repository is enough, since `git ls-files --others` needs no commits.
    fn untracked_repo(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(git::run(dir.path(), ["init", "-q"]).unwrap().code, Some(0));
        for (name, content) in files {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        dir
    }

    /// Each file costs 5 bytes plus one line.
    const FOUR_FILES: [(&str, &[u8]); 4] =
        [("d.txt", b"dddd\n"), ("c.txt", b"cccc\n"), ("b.txt", b"bbbb\n"), ("a.txt", b"aaaa\n")];
    const ONE_FILE_COST: usize = 5 + LINE_OVERHEAD_BYTES;

    fn read_paths(files: &[FileDiff]) -> Vec<&str> {
        files.iter().filter(|f| !f.binary).map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn the_budget_is_spent_on_bytes_and_lines_in_path_order() {
        let dir = untracked_repo(&FOUR_FILES);
        let spent_after_two = untracked_files(dir.path(), 2 * ONE_FILE_COST).unwrap();
        assert_eq!(read_paths(&spent_after_two), ["a.txt", "b.txt"]);
        let one_byte_more = untracked_files(dir.path(), 2 * ONE_FILE_COST + 1).unwrap();
        assert_eq!(read_paths(&one_byte_more), ["a.txt", "b.txt", "c.txt"], "the last file may overshoot");
    }

    #[test]
    fn files_past_the_budget_show_as_over_cap_with_a_content_id() {
        let dir = untracked_repo(&FOUR_FILES);
        let files = untracked_files(dir.path(), 1).unwrap();
        for f in &files[1..] {
            assert!(f.binary && f.hunks.is_empty() && f.content_id.is_some(), "{f:?}");
        }
    }

    #[test]
    fn untracked_content_ids_are_stable_across_builds() {
        // Pinned because reviewed marks saved to disk are derived from these.
        let dir = untracked_repo(&[("a.txt", b"hello"), ("b.txt", b"world")]);
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000);
        File::options().write(true).open(dir.path().join("b.txt")).unwrap().set_modified(mtime).unwrap();
        let files = untracked_files(dir.path(), 1).unwrap();
        assert_eq!(files[0].content_id.as_deref(), Some("a430d84680aabd0b"), "read from its bytes");
        assert!(files[1].binary, "over cap");
        assert_eq!(files[1].content_id.as_deref(), Some("1427257817dd4a16"), "from size and mtime");
    }

    #[test]
    fn the_same_files_are_read_on_every_load() {
        let dir = untracked_repo(&FOUR_FILES);
        let first = untracked_files(dir.path(), 2 * ONE_FILE_COST).unwrap();
        assert_eq!(read_paths(&first), ["a.txt", "b.txt"]);
        for _ in 0..3 {
            assert_eq!(first, untracked_files(dir.path(), 2 * ONE_FILE_COST).unwrap());
        }
    }

    #[test]
    fn a_binary_file_spends_budget_though_it_is_not_kept() {
        let dir = untracked_repo(&[("a.bin", &[0; 100]), ("b.txt", b"x\n")]);
        let spent = untracked_files(dir.path(), 100).unwrap();
        assert_eq!((spent[0].binary, spent[1].binary), (true, true));
        assert!(!untracked_files(dir.path(), 101).unwrap()[1].binary);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_still_shows_its_target_once_the_budget_is_spent() {
        let dir = untracked_repo(&[("a.txt", b"aaaa\n")]);
        std::os::unix::fs::symlink("a.txt", dir.path().join("link")).unwrap();
        let files = untracked_files(dir.path(), 1).unwrap();
        assert_eq!(files[1].hunks[0].lines[0].text, "a.txt");
    }

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
        assert_eq!(f.hunks[0].header, "@@ -1,2 +1,3 @@");
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["one", "two", "TWO", "three"]);
    }

    #[test]
    fn numbers_each_hunk_from_its_own_header() {
        let p = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -10,2 +10,3 @@ fn f() {\n c\n+d\n e\n";
        let f = &parse(p.as_bytes())[0];
        assert_eq!(f.hunks.len(), 2);
        assert_eq!((f.added, f.removed), (2, 1));
        let numbers = |h: &Hunk| h.lines.iter().map(|l| (l.old_no, l.new_no)).collect::<Vec<_>>();
        assert_eq!(f.hunks[0].header, "@@ -1,2 +1,2 @@");
        assert_eq!(numbers(&f.hunks[0]), [(Some(1), Some(1)), (Some(2), None), (None, Some(2))]);
        assert_eq!(f.hunks[1].header, "@@ -10,2 +10,3 @@ fn f() {");
        assert_eq!(numbers(&f.hunks[1]), [(Some(10), Some(10)), (None, Some(11)), (Some(11), Some(12))]);
    }

    #[test]
    fn keeps_tabs_and_trailing_carriage_returns_in_text() {
        let p = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-\ta\tb\r\n+\tc\r\n";
        let lines = &parse(p.as_bytes())[0].hunks[0].lines;
        assert_eq!(lines[0].text, "\ta\tb\r");
        assert_eq!(lines[1].text, "\tc\r");
    }

    #[test]
    fn unquotes_both_sides_of_a_rename() {
        let p = "diff --git \"a/o\\tld.txt\" \"b/n\\tew.txt\"\nsimilarity index 100%\nrename from \"o\\tld.txt\"\nrename to \"n\\tew.txt\"\n";
        let f = &parse(p.as_bytes())[0];
        assert_eq!((f.old_path.as_deref(), f.path.as_str()), (Some("o\tld.txt"), "n\tew.txt"));
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

    #[test]
    fn content_id_is_the_index_token_without_its_mode() {
        assert_eq!(parse(MODIFY.as_bytes())[0].content_id.as_deref(), Some("814f4a4..475bbc3"));
    }

    #[test]
    fn content_id_without_a_mode_keeps_the_whole_token() {
        let p = "diff --git a/n b/n\nnew file mode 100644\nindex 0000000..e69de29\n";
        assert_eq!(parse(p.as_bytes())[0].content_id.as_deref(), Some("0000000..e69de29"));
    }

    #[test]
    fn binary_file_has_a_content_id() {
        let p = "diff --git a/bin.dat b/bin.dat\nindex 8352675..9388380 100644\nBinary files a/bin.dat and b/bin.dat differ\n";
        assert_eq!(parse(p.as_bytes())[0].content_id.as_deref(), Some("8352675..9388380"));
    }

    #[test]
    fn pure_rename_and_mode_change_have_no_content_id() {
        let p = "diff --git a/big.txt b/moved.txt\nsimilarity index 100%\nrename from big.txt\nrename to moved.txt\ndiff --git a/m.sh b/m.sh\nold mode 100644\nnew mode 100755\n";
        let files = parse(p.as_bytes());
        assert_eq!((files[0].content_id.as_deref(), files[1].content_id.as_deref()), (None, None));
    }

    #[test]
    fn index_lines_in_a_hunk_body_are_not_content_ids() {
        let body = "@@ -1,2 +1,3 @@\n index 1111111..2222222 100644\n+index 3333333..4444444 100644\n-index 5555555..6666666 100644\n+index 7777777..8888888 100644\n";
        let without = format!("diff --git a/a b/a\n--- a/a\n+++ b/a\n{body}");
        assert_eq!(parse(without.as_bytes())[0].content_id, None);
        let with = format!("diff --git a/a b/a\nindex aaaaaaa..bbbbbbb 100644\n--- a/a\n+++ b/a\n{body}");
        assert_eq!(parse(with.as_bytes())[0].content_id.as_deref(), Some("aaaaaaa..bbbbbbb"));
    }

    #[test]
    fn releases_spare_hunk_capacity_on_every_file() {
        let body = " a\n".repeat(5);
        let file = |name: &str| format!("diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n@@ -1,5 +1,5 @@\n{body}");
        let files = parse(format!("{}{}", file("x"), file("y")).as_bytes());
        for f in &files {
            let lines = &f.hunks[0].lines;
            assert_eq!((lines.len(), lines.capacity()), (5, 5), "{}", f.path);
        }
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

    // Truncation alone never puts a non-ASCII byte first on a body line, which is
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
