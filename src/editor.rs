use std::path::Path;

const FALLBACK: &str = if cfg!(windows) { "notepad" } else { "vi" };

pub fn default_editor() -> String {
    pick(|name| std::env::var(name).ok())
}

/// `VISUAL` wins, as it does for git. Unlike git, a value that names no program
/// counts as unset.
fn pick(var: impl Fn(&str) -> Option<String>) -> String {
    let set = |name| var(name).filter(|value| split(value).is_some());
    set("VISUAL").or_else(|| set("EDITOR")).unwrap_or_else(|| FALLBACK.into())
}

/// Splits `editor` at whitespace, keeping quoted text together and, outside
/// Windows, a character after a backslash, so that `"/opt/My Ed/ed" -w` works.
/// git would run the value in a shell. This expands nothing, so that no shell
/// ever sees a file's name, and a missing editor is an error rather than exit 127.
fn words(editor: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote = None;
    let mut chars = editor.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => word.get_or_insert_default().push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                word.get_or_insert_default();
            }
            (None, '\\') if !cfg!(windows) => {
                if let Some(next) = chars.next() {
                    word.get_or_insert_default().push(next);
                }
            }
            (None, _) if c.is_whitespace() => words.extend(word.take()),
            (None, _) => word.get_or_insert_default().push(c),
        }
    }
    words.extend(word);
    words
}

/// The program and arguments in `editor`, or `None` if it names no program, in
/// which case any arguments belonged to nothing and go with it.
fn split(editor: &str) -> Option<(String, Vec<String>)> {
    let mut words = words(editor).into_iter();
    let prog = words.next().filter(|prog| !prog.trim().is_empty())?;
    Some((prog, words.collect()))
}

/// Builds the program and arguments that open `path` at `line`. `path` is
/// absolute, so even a file named `-x` can't be taken for an option.
pub fn command(editor: &str, path: &Path, line: u32) -> (String, Vec<String>) {
    let (prog, mut args) = split(editor).unwrap_or_else(|| (FALLBACK.into(), Vec::new()));
    let name = Path::new(&prog).file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    let file = path.display().to_string();
    let at_line = format!("{file}:{line}");
    match name.as_str() {
        "vi" | "vim" | "nvim" | "nano" | "emacs" | "emacsclient" | "kak" | "micro" => {
            args.extend([format!("+{line}"), file]);
        }
        "code" | "code-insiders" | "cursor" | "windsurf" | "codium" => {
            args.extend(["--wait".to_string(), "--goto".to_string(), at_line]);
        }
        "subl" | "sublime_text" | "zed" => args.extend(["--wait".to_string(), at_line]),
        "hx" | "helix" => args.push(at_line),
        "gedit" => args.extend([file, format!("+{line}")]),
        "idea" => args.extend(["--line".to_string(), line.to_string(), file]),
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

    #[test]
    fn other_editors_get_their_own_form_of_the_line() {
        assert_eq!(args("emacs").1, vec!["+7", "/r/a.rs"]);
        assert_eq!(args("emacsclient").1, vec!["+7", "/r/a.rs"]);
        assert_eq!(args("kak").1, vec!["+7", "/r/a.rs"]);
        assert_eq!(args("micro").1, vec!["+7", "/r/a.rs"]);
        assert_eq!(args("gedit").1, vec!["/r/a.rs", "+7"]);
        assert_eq!(args("sublime_text").1, vec!["--wait", "/r/a.rs:7"]);
        assert_eq!(args("zed").1, vec!["--wait", "/r/a.rs:7"]);
        assert_eq!(args("idea").1, vec!["--line", "7", "/r/a.rs"]);
    }

    #[test]
    fn quotes_keep_a_path_with_spaces_together() {
        let subl = "/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl";
        for editor in [format!("\"{subl}\" -w"), format!("'{subl}' -w")] {
            let (prog, a) = args(&editor);
            assert_eq!(prog, subl);
            assert_eq!(a, vec!["-w", "--wait", "/r/a.rs:7"]);
        }
        assert_eq!(args("vim -c 'set ft=rust'").1, vec!["-c", "set ft=rust", "+7", "/r/a.rs"]);
    }

    #[cfg(not(windows))]
    #[test]
    fn a_backslash_keeps_a_space_in_the_word() {
        assert_eq!(args("/opt/My\\ Editor/vim").0, "/opt/My Editor/vim");
    }

    #[cfg(windows)]
    #[test]
    fn a_backslash_is_a_path_separator_on_windows() {
        assert_eq!(args(r#""C:\Program Files\Vim\gvim.exe" -f"#).0, r"C:\Program Files\Vim\gvim.exe");
    }

    fn picked(visual: &str, editor: &str) -> String {
        pick(|name| Some(if name == "VISUAL" { visual } else { editor }.to_string()))
    }

    #[test]
    fn a_blank_value_counts_as_unset() {
        assert_eq!(picked("hx", "nano"), "hx");
        assert_eq!(picked("", "nano"), "nano");
        assert_eq!(picked("  ", "nano"), "nano");
        assert_eq!(picked("", " "), FALLBACK);
        assert_eq!(pick(|_| None), FALLBACK);
        assert_eq!(args("  ").0, FALLBACK);
    }

    #[test]
    fn a_value_with_no_real_program_counts_as_unset() {
        let fallback = args(FALLBACK);
        for blank in ["\"\"", "''", "\"\" vim", "'' ''", "\"", "'", "\"   \"", "'   ' -w"] {
            assert_eq!(args(blank), fallback, "{blank}");
            assert_eq!(picked(blank, "nano"), "nano", "{blank}");
            assert_eq!(picked("hx", blank), "hx", "{blank}");
            assert_eq!(picked(blank, blank), FALLBACK, "{blank}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn a_trailing_backslash_adds_no_word() {
        assert_eq!(args("vim \\").1, vec!["+7", "/r/a.rs"]);
        assert_eq!(args("\\"), args(FALLBACK));
        assert_eq!(picked("\\", "nano"), "nano");
    }

    #[test]
    fn a_quote_is_closed_only_by_its_own_kind() {
        let (prog, a) = args("\"/opt/it's/vim\" -u NONE");
        assert_eq!(prog, "/opt/it's/vim");
        assert_eq!(a, vec!["-u", "NONE", "+7", "/r/a.rs"]);
        assert_eq!(args("'/opt/a\"b/vim'").0, "/opt/a\"b/vim");
    }

    #[test]
    fn an_empty_quoted_argument_is_kept() {
        assert_eq!(args("vim -u ''").1, vec!["-u", "", "+7", "/r/a.rs"]);
    }

    #[test]
    fn every_name_of_an_editor_gets_its_form_of_the_line() {
        let goto = vec!["--wait", "--goto", "/r/a.rs:7"];
        assert_eq!(args("vi").1, vec!["+7", "/r/a.rs"]);
        for editor in ["code-insiders", "windsurf", "codium"] {
            assert_eq!(args(editor).1, goto, "{editor}");
        }
        assert_eq!(args("helix").1, vec!["/r/a.rs:7"]);
    }

    #[test]
    fn the_name_ignores_case_and_an_extension() {
        assert_eq!(args("VIM").1, vec!["+7", "/r/a.rs"]);
        assert_eq!(args("vim.exe").1, vec!["+7", "/r/a.rs"]);
        assert_eq!(args("Code.exe").1, vec!["--wait", "--goto", "/r/a.rs:7"]);
    }
}
