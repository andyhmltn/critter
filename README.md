# [reviewer](reviewer)

Interactive GitHub pull-request review in the terminal. It keeps GitHub's review model, while making navigation feel native in a keyboard-driven terminal.

## Install

```sh
cargo install --path .
```

Requires: Rust 1.85+ and an authenticated `gh` CLI. `nvim` is used by default when opening a file, or set `EDITOR` to override it.

## Use

```sh
reviewer
reviewer --repo owner/name
reviewer --repo owner/name 447
```

By default, the reviewer uses the repository in the current directory. Use `--repo owner/name` (or `-R owner/name`) to browse a GitHub repository without cloning it. Pass a PR number (for example, `reviewer 447` or `reviewer -R owner/name 447`) to skip the picker. After choosing a PR, it opens on the changed-file selector, renders unified diffs with line numbers and syntax-aware code colors, and batches inline comments into one GitHub review.

## Navigation model

The main review flow is hierarchical:

1. The pull-request picker lists matching PRs.
2. `Enter` opens the sidebar view with the PR-description preview and changed files.
3. `Enter` on a file opens the full-width diff.
4. `Esc` moves back through line mode, block mode, the sidebar, and finally the PR picker.

The description sits immediately above the file list. Press `k` on the first file to focus and expand it; press `j` to collapse it and return to the first file. `Tab` cycles between files, description, and diff, while `d` opens the expanded description directly. Use `Ctrl-d` and `Ctrl-u` to scroll a long description.

## Pull-request picker and search

The picker starts with the GitHub query `is:open`. Press `/` to focus the query and submit it with `Enter`. Searches are sent to GitHub, so free text and qualifiers such as `author:`, `label:`, `review:`, and `is:` use GitHub's normal semantics.

Typing `author:` offers repository contributors and authors from the loaded PRs. Use ↑/↓ to choose a suggestion and `Tab` to complete it. The active query and results remain in place after opening and returning from a PR.

## Reviewing diffs

Diff navigation starts in block mode. `j` and `k` jump between contiguous change blocks, which are marked by a cyan rail. `Enter` comments on the block's first line immediately. Use `Shift+Enter` when you want line mode, where `j` and `k` choose a different line and `Enter` starts the comment. `zz` centers the block midpoint, or the selected line in line mode.

Inline comments remain local until a review is submitted with `s`. Files can be marked viewed with `v`; that progress is saved locally per repository, PR, and head commit. Changed lines can be searched incrementally with `/`, then revisited with `n` and `N`.

## Keys

| Key | Action |
| --- | --- |
| `j` / `k` | Move through files or change blocks; move by line after entering a block |
| `Ctrl-d` / `Ctrl-u` | Page down or up in the focused pane |
| `zz` | Center the current change block or selected line |
| `h` / `l` | Previous or next changed file |
| `Enter` | Move forward from PR picker → sidebar → full diff, or comment on the first block line |
| `Shift+Enter` | Enter line-selection mode for the current change block |
| `v` | Mark the current file as viewed (press again to undo) |
| `Esc` | Move back through line mode → full diff → sidebar → PR picker |
| `/` | Search changed lines across files, with incremental highlights in the diff |
| `n` / `N` | Jump to the next or previous search result |
| `Tab` | Cycle focus through files, PR description, and diff panes |
| `Ctrl+Enter` | Add a newline while writing a comment |
| `d` | Focus and expand the PR description in the sidebar |
| `o` | Open the selected head-version file at the selected line in `$EDITOR` |
| `c` | Show and remove pending comments |
| `s` | Submit as approve, request changes, or comment, with an optional summary |
| `:` | Open the command panel (`:q` safely quits; `:q!` forces it) |
| `q` | Quit when no review comments are pending |

## Command and text input

Press `:` from the review to open the command panel. `:q` refuses to quit and displays a warning while unsubmitted review comments exist. Use `:q!` to explicitly discard those comments and quit anyway. Direct `q` and returning to the PR picker retain the safe behavior.

Search, comment, summary, and command inputs use the `vimltui` editing engine with insert, normal, replace, and visual modes. Press `Esc` once to enter normal mode and again to cancel the input; a blank comment closes immediately on the first `Esc`. Vim operator–motion composition, counts, registers, text objects, character-find motions, undo/redo, and dot-repeat are supported—for example `c$`, `d2w`, `ciw`, `ci"`, `f<char>`, `u`, and `.`. In multiline comment inputs, `Ctrl+Enter` inserts a newline.

Comments are posted only after choosing approve, request changes, or comment from the submit dialog. Quitting never posts a review automatically.

## gh-dash keybinding

```yaml
- key: R
  name: review
  command: reviewer {{.PrNumber}}
```
