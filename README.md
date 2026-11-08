# branchdiff

A fast cross-platform terminal UI for reviewing git worktrees against a base branch, typically `develop`, just before opening a PR. It's built for supervising several AI agents that each work on a feature in their own worktree: move between worktrees quickly, review each one, and copy your comments back as a Markdown prompt.

What it does not do: it never modifies a worktree. It reads git, renders diffs, and stores your comments and reviewed marks under the git common dir, away from the files you are reviewing.

The worktree pane is ordered by recent activity, with the newest HEAD commit or uncommitted file edit first.

## Install

Download a binary for your platform from the latest release, or build from source:

```sh
cargo install --path .
```

Releases produced by this workflow list every binary's SHA-256 in `SHA256SUMS`. Download it alongside your binary, then verify the checksum:

```sh
sha256sum --check --ignore-missing SHA256SUMS
```

Public repositories also publish build provenance attestations. Private repositories require GitHub Enterprise Cloud and the repository Actions variable `ENABLE_PRIVATE_ATTESTATIONS=true` to enable them. When an attestation is available, verify it with:

```sh
gh attestation verify branchdiff-<target> --repo sandalon/branchdiff
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
| `Page Up` / `Page Down` | scroll the diff pane one page |
| `n` / `p` | next or previous file |
| `v` | unified or side-by-side diff |
| `Shift+W` | toggle word wrap in the diff pane |
| `f` | toggle the focused pane full screen |
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

- Files marked reviewed in earlier versions need to be marked again after upgrading; comments are retained. New marks also track permission and rename changes.
- On Linux without a clipboard manager, copied text is lost when `branchdiff` quits.
- `e` picks the editor in git's order (`GIT_EDITOR`, `core.editor`, `VISUAL`, `EDITOR`, then `vi`, or `notepad` on Windows) but runs it without a shell, so variables in the value aren't expanded. On Windows, VS Code may need `EDITOR=code.cmd`. As with `git commit`, a `core.editor` set in the repository's own `.git/config` is honored, so pressing `e` in a repository you didn't create runs whatever program it names; set `GIT_EDITOR` to override it. Branchdiff's own git commands ignore `core.fsmonitor`, so merely opening a repository never runs its hooks.
- A commit inside a real submodule may not show the "changed" badge until `r` is pressed.
- The "changed" badge stays on after an edit is reverted, until `r` is pressed. A change that only moves the merge base doesn't raise it.
- On Linux, a low inotify limit makes the badge incomplete and shows a notice.
- On macOS, a worktree reached through a symlink created after it was added won't show the badge for file edits.
- On Windows, deleting a worktree's directory by hand isn't noticed until the list next refreshes.
- Concurrent sessions and detached worktrees on the same commit check the saved review's revision before writing. A conflicting save keeps the latest shared review intact and writes your complete local review to a private `*.json.conflict-*.json` recovery file beside it. The notice gives its path; you can keep editing or export your local notes, then restart to load the latest shared review. Repeated conflicting saves retain separate recovery files. All concurrent sessions must use a version with revision checks.
- Colours assume a dark background, so context rows can be hard to read on a light terminal.

Large diffs are limited to an 8 MiB tracked patch and a 64 MiB estimated diff
model per worktree. A patch that exceeds these limits is rejected in full with
an error; partial changes cannot be marked reviewed. Use `git diff` to review
such a patch, or reduce its size and press `r`. Untracked reads have a separate
32 MiB aggregate budget, with oversized files listed as unavailable.

Loaded diffs and syntax colours share a 128 MiB estimated cache budget. Older
worktrees are evicted and reloaded when selected; comments, reviewed marks and
navigation positions are retained. Evicted worktrees show an unknown (`+?`)
change count until reloaded. These budgets cover application-owned
models, not Git's own memory or allocator overhead. Worker buffers and queued
results are bounded separately.
