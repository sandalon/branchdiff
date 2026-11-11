//! A plain-text log file for troubleshooting. The terminal belongs to the UI,
//! so nothing can be printed while it runs; this is where errors, slow git
//! commands, and timings go instead. `branchdiff --log-path` prints where it is.
//!
//! Logging is best effort: a log that can't be opened or written is silently
//! skipped, and never stops the program.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A log grows to this size before it is moved aside to `<name>.old`, which
/// replaces the previous one, so at most twice this is ever kept.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Overrides the log file's location.
pub const LOG_FILE_ENV: &str = "BRANCHDIFF_LOG_FILE";

/// A git command that takes at least this long is logged even without `--verbose`.
pub const SLOW_COMMAND: Duration = Duration::from_secs(1);

struct Log {
    path: PathBuf,
    file: File,
    written: u64,
}

static LOG: OnceLock<Mutex<Log>> = OnceLock::new();
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Where the log goes: `$BRANCHDIFF_LOG_FILE`, or the platform's usual place
/// for an application's logs.
pub fn default_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(LOG_FILE_ENV).filter(|p| !p.is_empty()) {
        return Some(path.into());
    }
    platform_dir().map(|dir| dir.join("branchdiff").join("branchdiff.log"))
}

#[cfg(not(target_os = "macos"))]
fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute())
}

#[cfg(windows)]
fn platform_dir() -> Option<PathBuf> {
    env_dir("LOCALAPPDATA").or_else(|| std::env::home_dir().map(|h| h.join("AppData").join("Local")))
}

#[cfg(target_os = "macos")]
fn platform_dir() -> Option<PathBuf> {
    std::env::home_dir().map(|h| h.join("Library").join("Logs"))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn platform_dir() -> Option<PathBuf> {
    env_dir("XDG_STATE_HOME").or_else(|| std::env::home_dir().map(|h| h.join(".local").join("state")))
}

/// Opens the log at `path` for appending. With `verbose`, every git command
/// is logged, not only slow and failed ones. Only the first call has effect.
pub fn init(path: &Path, verbose: bool) -> std::io::Result<()> {
    VERBOSE.store(verbose, Ordering::Relaxed);
    if LOG.get().is_some() {
        return Ok(());
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= MAX_LOG_BYTES) {
        rotate(path);
    }
    let file = open(path)?;
    let written = file.metadata().map_or(0, |m| m.len());
    let _ = LOG.set(Mutex::new(Log { path: path.to_path_buf(), file, written }));
    Ok(())
}

/// Logs carry repository paths and branch names, so only the owner can read them.
fn open(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

fn rotate(path: &Path) {
    let mut old = path.as_os_str().to_owned();
    old.push(".old");
    let _ = std::fs::rename(path, old);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Whether a log is open, so callers can skip building messages for nothing.
pub fn enabled() -> bool {
    LOG.get().is_some()
}

/// Writes one line. Use the `log!` macro rather than calling this.
pub fn write(args: fmt::Arguments<'_>) {
    let Some(log) = LOG.get() else { return };
    let thread = std::thread::current();
    let line = format!(
        "{} [{}] {}: {}\n",
        timestamp(SystemTime::now()),
        std::process::id(),
        thread.name().unwrap_or("thread"),
        args
    );
    let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
    if log.written + line.len() as u64 > MAX_LOG_BYTES {
        rotate(&log.path);
        if let Ok(file) = open(&log.path) {
            log.file = file;
            log.written = 0;
        }
    }
    if log.file.write_all(line.as_bytes()).is_ok() {
        log.written += line.len() as u64;
    }
}

/// Writes a line to the log, if one is open, formatted like `format!`.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        if $crate::logging::enabled() {
            $crate::logging::write(format_args!($($arg)*));
        }
    };
}

/// An RFC 3339 UTC timestamp with milliseconds.
fn timestamp(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since.subsec_millis()
    )
}

/// Days since 1970-01-01 to a Gregorian date, after Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_utc_dates() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(1_709_210_096_789); // A leap day.
        assert_eq!(timestamp(t), "2024-02-29T12:34:56.789Z");
        let t = UNIX_EPOCH + Duration::from_secs(1_798_761_599); // The last second of 2026.
        assert_eq!(timestamp(t), "2026-12-31T23:59:59.000Z");
    }

    #[test]
    fn rotation_keeps_one_previous_log() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("b.log");
        std::fs::write(&path, "first").unwrap();
        rotate(&path);
        std::fs::write(&path, "second").unwrap();
        rotate(&path);
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(dir.path().join("b.log.old")).unwrap(), "second");
    }
}
