use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Longest encoded name kept whole. File names are usually limited to 255
/// bytes, and this leaves room for the prefix and every suffix the store adds.
const MAX_ENCODED_LEN: usize = 200;

/// Numbers temp files within this process.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Which file version `Comment::line` counts in. A replaced line has the
/// same number on both sides, so the number alone is ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Unreadable files `load` could not move, which `save` must not replace.
    /// Fixing the permissions behind a failed save would otherwise let the
    /// next save overwrite them.
    unmoved: Mutex<HashSet<PathBuf>>,
}

impl Store {
    pub fn new(git_common_dir: &Path) -> Self {
        Self { dir: git_common_dir.join("branchdiff"), unmoved: Mutex::default() }
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
        let parsed = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).ok(),
            Err(e) if e.kind() == ErrorKind::NotFound => Some(Review::default()),
            Err(_) => None,
        };
        let mut unmoved = self.unmoved.lock().unwrap();
        unmoved.remove(&path);
        if let Some(review) = parsed {
            return LoadedReview { review, ..LoadedReview::default() };
        }
        match set_aside(&path) {
            Ok(moved_aside) => LoadedReview { moved_aside, ..LoadedReview::default() },
            Err(_) => {
                unmoved.insert(path.clone());
                LoadedReview { left_in_place: Some(path), ..LoadedReview::default() }
            }
        }
    }

    /// Writes to a temp file then renames, so a crash cannot corrupt the review.
    ///
    /// Replaces the whole file: if two processes edit one review, the last
    /// save wins.
    pub fn save(&self, id: &ReviewId, review: &Review) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(review)?;
        let path = self.path_for(id);
        let mut unmoved = self.unmoved.lock().unwrap();
        if unmoved.contains(&path) {
            set_aside(&path).map_err(|e| anyhow!("{e}: could not move aside {}", path.display()))?;
            unmoved.remove(&path);
        }
        drop(unmoved);
        self.replace(&path, &bytes)
            .map_err(|e| anyhow!("{e}: {}", self.dir.display()))
    }

    fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let tmp = temp_path(path);
        let result = write_synced(&tmp, bytes).and_then(|()| fs::rename(&tmp, path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

/// Processes share the store directory, and a shared temp file would let one
/// rename the other's half-written data into place.
fn temp_path(path: &Path) -> PathBuf {
    let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!("json.{}.{n}.tmp", std::process::id()))
}

/// Flushes before returning. Otherwise a power loss can keep the rename but
/// not the data, leaving an empty file in place of the review.
fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
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
        let hash = format!("~{:016x}", fnv1a(s));
        out.truncate(MAX_ENCODED_LEN - hash.len());
        out.push_str(&hash);
    }
    out
}

/// FNV-1a. File names must not change between builds, which rules out
/// `DefaultHasher`.
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |hash, b| (hash ^ u64::from(b)).wrapping_mul(0x100000001b3))
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

    #[test]
    fn failed_save_leaves_old_file_intact() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        let mut r = Review::default();
        r.comments.push(comment("keep"));
        store.save(&id, &r).unwrap();
        // A temp file left by an interrupted save must not affect loading.
        std::fs::write(store.path_for(&id).with_extension("json.tmp"), "{ truncated").unwrap();
        assert_eq!(store.load(&id).review, r);
    }

    #[test]
    fn corrupt_file_is_preserved_and_loads_empty() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        store.save(&id, &Review::default()).unwrap();
        std::fs::write(store.path_for(&id), "not json").unwrap();
        assert_eq!(store.load(&id).review, Review::default());
        assert!(store.path_for(&id).with_extension("json.corrupt").exists());
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
                    for _ in 0..25 {
                        store.save(id, &r).unwrap();
                        let saved = std::fs::read(path).unwrap();
                        serde_json::from_slice::<Review>(&saved).unwrap();
                    }
                });
            }
        });
    }

    #[test]
    fn failed_save_leaves_no_temp_file() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        // A directory in the way makes the final rename fail.
        std::fs::create_dir_all(store.path_for(&id).join("blocker")).unwrap();
        assert!(store.save(&id, &Review::default()).is_err());
        assert_eq!(std::fs::read_dir(dir.path().join("branchdiff")).unwrap().count(), 1);
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
    fn file_that_is_not_utf8_is_preserved() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        store.save(&id, &Review::default()).unwrap();
        std::fs::write(store.path_for(&id), b"{\xff}").unwrap();
        assert_eq!(store.load(&id).review, Review::default());
        store.save(&id, &Review::default()).unwrap();
        assert_eq!(std::fs::read(store.path_for(&id).with_extension("json.corrupt")).unwrap(), b"{\xff}");
    }

    #[test]
    fn file_that_cannot_be_read_is_moved_aside() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        // A directory fails to read on every platform, even when run as root.
        std::fs::create_dir_all(store.path_for(&id)).unwrap();
        assert_eq!(store.load(&id).review, Review::default());
        assert!(store.path_for(&id).with_extension("json.corrupt").exists());
    }

    #[test]
    fn second_corruption_keeps_the_first_copy() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        store.save(&id, &Review::default()).unwrap();
        for text in ["first", "second"] {
            std::fs::write(store.path_for(&id), text).unwrap();
            assert_eq!(store.load(&id).review, Review::default());
        }
        let copy = |ext: &str| std::fs::read_to_string(store.path_for(&id).with_extension(ext)).unwrap();
        assert_eq!(copy("json.corrupt"), "first");
        assert_eq!(copy("json.corrupt.1"), "second");
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
            assert_ne!(moved, store.path_for(&id));
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
        let moved = store.load(&id).moved_aside.unwrap();
        assert_eq!(std::fs::read_to_string(moved.join("inside")).unwrap(), "original");
        assert!(!store.path_for(&id).exists());
    }

    #[test]
    fn each_corruption_reports_its_own_copy() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        store.save(&id, &Review::default()).unwrap();
        for text in ["first", "second"] {
            std::fs::write(store.path_for(&id), text).unwrap();
            let moved = store.load(&id).moved_aside.unwrap();
            assert_eq!(std::fs::read_to_string(moved).unwrap(), text);
        }
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

    /// Leaves a corrupt file for `id` in a read-only store directory, or
    /// returns false if permissions don't apply, as when running as root.
    #[cfg(unix)]
    fn corrupt_file_in_read_only_dir(store: &Store, id: &ReviewId) -> bool {
        store.save(id, &Review::default()).unwrap();
        std::fs::write(store.path_for(id), "not json").unwrap();
        set_store_dir_mode(store, 0o555);
        let enforced = std::fs::File::create(store.dir.join("probe")).is_err();
        if !enforced {
            set_store_dir_mode(store, 0o755);
        }
        enforced
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
        store.load(&id);
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
        store.load(&id);
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
    fn temp_names_are_unique_and_carry_the_process_id() {
        let path = Path::new("repo/branch-feat.json");
        let (first, second) = (temp_path(path), temp_path(path));
        assert_ne!(first, second);
        assert_eq!(first.parent(), path.parent());
        let name = first.file_name().unwrap().to_str().unwrap();
        assert!(name.contains(&format!(".{}.", std::process::id())), "{name}");
    }

    #[cfg(unix)]
    #[test]
    fn successful_retry_stops_later_saves_moving_the_healthy_file() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        if !corrupt_file_in_read_only_dir(&store, &id) {
            return;
        }
        store.load(&id);
        set_store_dir_mode(&store, 0o755);
        let mut r = Review::default();
        r.comments.push(comment("first"));
        store.save(&id, &r).unwrap();
        r.comments.push(comment("second"));
        store.save(&id, &r).unwrap();
        // `save` always writes this path, so only a second copy shows a move.
        assert!(!store.path_for(&id).with_extension("json.corrupt.1").exists());
        assert_eq!(std::fs::read(store.path_for(&id).with_extension("json.corrupt")).unwrap(), b"not json");
        assert_eq!(store.load(&id).review, r);
    }

    #[cfg(unix)]
    #[test]
    fn load_of_a_healthy_file_stops_later_saves_moving_it() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        if !corrupt_file_in_read_only_dir(&store, &id) {
            return;
        }
        store.load(&id);
        set_store_dir_mode(&store, 0o755);
        let mut r = Review::default();
        r.comments.push(comment("repaired"));
        std::fs::write(store.path_for(&id), serde_json::to_vec(&r).unwrap()).unwrap();
        assert_eq!(store.load(&id).review, r);
        store.save(&id, &r).unwrap();
        // `save` always writes this path, so only a copy shows a move.
        assert!(!store.path_for(&id).with_extension("json.corrupt").exists());
        assert_eq!(store.load(&id).review, r);
    }

    #[test]
    fn flags_comment_outdated_when_line_text_changes() {
        let c = comment("x");
        assert!(!c.is_outdated(Some("let x = 1;")));
        assert!(c.is_outdated(Some("let x = 2;")));
        assert!(c.is_outdated(None));
    }
}
