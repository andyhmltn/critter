# Critter 🐛

Give every diff a second look. 👀

Critter brings GitHub's familiar review flow to a fast, keyboard-driven TUI: read diffs, leave inline comments, and submit a review. It also works as a review loop for coding agents—launch Critter, write your feedback, and send it straight back to the agent for another pass.

![Critter demo](assets/demo.gif)

## 📦 Install

You need [Rust](https://www.rust-lang.org/tools/install), the [GitHub CLI](https://cli.github.com/), and an authenticated GitHub session (`gh auth login`).

```sh
cargo install --git https://github.com/andyhmltn/critter --locked
```

Critter uses `nvim` to open files by default. Set `$EDITOR` to use something else.

### Releases and distro packaging

Versioned sources are available on the [releases page](https://github.com/andyhmltn/critter/releases).
Tags use `v<version>` (for example, `v0.1.0`) and match the version in `Cargo.toml`.
To build a downloaded release for packaging:

```sh
cargo build --release --locked
install -Dm755 target/release/reviewer "$pkgdir/usr/bin/reviewer"
install -Dm644 LICENSE.md "$pkgdir/usr/share/licenses/critter/LICENSE.md"
```

Here, `$pkgdir` is the package staging directory used by Arch's `PKGBUILD`.
The executable is named `reviewer`. Rust is a build dependency; the GitHub CLI
(`gh`) is needed for GitHub operations. The tmux integration also needs `tmux`.

### Publishing a release

Update the version in `Cargo.toml` and refresh `Cargo.lock` with `cargo check`.
Commit and push those changes along with the release workflow, then create and
push an annotated tag matching the package version:

```sh
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
```

Use the new version in both commands for subsequent releases. The release
workflow checks the tag against `Cargo.toml`, runs the tests and release build
with the lockfile, and publishes a GitHub release with generated notes and
GitHub's source archives. Versions with a prerelease suffix (such as
`v0.2.0-rc.1`) are marked as prereleases. Published tags should not be moved;
create a new version for fixes.

## 🔍 Review a pull request

```sh
# Pick a PR from the current repository
reviewer

# Open a PR directly
reviewer 447

# Review a repository without cloning it
reviewer -R owner/repo 447
```

Comments stay local until you submit. Press `P` to approve, request changes, or leave a comment.

## 🤖 Review with an agent

Run Critter from the agent's tmux session and wait for the submitted feedback:

```sh
# Review the current branch's PR, including uncommitted work
reviewer pr-tmux --wait

# Review the local working tree before a PR exists
reviewer local-tmux --wait
```

The command prints your review as a ready-to-use prompt, so the agent can apply the comments and run the loop again. Nothing is posted to GitHub.

To narrow a local review:

```sh
reviewer pr-tmux --wait --unstaged
reviewer pr-tmux --wait --last-commit
reviewer local-tmux --wait --base main
```

The bundled Codex plugin exposes the same flow as `$local-review`.

To start a review without spending a Codex turn on the plugin, add a tmux
binding that opens Critter in its own window. The binding captures the
originating pane before opening the review so Critter knows which Codex chat
should receive the result. When you submit, the review window closes and the
complete feedback is pasted into that chat and sent. Quitting without submitting
leaves the Codex chat untouched.

```tmux
# Review unstaged tracked changes with prefix + h.
bind-key h set-environment -gF REVIEWER_CODEX_PANE '#{pane_id}' \; \
  new-window -n reviewer -c '#{pane_current_path}' \
  '~/.cargo/bin/reviewer codex-tmux --target-pane "$REVIEWER_CODEX_PANE" --unstaged-or-pr'
```

`--unstaged-or-pr` reviews unstaged tracked changes when there are any and falls
back to the current branch pull request when the working tree is clean. Use
`reviewer codex-tmux --last-commit` for the last commit, or omit the scope flag
to always review the current branch pull request.

## ⌨️ Keys

| Key | Action |
| --- | --- |
| `j` / `k` | Move between files, change blocks, or lines |
| `h` / `l` | Previous / next file |
| `Enter` | Open or comment |
| `Alt+Enter` | Insert a newline in a comment |
| `V` | Select individual lines |
| `/` | Search changed lines |
| `n` / `N` | Next / previous search result |
| `v` | Mark file viewed |
| `o` | Open the current line in `$EDITOR` |
| `c` | View pending comments |
| `P` | Submit or hand off the review |
| `b` | Open review intelligence |
| `<Space>l` | Toggle the file sidebar |
| `:` | Open the command palette |
| `Esc` | Go back |
| `q` | Quit |

## 🧠 More

Critter can also run a deeper background review through `pi`:

```sh
reviewer peer-review -R owner/repo 42
reviewer peer-review-status -R owner/repo 42
```

It never starts an AI review automatically. Results are cached locally and appear inside the TUI.

Run `reviewer --help` for every command and `reviewer --version` for the installed version.

### gh-dash

Add Critter as a [`gh-dash`](https://github.com/dlvhdr/gh-dash) keybinding:

```yaml
- key: R
  name: review
  command: reviewer {{.PrNumber}}
```
