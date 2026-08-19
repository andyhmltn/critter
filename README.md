# [reviewer](reviewer)

Interactive GitHub pull-request review in the terminal. It keeps GitHub's review model, while making navigation feel native in a keyboard-driven terminal.

## Install

```sh
cargo install --path .
```

Requires: Rust 1.85+ and an authenticated `gh` CLI. `nvim` is used by default when opening a file, or set `EDITOR` to override it.

## Use

```sh
reviewer 447 dlvhdr/gh-dash
```

The reviewer opens on the changed-file selector, renders unified diffs with line numbers and syntax-aware code colors, and batches inline comments into one GitHub review.

## Keys

| Key | Action |
| --- | --- |
| `j` / `k` | Move through the focused file list or diff |
| `Ctrl-d` / `Ctrl-u` | Page down or up in the focused pane |
| `h` / `l` | Previous or next changed file |
| `Enter` | Open the selected file from the sidebar, or add an inline comment from a diff |
| `Esc` | Return from the diff to the file sidebar |
| `/` | Search changed lines across files, with incremental highlights in the diff |
| `n` / `N` | Jump to the next or previous search result |
| `Tab` | Switch between files and diff panes |
| `Ctrl+Enter` | Add a newline while writing a comment |
| `d` | Read the PR description |
| `o` | Open the selected head-version file at the selected line in `$EDITOR` |
| `c` | Show and remove pending comments |
| `s` | Submit as approve, request changes, or comment, with an optional summary |
| `q` | Quit without submitting |

## gh-dash keybinding

```yaml
- key: R
  name: review
  command: reviewer {{.PrNumber}} {{.RepoName}}
```

Comments are only submitted when an action is chosen from the submit dialog. Quit never posts a review.
