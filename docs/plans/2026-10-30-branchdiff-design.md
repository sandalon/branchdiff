# branchdiff Design

**Date:** 2026-10-30
**Status:** Approved

## Purpose

`branchdiff` is a cross-platform terminal UI for reviewing git worktrees against a base branch, typically `develop`, just before opening a PR.

The main use case is supervising several AI agents that each work on a feature in their own worktree. The tool must make it quick to move between worktrees, review each one, and hand feedback back to the agent.

## Requirements

- Runs as a full-screen terminal UI on macOS, Linux and Windows.
- Compares a worktree against a base branch.
- Switches worktrees at runtime, from any screen, without restarting.
- Keeps the UI fast at all times.
- Lets the reviewer leave line comments and export them as a prompt for an agent.
- Never modifies a worktree.

## Stack

- **Language:** Rust
- **TUI:** ratatui
- **File watching:** `notify` crate
- **Git access:** the `git` CLI

The tool ships as a single binary.

## Architecture

Run `branchdiff` from anywhere inside a repo. It discovers worktrees with `git worktree list --porcelain`. The base branch defaults to `develop` and can be set with `--base` or a per-repo value, `base`, in `<git-common-dir>/branchdiff/config.toml`.

The core rule is that **the UI thread never waits on git.**

- **UI loop:** handles input and draws from in-memory state only. Every keypress gets an immediate response, even while data is loading. Two short exceptions: saving a review is synchronous, so an edit is on disk before the next key is handled, and loading a review file happens as the worktree list is applied. Startup runs `git rev-parse` before the first frame, and `e` reads `core.editor` just before handing over the terminal.
- **Git workers:** a background thread pool runs git and sends results to the UI over channels. Each worktree has at most one load running. A request made during a load runs next, and the finished result is still shown unless the base branch changed in the meantime. One worktree list runs at a time, and one probe per worktree.
- **File watcher:** watches each worktree and debounces bursts of edits. A report doesn't raise the badge by itself: a probe job compares HEAD and a `git status` fingerprint with the diff on screen, so a tool that only rewrites the index changes nothing. A change seen during a load is probed once that load lands. It also watches `.git/worktrees`, so worktrees created or removed during a session update the list.

### Git access

Workers call the `git` CLI rather than libgit2 or gitoxide. Diffs then match git's own output, user config and `.gitattributes` are respected, and there is no native library to package. Each call costs a process, so results are cached.

### Cache

Each worktree holds one diff in memory, with a key of the base commit, its HEAD and a fingerprint of its working tree state. Every worktree's full diff loads in the background at startup, so switching is just a redraw.

## User interface

There is one main screen with three areas:

```
┌ Worktrees ──────────┬ Files ───────┬ Diff ──────────────────┐
│ ▸ feat-auth  +412 ● │ M src/a.rs   │ @@ -10,6 +10,9 @@      │
│   fix-cache   +38   │ A src/b.rs   │  fn load() {           │
│   feat-api   +903   │ D old.rs     │ +    retry();          │
└─────────────────────┴──────────────┴────────────────────────┘
 base: develop   feat-auth   3 files   2 comments   ● changed
```

Worktree rows show lines added, a "changed" badge and a comment count. The diff area shows a placeholder while data loads and never blocks input.

Each worktree remembers its selected file, scroll position and unexported comments, so switching back returns to the same place.

When a worktree changes during review, it gets a "changed" badge. The diff does not redraw until the user refreshes, so nothing moves under the cursor. The badge also shows after a base change, until that worktree's diff has reloaded. It stays on after an edit is reverted, until `r`. A change that only moves the merge base doesn't raise it.

### Keybindings

| Key | Action |
| --- | --- |
| `[` / `]` | Previous or next worktree, from anywhere |
| `1`–`9` | Jump to a worktree |
| `w` | Fuzzy-find a worktree |
| `Tab` | Move between areas |
| `j` / `k` | Move and scroll |
| `n` / `p` | Next or previous file |
| `v` | Toggle unified and side-by-side diffs |
| `c` | Comment on the current line |
| `d` | Delete the comment on the current line |
| `D` | Clear all comments for this worktree, after a confirmation |
| `e` | Open the file and line in your editor, chosen in git's order: `GIT_EDITOR`, `core.editor`, `VISUAL`, `EDITOR`, then a platform default |
| `x` | Mark a file as reviewed |
| `y` | Copy comments to the clipboard as a Markdown prompt, or write the file if there's no clipboard |
| `Y` | Write comments to `<git-common-dir>/branchdiff/exports/latest.md` |
| `r` | Refresh this worktree and the worktree list, or retry a failed command |
| `b` | Change the base branch for this session |
| `?` | Open or close the help overlay |
| `Esc` | Close the help overlay or a prompt |
| `q` / `Ctrl-C` | Quit. Ctrl-C also quits from a prompt |

`?` works everywhere except while typing a comment, when it is entered as text.

## Data flow

Loading a worktree:

1. Run `git merge-base <base> HEAD` in the worktree to find the fork point.
2. Read HEAD and the working tree fingerprint, before the diff, so a change during the load shows as a difference.
3. Run `git diff <merge-base>` to get the summary and per-file diffs. This covers committed and uncommitted changes.
4. Run `git ls-files --others --exclude-standard` to find untracked files, and show them as new files.
5. Send the results to the UI, which keeps them in memory with the key that identifies their inputs.

The default diff is measured from the fork point, not the base branch tip, so changes merged to the base after forking do not appear in the review.

Very large diffs render only the visible lines. Binary files show a one-line notice.

## Comments and reviewed marks

Comments and reviewed marks are stored as JSON under `<git-common-dir>/branchdiff/`, in one file per branch with the branch name encoded into the filename.

- Git does not track this location, so comments cannot be committed by accident.
- Keying by branch means they survive a worktree being removed and recreated.
- Writes go to a temp file and are then renamed, so a crash cannot corrupt them.

Each comment records its file, line and line text. A comment is flagged as outdated if that code changes. A reviewed mark resets if the file's contents change.

Lines with comments get a marker in the diff, and the status bar shows the comment on the current line. `d` deletes it, and `D` clears the worktree's comments once the agent has handled them.

Export produces a Markdown prompt that can be pasted into any agent, which avoids integrating with a specific one.

## Error handling

| Situation | Behaviour |
| --- | --- |
| Not in a repo, or `git` missing | Exit with a clear message before the UI starts |
| Base branch missing | Open the UI and show the base picker |
| No merge base | Show the failure on that worktree's row; other worktrees keep working |
| Detached HEAD | Show the diff normally; key comments by commit, or by the branch being rebased during a rebase |
| Worktree deleted while selected | Show a notice and move to the next worktree; keep saved comments on disk |
| Mid-rebase or merge | Show a warning badge, since the diff may look unusual |
| Git command fails | Show stderr in the diff area; `r` retries |
| No terminal | Exit with a clear message |
| Config file unusable | Ignore it and show a notice |
| File watching unavailable or at its limit | Show a notice; the badge may be incomplete |
| Review file unreadable | Move it aside and show a notice |
| Editor fails or won't start | Show a notice |
| SIGTERM or SIGHUP | Restore the terminal and exit once any editor has finished |

## Testing

- **Git layer:** integration tests build real temporary repos with worktrees and cover uncommitted edits, untracked files, renames, binary files, detached HEAD and no merge base.
- **Parsing and state:** unit tests cover diff parsing, cache keys, dropping late results and flagging outdated comments. A test confirms an interrupted save leaves the old file intact.
- **UI:** snapshot tests render screens to ratatui's test backend and check them with `insta`. Key-driven tests cover switching, per-worktree position and the help overlay.
- **Platforms:** CI runs everything on macOS, Linux and Windows.
- **Speed:** tests render an in-memory app with 10 worktrees and a 5,000-file diff, and a benchmark parses a patch of about 100 MB. CI fails if these targets slip:

| Measure | Target |
| --- | --- |
| First usable frame | Under 100 ms |
| Switch to a cached worktree | Under 16 ms |
| Keypress to redraw | Under 16 ms |

## First release scope

**Included:**

- Live worktree list, runtime switching and per-worktree position
- Unified and side-by-side diffs with syntax highlighting
- Line comments with diff markers and deletion, Markdown export and jump-to-editor
- Reviewed marks and file watching with the "changed" badge
- Runtime base branch switching and the `?` help overlay
- Prebuilt binaries for macOS, Linux and Windows

**Left out:**

- Staging, reverting or any other edits to worktrees
- Creating PRs or integrating with GitHub
- Sending comments directly to agents
- Viewing multiple repos at once
- Custom keybindings and themes

The only config in the first release is the per-repo base branch, in `<git-common-dir>/branchdiff/config.toml`.
