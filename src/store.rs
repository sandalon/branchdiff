use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    pub fn load(&self, id: &ReviewId) -> Review {
        let path = self.path_for(id);
        let parsed = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).ok(),
            Err(e) if e.kind() == ErrorKind::NotFound => return Review::default(),
            Err(_) => None,
        };
        parsed.unwrap_or_else(|| {
            set_aside(&path);
            Review::default()
        })
    }

    /// Writes to a temp file then renames, so a crash cannot corrupt the review.
    ///
    /// Replaces the whole file: if two processes edit one review, the last
    /// save wins.
    pub fn save(&self, id: &ReviewId, review: &Review) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(review)?;
        self.replace(&self.path_for(id), &bytes)
            .map_err(|e| anyhow!("{e}: {}", self.dir.display()))
    }

    fn replace(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        // Processes share this directory, and a shared temp file would let one
        // rename the other's half-written data into place.
        let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.{}.{n}.tmp", std::process::id()));
        let result = write_synced(&tmp, bytes).and_then(|()| fs::rename(&tmp, path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

/// Flushes before returning. Otherwise a power loss can keep the rename but
/// not the data, leaving an empty file in place of the review.
fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Moves a bad file to the first free `.json.corrupt` name, keeping earlier ones.
fn set_aside(path: &Path) {
    let free = std::iter::once("json.corrupt".to_string())
        .chain((1u32..).map(|n| format!("json.corrupt.{n}")))
        .map(|extension| path.with_extension(extension))
        .find(|candidate| !candidate.exists());
    if let Some(free) = free {
        let _ = fs::rename(path, free);
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
        assert_eq!(store.load(&ReviewId::Branch("feat".into())), r);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = TempDir::new().unwrap();
        assert_eq!(Store::new(dir.path()).load(&ReviewId::Branch("x".into())), Review::default());
    }

    #[test]
    fn branches_with_slashes_do_not_collide() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let mut r = Review::default();
        r.comments.push(comment("slash"));
        store.save(&ReviewId::Branch("feature/x".into()), &r).unwrap();
        assert_eq!(store.load(&ReviewId::Branch("feature-x".into())), Review::default());
        assert_eq!(store.load(&ReviewId::Branch("feature/x".into())), r);
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
        assert_eq!(store.load(&id), r);
    }

    #[test]
    fn corrupt_file_is_preserved_and_loads_empty() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        store.save(&id, &Review::default()).unwrap();
        std::fs::write(store.path_for(&id), "not json").unwrap();
        assert_eq!(store.load(&id), Review::default());
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
            assert_eq!(store.load(&ReviewId::Branch(name.clone())).comments[0].body, *name);
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
        assert_eq!(store.load(&id), Review::default());
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
        assert_eq!(store.load(&id), Review::default());
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
            assert_eq!(store.load(&id), Review::default());
        }
        let copy = |ext: &str| std::fs::read_to_string(store.path_for(&id).with_extension(ext)).unwrap();
        assert_eq!(copy("json.corrupt"), "first");
        assert_eq!(copy("json.corrupt.1"), "second");
    }

    #[test]
    fn flags_comment_outdated_when_line_text_changes() {
        let c = comment("x");
        assert!(!c.is_outdated(Some("let x = 1;")));
        assert!(c.is_outdated(Some("let x = 2;")));
        assert!(c.is_outdated(None));
    }
}
