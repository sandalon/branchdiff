# branchdiff

A fast cross-platform terminal UI for reviewing git worktrees against a base branch, typically `develop`, just before opening a PR. It's built for supervising several AI agents that each work on a feature in their own worktree: move between worktrees quickly, review each one, and copy your comments back as a Markdown prompt.

What it does not do: it never modifies a worktree. It reads git, renders diffs, and stores your comments and reviewed marks under the git common dir, away from the files you are reviewing.

## Install

Download a binary for your platform from the latest release, or build from source:

```sh
cargo install --path .
```

## Usage

From anywhere inside a repo:

```sh
branchdiff
branchdiff --base main
```

The base branch defaults to `develop`. Set a per-repo default with a config file at `<git-common-dir>/branchdiff/config.toml`:

```toml
base = "main"
```

A blank `base` counts as unset, and an unusable config file is ignored with a notice. The base can also be changed for a session with `b`.

## Keys

| Key | Action |
| --- | --- |
| `[` / `]` | previous or next worktree |
| `1`–`9` | jump to a worktree |
| `w` | fuzzy find a worktree |
| `Tab` / `Right` | next area |
| `Shift+Tab` / `Left` | previous area |
| `j` / `k` | move and scroll |
| `n` / `p` | next or previous file |
| `v` | unified or side-by-side diff |
| `f` | toggle the diff pane full screen |
| `c` | comment on the current line |
| `d` / `D` | delete a comment, or clear all |
| `e` | open in $EDITOR |
| `x` | mark file as reviewed |
| `y` / `Y` | copy comments, or write to a file |
| `r` | refresh or retry |
| `b` | change the base branch |
| `?` | open or close this help |
| `Esc` | close help or a prompt |
| `q` | quit |

`Ctrl-C` also quits (except on Windows while an editor is open; in that case it isn't handled).

Comments and reviewed marks are stored as JSON under `<git-common-dir>/branchdiff/`, one file per branch, so they survive a worktree being removed and recreated — and they never modify a worktree. Review files are private to their owner, so another user sharing the same `.git` sees theirs moved aside.

## Notes

- On Linux without a clipboard manager, copied text is lost when `branchdiff` quits.
- `e` picks the editor in git's order (`GIT_EDITOR`, `core.editor`, `VISUAL`, `EDITOR`, then `vi`, or `notepad` on Windows) but runs it without a shell, so variables in the value aren't expanded. On Windows, VS Code may need `EDITOR=code.cmd`.
- A commit inside a real submodule may not show the "changed" badge until `r` is pressed.
- The "changed" badge stays on after an edit is reverted, until `r` is pressed. A change that only moves the merge base doesn't raise it.
- On Linux, a low inotify limit makes the badge incomplete and shows a notice.
- On macOS, a worktree reached through a symlink created after it was added won't show the badge for file edits.
- On Windows, deleting a worktree's directory by hand isn't noticed until the list next refreshes.
- Two `branchdiff` sessions reviewing the same branch, or two detached worktrees on the same commit, overwrite each other's comments when they save.
- Colours assume a dark background, so context rows can be hard to read on a light terminal.
