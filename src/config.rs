use serde::Deserialize;
use std::io::ErrorKind;
use std::path::Path;

/// Where the per-repository config lives, relative to the git common dir.
const FILE: &str = "branchdiff/config.toml";

#[derive(Deserialize)]
struct Config {
    base: Option<String>,
}

/// Reads `base` from the config. A missing file or a blank value is no setting. `Err` holds
/// a one-line notice for a file that exists but can't be used.
pub fn configured_base(common_dir: &Path) -> Result<Option<String>, String> {
    let notice = |e: &dyn std::fmt::Display| {
        let text = e.to_string();
        format!("Ignored {FILE}: {}", text.lines().next().unwrap_or_default())
    };
    let text = match std::fs::read_to_string(common_dir.join(FILE)) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(notice(&e)),
    };
    let config: Config = toml::from_str(&text).map_err(|e| notice(&e))?;
    Ok(config.base.filter(|base| !base.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A common dir whose config holds `content`.
    fn with_config(content: &[u8]) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("branchdiff")).unwrap();
        std::fs::write(dir.path().join(FILE), content).unwrap();
        dir
    }

    #[test]
    fn a_missing_file_sets_nothing_and_is_no_problem() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(configured_base(dir.path()), Ok(None));
    }

    #[test]
    fn base_is_read_from_the_file() {
        let dir = with_config(b"base = \"main\"\n");
        assert_eq!(configured_base(dir.path()), Ok(Some("main".to_string())));
    }

    #[test]
    fn a_file_without_base_sets_nothing() {
        let dir = with_config(b"# nothing yet\n");
        assert_eq!(configured_base(dir.path()), Ok(None));
    }

    #[test]
    fn a_blank_base_sets_nothing() {
        for text in ["base = \"\"\n", "base = \"   \"\n", "base = \"\\t\\n\"\n"] {
            let dir = with_config(text.as_bytes());
            assert_eq!(configured_base(dir.path()), Ok(None), "{text}");
        }
    }

    #[test]
    fn invalid_toml_gives_a_one_line_notice_naming_the_file() {
        let notice = configured_base(with_config(b"base = [broken\n").path()).unwrap_err();
        assert!(notice.contains(FILE), "{notice}");
        assert!(notice.contains("TOML parse error"), "{notice}");
        assert!(!notice.contains('\n'), "{notice}");
    }

    #[test]
    fn a_base_of_the_wrong_type_gives_a_notice() {
        assert!(configured_base(with_config(b"base = 1\n").path()).is_err());
    }

    #[test]
    fn a_file_that_is_not_text_gives_a_notice() {
        let notice = configured_base(with_config(&[0xff, 0xfe]).path()).unwrap_err();
        assert!(notice.contains(FILE), "{notice}");
    }

    #[test]
    fn an_unreadable_file_gives_a_notice() {
        // A directory can't be read as a file, whoever runs the test.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(FILE)).unwrap();
        let notice = configured_base(dir.path()).unwrap_err();
        assert!(notice.contains(FILE), "{notice}");
    }
}
