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
