use crate::diff::FileDiff;

#[derive(Debug, Clone, Default)]
pub struct HighlightedFile;

pub fn highlight_file(_file: &FileDiff) -> HighlightedFile {
    HighlightedFile
}
