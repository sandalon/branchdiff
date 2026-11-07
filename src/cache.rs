use crate::diff::FileDiff;
use std::path::PathBuf;
use std::sync::Arc;

pub type Diffs = Arc<Vec<FileDiff>>;

/// Identifies the inputs of a loaded diff. Results for an older key are stale.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub worktree: PathBuf,
    pub base_commit: String,
    pub head: String,
    pub fingerprint: u64,
}

/// Budget for retained diff models and syntax highlights across all worktrees.
/// Worker scratch buffers and allocator bookkeeping are additional bounded costs.
pub const MAX_CACHED_BYTES: usize = 128 * 1024 * 1024;

pub fn diff_bytes(files: &[FileDiff], capacity: usize) -> usize {
    files.iter().fold(capacity.saturating_mul(std::mem::size_of::<FileDiff>()), |total, f| {
        total
            .saturating_add(f.path.capacity())
            .saturating_add(f.old_path.as_ref().map_or(0, String::capacity))
            .saturating_add(f.content_id.as_ref().map_or(0, String::capacity))
            .saturating_add(f.hunks.capacity() * std::mem::size_of::<crate::diff::Hunk>())
            .saturating_add(
                f.hunks
                    .iter()
                    .map(|h| {
                        h.header.capacity()
                            + h.lines.capacity() * std::mem::size_of::<crate::diff::Line>()
                            + h.lines.iter().map(|l| l.text.capacity()).sum::<usize>()
                    })
                    .sum::<usize>(),
            )
    })
}
