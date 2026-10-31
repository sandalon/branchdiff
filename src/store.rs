use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use crate::git::Worktree;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// Longest encoded name kept whole. File names are usually limited to 255
/// bytes, and this leaves room for the prefix and every suffix the store adds.
const MAX_ENCODED_LEN: usize = 200;

/// Which file version `Comment::line` counts in. A replaced line has the
/// same number on both sides, so the number alone is ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side { Old, New }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub path: String,
    pub line: u32,
    pub side: Side,
    pub line_text: String,
    pub body: String,
}

impl Comment {
    /// `current` is the text now at this comment's line, if the line still exists.
    pub fn is_outdated(&self, current: Option<&str>) -> bool {
        current != Some(self.line_text.as_str())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub comments: Vec<Comment>,
    /// Maps a file path to a hash of its diff when it was marked reviewed.
    pub reviewed: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReviewId {
    Branch(String),
    Commit(String),
}

impl ReviewId {
    pub fn of(w: &Worktree) -> Self {
        match &w.branch {
            Some(branch) => Self::Branch(branch.clone()),
            None => Self::Commit(w.head.clone()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadedReview {
    pub review: Review,
    /// Where an unreadable review was moved, so the UI can say so.
    pub moved_aside: Option<PathBuf>,
    /// An unreadable review that could not be moved. It stays where it is, and
    /// `save` won't replace it until it can be moved.
    pub left_in_place: Option<PathBuf>,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(git_common_dir: &Path) -> Self {
        Self { dir: git_common_dir.join("branchdiff") }
    }

    pub fn path_for(&self, id: &ReviewId) -> PathBuf {
        let name = match id {
            ReviewId::Branch(b) => format!("branch-{}", encode(b)),
            ReviewId::Commit(c) => format!("commit-{}", encode(c)),
        };
        self.dir.join(format!("{name}.json"))
    }

    /// Returns an empty review if there is no saved one. A file that can't be
    /// read or parsed is moved aside, so the next save can't overwrite it.
    pub fn load(&self, id: &ReviewId) -> LoadedReview {
        let path = self.path_for(id);
        if let Some(review) = read_review(&path) {
            return LoadedReview { review, ..LoadedReview::default() };
        }
        match set_aside(&path) {
            Ok(moved_aside) => LoadedReview { moved_aside, ..LoadedReview::default() },
            Err(_) => LoadedReview { left_in_place: Some(path), ..LoadedReview::default() },
        }
    }

    /// Writes to a temp file then renames, so a crash cannot corrupt the review.
    /// A file already there that can't be used is moved aside first, and if it
    /// can't be moved it is left alone and the save fails.
    ///
    /// Replaces the whole file: if two processes edit one review, the last
    /// save wins.
    pub fn save(&self, id: &ReviewId, review: &Review) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(review)?;
        let path = self.path_for(id);
        if read_review(&path).is_none() {
            set_aside(&path).map_err(|e| anyhow!("{e}: could not move aside {}", path.display()))?;
        }
        self.replace(&path, &bytes).map_err(|e| anyhow!("{e}: {}", self.dir.display()))
    }

    /// Review files end up 0600, which suits private review notes.
    fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let mut tmp = NamedTempFile::new_in(&self.dir)?;
        tmp.write_all(bytes)?;
        // Otherwise a power loss can keep the rename but not the data, leaving
        // an empty file in place of the review.
        tmp.as_file().sync_all()?;
        tmp.persist(path).map(drop).map_err(|e| e.error)
    }
}

/// Returns `None` if something is there that can't be used. A missing file is
/// an empty review, including when a file sits where the store directory goes.
fn read_review(path: &Path) -> Option<Review> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).ok(),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Some(Review::default()),
        Err(_) => None,
    }
}

/// Moves a bad file to the first free `.json.corrupt` name, keeping earlier
/// ones, and returns that name. Returns `None` if the file has gone, which
/// means another process moved it first.
fn set_aside(path: &Path) -> io::Result<Option<PathBuf>> {
    // Two processes moving different bad files at the same instant can pick the same free name, which is accepted as too narrow to matter.
    let mut free = path.with_extension("json.corrupt");
    let mut n = 0;
    while free.exists() {
        n += 1;
        free = path.with_extension(format!("json.corrupt.{n}"));
    }
    match fs::rename(path, &free) {
        Ok(()) => Ok(Some(free)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Percent-encodes anything but `[a-z0-9._-]`, so branch names map to one flat,
/// unique file name on every platform. Upper case is encoded because git can
/// hold `Feature` and `feature` together on a case-insensitive filesystem.
///
/// A name too long for a file is cut and given a hash of the whole name. The
/// `~` can't come from encoding, so it can't match a name that wasn't cut.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    if out.len() > MAX_ENCODED_LEN {
        let hash = format!("~{:016x}", fnv1a(s.as_bytes()));
        out.truncate(MAX_ENCODED_LEN - hash.len());
        out.push_str(&hash);
    }
    out
}

/// FNV-1a, for hashes that outlive the process. `DefaultHasher` can change
/// between Rust releases, which would orphan saved reviews and reviewed marks.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, b| (hash ^ u64::from(*b)).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn comment(body: &str) -> Comment {
        Comment { path: "a.rs".into(), line: 3, side: Side::New, line_text: "let x = 1;".into(), body: body.into() }
    }

    #[test]
    fn round_trips_a_review() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let mut r = Review::default();
        r.comments.push(comment("fix"));
        r.reviewed.insert("a.rs".into(), 42);
        store.save(&ReviewId::Branch("feat".into()), &r).unwrap();
        assert_eq!(store.load(&ReviewId::Branch("feat".into())).review, r);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = TempDir::new().unwrap();
        assert_eq!(Store::new(dir.path()).load(&ReviewId::Branch("x".into())).review, Review::default());
    }

    #[test]
    fn branches_with_slashes_do_not_collide() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let mut r = Review::default();
        r.comments.push(comment("slash"));
        store.save(&ReviewId::Branch("feature/x".into()), &r).unwrap();
        assert_eq!(store.load(&ReviewId::Branch("feature-x".into())).review, Review::default());
        assert_eq!(store.load(&ReviewId::Branch("feature/x".into())).review, r);
    }

    #[test]
    fn detached_head_is_keyed_by_commit() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        store.save(&ReviewId::Commit("abc".into()), &Review::default()).unwrap();
        assert!(store.path_for(&ReviewId::Commit("abc".into())).to_string_lossy().contains("commit-"));
    }

    #[cfg(unix)]
    #[test]
    fn failed_save_leaves_old_file_intact() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        let mut r = Review::default();
        r.comments.push(comment("keep"));
        store.save(&id, &r).unwrap();
        let old = std::fs::read(store.path_for(&id)).unwrap();
        if !make_store_dir_read_only(&store) {
            return;
        }
        let saved = store.save(&id, &Review::default());
        set_store_dir_mode(&store, 0o755);
        assert!(saved.is_err());
        assert_eq!(std::fs::read(store.path_for(&id)).unwrap(), old);
    }

    #[test]
    fn concurrent_saves_never_fail_or_expose_a_partial_file() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        store.save(&id, &Review::default()).unwrap();
        let path = store.path_for(&id);
        std::thread::scope(|s| {
            for writer in 0..4 {
                let (store, id, path) = (&store, &id, &path);
                s.spawn(move || {
                    let r = Review { comments: vec![comment("body"); 1 + writer * 200], ..Review::default() };
                    for _ in 0..5 {
                        store.save(id, &r).unwrap();
                        let saved = std::fs::read(path).unwrap();
                        serde_json::from_slice::<Review>(&saved).unwrap();
                    }
                });
            }
        });
        assert_eq!(std::fs::read_dir(&store.dir).unwrap().count(), 1, "no temp files left behind");
    }

    #[test]
    fn save_replaces_the_previous_review() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        store.save(&id, &Review::default()).unwrap();
        let mut r = Review::default();
        r.comments.push(comment("newer"));
        store.save(&id, &r).unwrap();
        assert_eq!(store.load(&id).review, r);
    }

    #[test]
    fn file_corrupted_after_load_is_moved_aside_by_the_next_save() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        let mut r = Review::default();
        r.comments.push(comment("in memory"));
        store.save(&id, &r).unwrap();
        let loaded = store.load(&id);
        std::fs::write(store.path_for(&id), "garbled").unwrap();
        store.save(&id, &loaded.review).unwrap();
        assert_eq!(std::fs::read(store.path_for(&id).with_extension("json.corrupt")).unwrap(), b"garbled");
        assert_eq!(store.load(&id).review, r);
    }

    #[test]
    fn nothing_is_reported_when_a_file_blocks_the_store_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("branchdiff"), "a file, not a directory").unwrap();
        let loaded = Store::new(dir.path()).load(&ReviewId::Branch("feat".into()));
        assert_eq!(loaded, LoadedReview::default());
    }

    #[test]
    fn save_error_gives_the_cause_and_the_location() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("branchdiff"), "a file, not a directory").unwrap();
        let store = Store::new(dir.path());
        let message = store.save(&ReviewId::Branch("feat".into()), &Review::default()).unwrap_err().to_string();
        assert!(message.contains("os error"), "{message}");
        assert!(message.contains(&dir.path().join("branchdiff").display().to_string()), "{message}");
    }

    #[test]
    fn long_branch_names_save_and_stay_distinct() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let names = ["語".repeat(27), "語".repeat(250), "語".repeat(250) + "x", "a".repeat(201), "a/".repeat(130) + "b"];
        for name in &names {
            let mut r = Review::default();
            r.comments.push(comment(name));
            store.save(&ReviewId::Branch(name.clone()), &r).unwrap();
        }
        for name in &names {
            assert_eq!(store.load(&ReviewId::Branch(name.clone())).review.comments[0].body, *name);
        }
    }

    #[test]
    fn branches_differing_only_in_case_use_different_files() {
        let store = Store::new(Path::new("repo"));
        let file = |branch: &str| store.path_for(&ReviewId::Branch(branch.into())).to_string_lossy().to_lowercase();
        assert_ne!(file("Feature"), file("feature"));
    }

    #[test]
    fn bad_files_report_where_their_bytes_went() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let contents: [&[u8]; 4] = [b"not json", b"{\xff}", b"", br#"{"comments":[{"path":"a"#];
        for (i, bytes) in contents.into_iter().enumerate() {
            let id = ReviewId::Branch(format!("branch{i}"));
            store.save(&id, &Review::default()).unwrap();
            std::fs::write(store.path_for(&id), bytes).unwrap();
            let loaded = store.load(&id);
            assert_eq!(loaded.review, Review::default());
            assert_eq!(loaded.left_in_place, None);
            let moved = loaded.moved_aside.unwrap();
            assert_eq!(moved, store.path_for(&id).with_extension("json.corrupt"));
            store.save(&id, &Review::default()).unwrap();
            assert_eq!(std::fs::read(moved).unwrap(), bytes);
        }
    }

    #[test]
    fn unreadable_file_reports_where_it_went() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        // A directory fails to read on every platform, even when run as root.
        std::fs::create_dir_all(store.path_for(&id)).unwrap();
        std::fs::write(store.path_for(&id).join("inside"), "original").unwrap();
        let loaded = store.load(&id);
        assert_eq!(loaded.review, Review::default());
        let moved = loaded.moved_aside.unwrap();
        assert_eq!(moved, store.path_for(&id).with_extension("json.corrupt"));
        assert_eq!(std::fs::read_to_string(moved.join("inside")).unwrap(), "original");
        assert!(!store.path_for(&id).exists());
    }

    #[test]
    fn each_corruption_reports_its_own_copy() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        store.save(&id, &Review::default()).unwrap();
        let mut moved = Vec::new();
        for text in ["first", "second"] {
            std::fs::write(store.path_for(&id), text).unwrap();
            moved.push(store.load(&id).moved_aside.unwrap());
        }
        assert_eq!(moved[0], store.path_for(&id).with_extension("json.corrupt"));
        assert_eq!(moved[1], store.path_for(&id).with_extension("json.corrupt.1"));
        assert_eq!(std::fs::read_to_string(&moved[0]).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&moved[1]).unwrap(), "second");
    }

    #[test]
    fn good_and_missing_files_report_nothing() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        let reported = |loaded: LoadedReview| loaded.moved_aside.is_some() || loaded.left_in_place.is_some();
        assert!(!reported(store.load(&id)));
        let mut r = Review::default();
        r.comments.push(comment("keep"));
        store.save(&id, &r).unwrap();
        let loaded = store.load(&id);
        assert_eq!(loaded.review, r);
        assert!(!reported(loaded));
        assert!(store.path_for(&id).exists());
    }

    /// Returns false if permissions don't apply, as when running as root.
    #[cfg(unix)]
    fn make_store_dir_read_only(store: &Store) -> bool {
        set_store_dir_mode(store, 0o555);
        let enforced = std::fs::File::create(store.dir.join("probe")).is_err();
        if !enforced {
            set_store_dir_mode(store, 0o755);
        }
        enforced
    }

    #[cfg(unix)]
    fn corrupt_file_in_read_only_dir(store: &Store, id: &ReviewId) -> bool {
        store.save(id, &Review::default()).unwrap();
        std::fs::write(store.path_for(id), "not json").unwrap();
        make_store_dir_read_only(store)
    }

    #[cfg(unix)]
    fn set_store_dir_mode(store: &Store, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&store.dir, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn file_that_cannot_be_moved_is_reported_in_place() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        if !corrupt_file_in_read_only_dir(&store, &id) {
            return;
        }
        let loaded = store.load(&id);
        set_store_dir_mode(&store, 0o755);
        assert_eq!(loaded.review, Review::default());
        assert_eq!(loaded.moved_aside, None);
        assert_eq!(loaded.left_in_place, Some(store.path_for(&id)));
    }

    #[cfg(unix)]
    #[test]
    fn save_does_not_replace_a_file_that_could_not_be_moved() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        if !corrupt_file_in_read_only_dir(&store, &id) {
            return;
        }
        let message = store.save(&id, &Review::default()).unwrap_err().to_string();
        set_store_dir_mode(&store, 0o755);
        assert!(message.contains("os error"), "{message}");
        assert!(message.contains("could not move aside"), "{message}");
        assert!(message.contains(&store.path_for(&id).display().to_string()), "{message}");
        assert_eq!(std::fs::read(store.path_for(&id)).unwrap(), b"not json");
    }

    #[cfg(unix)]
    #[test]
    fn save_after_permissions_are_fixed_keeps_the_unmoved_file() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        if !corrupt_file_in_read_only_dir(&store, &id) {
            return;
        }
        set_store_dir_mode(&store, 0o755);
        let mut r = Review::default();
        r.comments.push(comment("new"));
        store.save(&id, &r).unwrap();
        assert_eq!(std::fs::read(store.path_for(&id).with_extension("json.corrupt")).unwrap(), b"not json");
        assert_eq!(store.load(&id).review, r);
    }

    #[test]
    fn changing_the_long_name_hash_would_orphan_saved_reviews() {
        // Pinned because a saved review is only found again under this exact name.
        let store = Store::new(Path::new("repo"));
        let path = store.path_for(&ReviewId::Branch("a".repeat(300)));
        let expected = format!("branch-{}~ee7717eba7a17e21.json", "a".repeat(183));
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), expected);
    }

    #[test]
    fn review_id_follows_the_branch_or_else_the_commit() {
        let worktree = |branch: Option<&str>| Worktree { path: "wt".into(), head: "abc".into(), branch: branch.map(Into::into) };
        assert_eq!(ReviewId::of(&worktree(Some("feat"))), ReviewId::Branch("feat".into()));
        assert_eq!(ReviewId::of(&worktree(None)), ReviewId::Commit("abc".into()));
    }

    #[test]
    fn flags_comment_outdated_when_line_text_changes() {
        let c = comment("x");
        assert!(!c.is_outdated(Some("let x = 1;")));
        assert!(c.is_outdated(Some("let x = 2;")));
        assert!(c.is_outdated(None));
    }
}
