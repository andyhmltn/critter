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

## Review intelligence

Press `b` from a review to open the local review-intelligence view. It ranks large change blocks, inventories changed declarations, and surfaces likely contract, control-flow, test, and risk-sensitive changes without calling an AI service. Use `j`/`k` to select a section and `Enter` to inspect it. Changed-code evidence is underlined: use `n`/`N` to select evidence and `Enter` to jump directly to its diff; `Esc` returns to the report. The standard `:` command palette remains available.

## Navigation model

The main review flow is hierarchical:

1. The pull-request picker lists matching PRs.
2. `Enter` opens the sidebar view with the PR-description preview and changed files.
3. `Enter` on a file opens the full-width diff.
4. `Esc` moves back through line mode, block mode, the sidebar, and finally the PR picker.

The description sits immediately above the file list. Press `k` on the first file to focus and expand it; press `j` to collapse it and return to the first file. `Tab` cycles between files, description, and diff, while `d` opens the expanded description directly. Use `Ctrl-d` and `Ctrl-u` to scroll a long description.

## Pull-request picker and search

The picker starts with the last successfully submitted query for that repository, defaulting to `is:open`. Queries are saved in the user's cache independently for each `owner/repo`. Press `/` to focus the query and submit it with `Enter`. Searches are sent to GitHub, so free text and qualifiers such as `author:`, `label:`, `review:`, and `is:` use GitHub's normal semantics.

Typing `author:` offers repository contributors and authors from the loaded PRs. Use ↑/↓ to choose a suggestion and `Tab` to complete it. The active query and results remain in place after opening and returning from a PR.

## Reviewing diffs

Diff navigation starts in block mode. `j` and `k` jump between contiguous change blocks, automatically centering each block and marking it with a cyan rail. `Enter` comments on the block's first line immediately. Use `V` to enter line mode, where `j` and `k` select individual lines and `Enter` starts the comment. The selected row uses a solid neon highlight instead of a marker rail. `zz` centers the block midpoint or selected line.

Whole-file additions, whole-file deletions, and single change blocks that fill the viewport open directly in line mode at the top. Because block navigation would have only one target, this automatic line mode is locked: `Esc` returns to the sidebar instead of switching to block mode. Line mode entered manually with `V` still uses `Esc` to return to block mode.

The optional sidebar is displayed on the right. `<Space>l` opens it and focuses the file list; using the chord again hides it and returns focus to the diff.

### Logical review flow

When a PR opens, reviewer starts building a logical review plan in the background with `pi`, using OpenRouter and `deepseek/deepseek-v4-flash` by default. The file list remains selected and fully navigable while a compact animated footer reports the current analysis phase and elapsed time. To keep generation quick, the model receives a bounded changed-code manifest rather than the full raw diff. A cached plan opens immediately; a newly generated plan automatically replaces the file list when ready. Flow descriptions explain dependencies, reviewer intent, and failure modes, and wrap within the sidebar. Press `f` to toggle back to ordinary file order. Use `j`/`k` while the flow panel is focused, or `[`/`]` from the review, to step backward or forward through the plan and jump to the corresponding changed code.

Completed plans are cached per repository, PR number, and head commit under the user's cache directory, so reopening an unchanged PR is immediate and a new head commit automatically receives a fresh plan. Set `REVIEWER_PI_PROVIDER` and `REVIEWER_PI_MODEL` to override the defaults. This feature sends a compacted PR diff to the configured model provider; `pi` runs without tools, skills, extensions, or a persistent session.

Inline comments remain local until a review is submitted with `s`. Pending comments are autosaved atomically per repository, PR, and head commit, then restored when the same review is reopened after a crash or interrupted session. A successful submission removes the saved draft; comments from an older head are never mixed into the new diff. Files can be marked viewed with `v`; that progress is saved locally per repository, PR, and head commit. Changed lines can be searched incrementally with `/`, then revisited with `n` and `N`.

## Keys

| Key | Action |
| --- | --- |
| `j` / `k` | Move through files or change blocks; move by line after entering a block |
| `Ctrl-d` / `Ctrl-u` | Page down or up in the focused pane |
| `zz` | Center the current change block or selected line |
| `h` / `l` | Previous or next changed file |
| `Enter` | Move forward from PR picker → sidebar → full diff, or comment on the first block line |
| `V` | Enter line-selection mode for the current change block |
| `v` | Mark the current file as viewed (press again to undo) |
| `Esc` | Leave manual line mode, or return from a locked single-block diff to the sidebar |
| `/` | Search changed lines across files, with incremental highlights in the diff |
| `n` / `N` | Jump to the next or previous search result |
| `Tab` | Cycle focus through files, PR description, and diff panes |
| `<Space>l` | Toggle the right sidebar and move focus with it |
| `f` | Toggle the file sidebar between file order and AI logical flow |
| `[` / `]` | Jump to the previous or next logical review-flow step |
| `Ctrl+Enter` | Add a newline while writing a comment |
| `d` | Focus and expand the PR description in the sidebar |
| `b` | Toggle the local review-intelligence view |
| `o` | Open the selected head-version file at the selected line in `$EDITOR` |
| `c` | Show and remove pending comments |
| `s` | Submit as approve, request changes, or comment, with an optional summary |
| `:` | Open the command panel (`:q` safely quits; `:q!` forces it) |
| `q` | Quit when no review comments are pending |

## Command and text input

Press `:` from the review to open the command panel. `:q` refuses to quit and displays a warning while unsubmitted review comments exist. Use `:q!` to explicitly delete the saved draft and quit anyway. Direct `q` and returning to the PR picker retain the safe behavior.

Search, comment, summary, and command inputs use the `vimltui` editing engine with insert, normal, replace, and visual modes. Press `Esc` once to enter normal mode and again to cancel the input; a blank comment closes immediately on the first `Esc`. Vim operator–motion composition, counts, registers, text objects, character-find motions, undo/redo, and dot-repeat are supported—for example `c$`, `d2w`, `ciw`, `ci"`, `f<char>`, `u`, and `.`. In multiline comment inputs, `Ctrl+Enter` inserts a newline.

Comments are posted only after choosing approve, request changes, or comment from the submit dialog. Quitting never posts a review automatically.

## gh-dash keybinding

```yaml
- key: R
  name: review
  command: reviewer {{.PrNumber}}
```
