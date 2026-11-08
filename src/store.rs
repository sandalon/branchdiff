use crate::git::Worktree;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Longest encoded name kept whole. File names are usually limited to 255
/// bytes, and this leaves room for the prefix and every suffix the store adds.
const MAX_ENCODED_LEN: usize = 200;

/// Which file version `Comment::line` counts in. A replaced line has the
/// same number on both sides, so the number alone is ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Old,
    New,
}

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

/// The exact persisted revision seen by one editing state. Cloned save effects
/// share updates after successful saves, while independent worktrees/sessions
/// retain their own baseline even when they review the same commit.
#[derive(Debug, Clone, Default)]
pub struct ReviewVersion(Arc<Mutex<Option<Vec<u8>>>>);

impl ReviewVersion {
    fn from_bytes(bytes: Option<Vec<u8>>) -> Self {
        Self(Arc::new(Mutex::new(bytes)))
    }

    pub fn session_id(&self) -> usize {
        Arc::as_ptr(&self.0) as usize
    }
}

impl PartialEq for ReviewVersion {
    fn eq(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.0, &other.0) {
            return true;
        }
        let own = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let other = other.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        own == other
    }
}

impl Eq for ReviewVersion {}

#[derive(Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    /// The canonical review was left intact. This private JSON contains all
    /// local notes and reviewed marks, including local deletions.
    Conflict(PathBuf),
}

#[derive(Serialize)]
struct PersistedReview<'a> {
    #[serde(rename = "_branchdiff_revision")]
    revision: &'a str,
    #[serde(flatten)]
    review: &'a Review,
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
    pub version: ReviewVersion,
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
        // A missing directory needs no lock. If another session creates it
        // now, the first save will detect that its baseline was missing.
        if !self.dir.is_dir() {
            return LoadedReview::default();
        }
        let lock = self.lock(&path);
        // Saves replace the file by rename, so a usable review can be read
        // without the lock, as when an older version left no lock file in a
        // read-only store. Only moving an unusable file aside needs it.
        if let Some((review, bytes)) = read_review(&path) {
            return LoadedReview { review, version: ReviewVersion::from_bytes(bytes), ..LoadedReview::default() };
        }
        if lock.is_err() {
            return LoadedReview { left_in_place: Some(path), ..LoadedReview::default() };
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
    /// Compares the expected persisted revision under an interprocess lock.
    /// A conflicting snapshot is retained separately, never applied over newer
    /// notes. Only a successful canonical save advances this editing state's
    /// revision; retrying a conflict therefore remains safe.
    pub fn save(&self, id: &ReviewId, review: &Review, version: &ReviewVersion) -> Result<SaveOutcome> {
        let path = self.path_for(id);
        let mut expected = version.0.lock().unwrap_or_else(|e| e.into_inner());
        let _lock = self.lock(&path).map_err(|e| anyhow!("{e}: {}", self.dir.display()))?;
        let (current, recovered) = match read_review(&path) {
            Some((_, bytes)) => (bytes, false),
            None => {
                set_aside(&path).map_err(|e| anyhow!("{e}: could not move aside {}", path.display()))?;
                // Corrupt data has been preserved. It cannot represent a
                // competing usable review, so recovery may save local notes.
                (None, true)
            }
        };
        let conflict = current != *expected && !recovered;
        let (mut file, written) = if conflict {
            let prefix = format!("{}.conflict-", path.file_name().unwrap_or_default().to_string_lossy());
            create_unique(&self.dir, &prefix, ".json")?
        } else {
            create_unique(&self.dir, ".tmp", "")?
        };
        // The fresh file name, timestamp and process id make every saved
        // revision distinct, including writes that restore identical notes.
        let revision = format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos(),
            written.file_name().unwrap_or_default().to_string_lossy()
        );
        let bytes = serde_json::to_vec_pretty(&PersistedReview { revision: &revision, review })?;
        // Otherwise a power loss can keep the rename but not the data, leaving
        // an empty file in place of the review.
        let durable = file.write_all(&bytes).and_then(|()| file.sync_all());
        drop(file);
        if conflict {
            return match durable {
                Ok(()) => Ok(SaveOutcome::Conflict(written)),
                Err(e) => {
                    let _ = fs::remove_file(&written);
                    Err(e.into())
                }
            };
        }
        // `std::fs::rename` rather than `tempfile`'s persist: on Windows that
        // calls `MoveFileExW`, which fails for paths past MAX_PATH and when
        // another handle has the review open. std handles both.
        if let Err(e) = durable.and_then(|()| fs::rename(&written, &path)) {
            let _ = fs::remove_file(&written);
            return Err(anyhow!("{e}: {}", self.dir.display()));
        }
        *expected = Some(bytes);
        Ok(SaveOutcome::Saved)
    }

    /// Locks contain no notes and can be opened read-only by another user of
    /// a shared repository. JSON and recovery files remain owner-private.
    fn lock(&self, path: &Path) -> io::Result<fs::File> {
        fs::create_dir_all(&self.dir)?;
        // Lock a stable sibling rather than the JSON inode replaced by rename.
        // Closing the handle releases the native lock, including after a crash.
        let lock_path = path.with_extension("json.lock");
        let file = match fs::File::open(&lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                match fs::File::options().read(true).write(true).create_new(true).open(&lock_path) {
                    Ok(file) => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            // Only chmod this newly created handle. Never
                            // change an existing path's permissions or truncate
                            // it, including when another process wins creation.
                            file.set_permissions(fs::Permissions::from_mode(0o644))?;
                        }
                        file
                    }
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => fs::File::open(&lock_path)?,
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(ErrorKind::InvalidData, "review lock is not a regular file"));
        }
        lock_file(&file)?;
        Ok(file)
    }
}

/// Creates a new owner-private file in `dir` with a name no other writer has.
/// Unlike `tempfile`, std opens long Windows paths and sets no temporary
/// attribute that a persisted review would keep.
pub(crate) fn create_unique(dir: &Path, prefix: &str, suffix: &str) -> io::Result<(fs::File, PathBuf)> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    loop {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("{prefix}{:x}-{nanos:x}-{n:x}{suffix}", std::process::id()));
        let mut options = fs::File::options();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(unix)]
fn lock_file(file: &fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: file owns this live descriptor for the duration of the lock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(windows)]
fn lock_file(file: &fs::File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK};
    // SAFETY: OVERLAPPED is a C structure whose zero initialization selects
    // offset zero. This synchronous handle keeps the structure live until the
    // blocking call returns; closing file releases its one-byte lock.
    let mut overlapped = unsafe { std::mem::zeroed() };
    let locked = unsafe { LockFileEx(file.as_raw_handle(), LOCKFILE_EXCLUSIVE_LOCK, 0, 1, 0, &mut overlapped) };
    if locked == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Returns `None` if something is there that can't be used. A missing file is
/// an empty review, including when a file sits where the store directory goes.
fn read_review(path: &Path) -> Option<(Review, Option<Vec<u8>>)> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).ok().map(|review| (review, Some(bytes))),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Some((Review::default(), None)),
        Err(_) => None,
    }
}

/// Moves a bad file to the first free `.json.corrupt` name, keeping earlier
/// ones, and returns that name. Returns `None` if the file has gone, which
/// means another process moved it first.
fn set_aside(path: &Path) -> io::Result<Option<PathBuf>> {
    // Callers hold this review's stable sibling lock while selecting the name.
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

    fn save_current(store: &Store, id: &ReviewId, review: &Review) -> Result<SaveOutcome> {
        let version = store.load(id).version;
        store.save(id, review, &version)
    }

    fn notes(body: &str) -> Review {
        Review { comments: vec![comment(body)], ..Review::default() }
    }

    fn conflict(outcome: SaveOutcome) -> PathBuf {
        match outcome {
            SaveOutcome::Conflict(path) => path,
            SaveOutcome::Saved => panic!("stale snapshot replaced the canonical review"),
        }
    }

    #[test]
    fn stale_sessions_preserve_newer_notes_and_every_conflicting_snapshot() {
        let dir = TempDir::new().unwrap();
        let first = Store::new(dir.path());
        let second = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        let first_version = first.load(&id).version;
        let second_version = second.load(&id).version;
        assert_eq!(first.save(&id, &notes("newer"), &first_version).unwrap(), SaveOutcome::Saved);
        let first_recovery = conflict(second.save(&id, &notes("local"), &second_version).unwrap());
        let second_recovery = conflict(second.save(&id, &notes("local plus another note"), &second_version).unwrap());
        assert_ne!(first_recovery, second_recovery);
        assert_eq!(first.load(&id).review, notes("newer"));
        assert_eq!(read_review(&first_recovery).unwrap().0, notes("local"));
        assert_eq!(read_review(&second_recovery).unwrap().0, notes("local plus another note"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(first_recovery).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn stale_clear_cannot_erase_new_notes_or_reviewed_marks() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Commit("abc".into());
        let mut newer = notes("keep");
        newer.reviewed.insert("a.rs".into(), 42);
        let editor = store.load(&id).version;
        store.save(&id, &newer, &editor).unwrap();
        let stale = store.load(&id).version;
        newer.comments.push(comment("another session added this"));
        store.save(&id, &newer, &editor).unwrap();
        let recovery = conflict(store.save(&id, &Review::default(), &stale).unwrap());
        assert_eq!(store.load(&id).review, newer);
        assert_eq!(read_review(&recovery).unwrap().0, Review::default());
    }

    #[test]
    fn cloned_effect_versions_advance_but_independent_states_do_not() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Commit("shared-by-two-detached-worktrees".into());
        let first = store.load(&id).version;
        let later_effect = first.clone();
        let other_worktree = store.load(&id).version;
        store.save(&id, &notes("first edit"), &first).unwrap();
        assert_eq!(store.save(&id, &notes("second edit"), &later_effect).unwrap(), SaveOutcome::Saved);
        conflict(store.save(&id, &notes("other worktree"), &other_worktree).unwrap());
        assert_eq!(store.load(&id).review, notes("second edit"));
    }

    #[test]
    fn legacy_reviews_upgrade_and_external_same_revision_edits_are_detected() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("legacy".into());
        fs::create_dir_all(&store.dir).unwrap();
        fs::write(store.path_for(&id), serde_json::to_vec(&notes("legacy")).unwrap()).unwrap();
        let loaded = store.load(&id);
        assert_eq!(loaded.review, notes("legacy"));
        assert_eq!(store.save(&id, &notes("upgraded"), &loaded.version).unwrap(), SaveOutcome::Saved);
        let bytes = fs::read(store.path_for(&id)).unwrap();
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["_branchdiff_revision"].is_string());
        json["comments"][0]["body"] = "external edit preserving the revision field".into();
        fs::write(store.path_for(&id), serde_json::to_vec(&json).unwrap()).unwrap();
        conflict(store.save(&id, &notes("stale"), &loaded.version).unwrap());
        assert_eq!(store.load(&id).review.comments[0].body, "external edit preserving the revision field");
    }

    #[test]
    fn a_deleted_review_is_not_silently_recreated_by_a_stale_session() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        let version = store.load(&id).version;
        store.save(&id, &notes("before deletion"), &version).unwrap();
        fs::remove_file(store.path_for(&id)).unwrap();
        let recovery = conflict(store.save(&id, &notes("local after deletion"), &version).unwrap());
        assert!(!store.path_for(&id).exists());
        assert_eq!(read_review(&recovery).unwrap().0, notes("local after deletion"));
    }

    #[test]
    fn restoring_identical_notes_still_creates_a_new_persisted_revision() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        let editor = store.load(&id).version;
        store.save(&id, &notes("original"), &editor).unwrap();
        let stale = store.load(&id).version;
        store.save(&id, &notes("intermediate"), &editor).unwrap();
        store.save(&id, &notes("original"), &editor).unwrap();
        conflict(store.save(&id, &notes("stale edit"), &stale).unwrap());
        assert_eq!(store.load(&id).review, notes("original"));
    }

    #[cfg(unix)]
    #[test]
    fn shared_read_only_locks_work_without_changing_existing_permissions_or_contents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("shared".into());
        let version = store.load(&id).version;
        store.save(&id, &notes("first"), &version).unwrap();
        let path = store.path_for(&id).with_extension("json.lock");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        fs::write(&path, "existing lock contents").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        let loaded = store.load(&id);
        assert_eq!(loaded.review, notes("first"));
        assert_eq!(store.save(&id, &notes("second"), &loaded.version).unwrap(), SaveOutcome::Saved);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o444);
        assert_eq!(fs::read_to_string(&path).unwrap(), "existing lock contents");
        // Permit TempDir's ordinary cleanup on platforms with stricter rules.
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn wait_for(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !path.exists() {
            assert!(std::time::Instant::now() < deadline, "timed out waiting for {}", path.display());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    struct WriterChildren(Vec<std::process::Child>);

    impl Drop for WriterChildren {
        fn drop(&mut self) {
            // A broken lock or readiness assertion must fail this test rather
            // than leave child processes alive and hang the whole CI job.
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Runs only when launched by the race test below, in a fresh process with
    /// its own native lock handle and independent baseline.
    #[test]
    fn writer_subprocess() {
        let Some(root) = std::env::var_os("BRANCHDIFF_WRITER_ROOT") else { return };
        let root = PathBuf::from(root);
        let name = std::env::var("BRANCHDIFF_WRITER_NAME").unwrap();
        let store = Store::new(&root);
        let id = ReviewId::Branch("concurrent".into());
        let version = store.load(&id).version;
        fs::write(root.join(format!("{name}.ready")), "ready").unwrap();
        wait_for(&root.join("go"));
        let outcome = store.save(&id, &notes(&name), &version).unwrap();
        let result = match outcome {
            SaveOutcome::Saved => "saved".to_string(),
            SaveOutcome::Conflict(path) => path.to_string_lossy().into_owned(),
        };
        fs::write(root.join(format!("{name}.result")), result).unwrap();
    }

    #[test]
    fn separate_processes_competing_for_one_revision_keep_both_reviews() {
        let dir = TempDir::new().unwrap();
        let mut children = WriterChildren(Vec::new());
        for name in ["one", "two"] {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "store::tests::writer_subprocess", "--nocapture"])
                .env("BRANCHDIFF_WRITER_ROOT", dir.path())
                .env("BRANCHDIFF_WRITER_NAME", name)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            children.0.push(child);
        }
        for name in ["one", "two"] {
            wait_for(&dir.path().join(format!("{name}.ready")));
        }
        fs::write(dir.path().join("go"), "go").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for child in &mut children.0 {
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                assert!(std::time::Instant::now() < deadline, "writer process did not release its lock");
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                use std::io::Read;
                pipe.read_to_string(&mut stderr).unwrap();
            }
            assert!(status.success(), "{stderr}");
        }
        let outcomes: Vec<_> = ["one", "two"]
            .map(|name| fs::read_to_string(dir.path().join(format!("{name}.result"))).unwrap())
            .into_iter()
            .collect();
        assert_eq!(outcomes.iter().filter(|outcome| outcome.as_str() == "saved").count(), 1);
        let recovery = outcomes.iter().find(|outcome| outcome.as_str() != "saved").unwrap();
        let store = Store::new(dir.path());
        let canonical = store.load(&ReviewId::Branch("concurrent".into())).review;
        let recovered = read_review(Path::new(recovery)).unwrap().0;
        let mut bodies = vec![canonical.comments[0].body.as_str(), recovered.comments[0].body.as_str()];
        bodies.sort();
        assert_eq!(bodies, ["one", "two"]);
    }

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
        save_current(&store, &ReviewId::Branch("feat".into()), &r).unwrap();
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
        save_current(&store, &ReviewId::Branch("feature/x".into()), &r).unwrap();
        assert_eq!(store.load(&ReviewId::Branch("feature-x".into())).review, Review::default());
        assert_eq!(store.load(&ReviewId::Branch("feature/x".into())).review, r);
    }

    #[test]
    fn detached_head_is_keyed_by_commit() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        save_current(&store, &ReviewId::Commit("abc".into()), &Review::default()).unwrap();
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
        save_current(&store, &id, &r).unwrap();
        let old = std::fs::read(store.path_for(&id)).unwrap();
        if !make_store_dir_read_only(&store) {
            return;
        }
        let saved = save_current(&store, &id, &Review::default());
        set_store_dir_mode(&store, 0o755);
        assert!(saved.is_err());
        assert_eq!(std::fs::read(store.path_for(&id)).unwrap(), old);
    }

    #[test]
    fn concurrent_saves_never_fail_or_expose_a_partial_file() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        save_current(&store, &id, &Review::default()).unwrap();
        let path = store.path_for(&id);
        std::thread::scope(|s| {
            for writer in 0..4 {
                let (store, id, path) = (&store, &id, &path);
                s.spawn(move || {
                    let r = Review { comments: vec![comment("body"); 1 + writer * 200], ..Review::default() };
                    for _ in 0..5 {
                        save_current(store, id, &r).unwrap();
                        let saved = std::fs::read(path).unwrap();
                        serde_json::from_slice::<Review>(&saved).unwrap();
                    }
                });
            }
        });
        assert!(
            std::fs::read_dir(&store.dir).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tmp")),
            "no temp files left behind"
        );
    }

    #[test]
    fn save_replaces_the_previous_review() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        save_current(&store, &id, &Review::default()).unwrap();
        let mut r = Review::default();
        r.comments.push(comment("newer"));
        save_current(&store, &id, &r).unwrap();
        assert_eq!(store.load(&id).review, r);
    }

    #[test]
    fn file_corrupted_after_load_is_moved_aside_by_the_next_save() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("feat".into());
        let mut r = Review::default();
        r.comments.push(comment("in memory"));
        save_current(&store, &id, &r).unwrap();
        let loaded = store.load(&id);
        std::fs::write(store.path_for(&id), "garbled").unwrap();
        save_current(&store, &id, &loaded.review).unwrap();
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
        let message =
            save_current(&store, &ReviewId::Branch("feat".into()), &Review::default()).unwrap_err().to_string();
        assert!(message.contains("os error"), "{message}");
        assert!(message.contains(&dir.path().join("branchdiff").display().to_string()), "{message}");
    }

    #[test]
    fn long_branch_names_save_and_stay_distinct() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let names =
            ["語".repeat(27), "語".repeat(250), "語".repeat(250) + "x", "a".repeat(201), "a/".repeat(130) + "b"];
        for name in &names {
            let mut r = Review::default();
            r.comments.push(comment(name));
            save_current(&store, &ReviewId::Branch(name.clone()), &r).unwrap();
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
            save_current(&store, &id, &Review::default()).unwrap();
            std::fs::write(store.path_for(&id), bytes).unwrap();
            let loaded = store.load(&id);
            assert_eq!(loaded.review, Review::default());
            assert_eq!(loaded.left_in_place, None);
            let moved = loaded.moved_aside.unwrap();
            assert_eq!(moved, store.path_for(&id).with_extension("json.corrupt"));
            save_current(&store, &id, &Review::default()).unwrap();
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
        save_current(&store, &id, &Review::default()).unwrap();
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
        save_current(&store, &id, &r).unwrap();
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
        save_current(store, id, &Review::default()).unwrap();
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
    fn a_review_without_a_lock_file_still_loads_from_a_read_only_store() {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path());
        let id = ReviewId::Branch("legacy".into());
        fs::create_dir_all(&store.dir).unwrap();
        // Written by a version that never created lock files.
        fs::write(store.path_for(&id), serde_json::to_vec(&notes("legacy")).unwrap()).unwrap();
        if !make_store_dir_read_only(&store) {
            return;
        }
        let loaded = store.load(&id);
        set_store_dir_mode(&store, 0o755);
        assert_eq!(loaded.review, notes("legacy"));
        assert_eq!(loaded.left_in_place, None);
        assert_eq!(store.save(&id, &notes("upgraded"), &loaded.version).unwrap(), SaveOutcome::Saved);
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
        let message = save_current(&store, &id, &Review::default()).unwrap_err().to_string();
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
        save_current(&store, &id, &r).unwrap();
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
        let worktree =
            |branch: Option<&str>| Worktree { path: "wt".into(), head: "abc".into(), branch: branch.map(Into::into) };
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
