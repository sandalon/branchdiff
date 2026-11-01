use std::path::Path;

pub fn default_editor() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| if cfg!(windows) { "notepad".into() } else { "vi".into() })
}

/// Builds the program and arguments that open `path` at `line`.
pub fn command(editor: &str, path: &Path, line: u32) -> (String, Vec<String>) {
    let mut parts = editor.split_whitespace().map(String::from);
    let prog = parts.next().unwrap_or_else(|| "vi".into());
    let mut args: Vec<String> = parts.collect();
    let name = Path::new(&prog).file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    let file = path.display().to_string();
    let at_line = format!("{file}:{line}");
    match name.as_str() {
        "vi" | "vim" | "nvim" | "nano" | "emacs" | "kak" | "micro" => {
            args.extend([format!("+{line}"), file]);
        }
        "code" | "code-insiders" | "cursor" | "windsurf" | "codium" => {
            args.extend(["--wait".to_string(), "--goto".to_string(), at_line]);
        }
        "subl" => args.extend(["--wait".to_string(), at_line]),
        "hx" | "helix" => args.push(at_line),
        _ => args.push(file),
    }
    (prog, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn args(editor: &str) -> (String, Vec<String>) {
        command(editor, Path::new("/r/a.rs"), 7)
    }

    #[test]
    fn terminal_editors_use_plus_line() {
        assert_eq!(args("vim").1, vec!["+7", "/r/a.rs"]);
        assert_eq!(args("nvim").1, vec!["+7", "/r/a.rs"]);
        assert_eq!(args("nano").1, vec!["+7", "/r/a.rs"]);
    }

    #[test]
    fn vscode_family_uses_goto_and_wait() {
        assert_eq!(args("code").1, vec!["--wait", "--goto", "/r/a.rs:7"]);
        assert_eq!(args("cursor").1, vec!["--wait", "--goto", "/r/a.rs:7"]);
    }

    #[test]
    fn sublime_and_helix_use_path_colon_line() {
        assert_eq!(args("subl").1, vec!["--wait", "/r/a.rs:7"]);
        assert_eq!(args("hx").1, vec!["/r/a.rs:7"]);
    }

    #[test]
    fn keeps_existing_arguments_and_full_paths() {
        let (prog, a) = args("/usr/bin/vim -u NONE");
        assert_eq!(prog, "/usr/bin/vim");
        assert_eq!(a, vec!["-u", "NONE", "+7", "/r/a.rs"]);
    }

    #[test]
    fn unknown_editors_just_get_the_path() {
        assert_eq!(args("myed").1, vec!["/r/a.rs"]);
    }
}
