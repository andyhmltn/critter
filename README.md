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
```

The reviewer uses the repository in the current directory and opens with a picker for its open pull requests. Pass a PR number (for example, `reviewer 447`) to skip the picker. After choosing a PR, it opens on the changed-file selector, renders unified diffs with line numbers and syntax-aware code colors, and batches inline comments into one GitHub review.

## Keys

| Key | Action |
| --- | --- |
| `j` / `k` | Move through files or change blocks; move by line after entering a block |
| `Ctrl-d` / `Ctrl-u` | Page down or up in the focused pane |
| `h` / `l` | Previous or next changed file |
| `Enter` | Open a file, enter a change block, or comment on the selected line |
| `v` | Mark the current file as viewed (press again to undo) |
| `Esc` | Leave line mode, then return from the diff to the file sidebar |
| `/` | Search changed lines across files, with incremental highlights in the diff |
| `n` / `N` | Jump to the next or previous search result |
| `Tab` | Switch between files and diff panes |
| `Ctrl+Enter` | Add a newline while writing a comment |
| `d` | Read the PR description |
| `o` | Open the selected head-version file at the selected line in `$EDITOR` |
| `c` | Show and remove pending comments |
| `s` | Submit as approve, request changes, or comment, with an optional summary |
| `b` | Return to the pull-request picker |
| `q` | Quit without submitting |

## gh-dash keybinding

```yaml
- key: R
  name: review
  command: reviewer {{.PrNumber}}
```

Comments are only submitted when an action is chosen from the submit dialog. Quit never posts a review.

Comment and command inputs support Vim-style normal-mode editing, including `cc`, `dd`, `cw`, `dw`, `ciw`, and `diw`.

Viewed-file progress is saved locally and restored the next time the same pull request is opened. It resets automatically when the pull request head commit changes.
