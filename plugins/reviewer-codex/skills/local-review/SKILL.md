---
name: local-review
description: Start a local Reviewer pass for the current pull request, unstaged changes, or last commit when the user explicitly asks for a local review.
---

# Local review with Reviewer

Use this skill only when the user explicitly asks to do, start, or open a local review.

Run exactly one blocking command. Infer the scope from the request and the work in the current conversation:

- Current branch pull request: `reviewer pr-tmux --wait`
- Unstaged tracked changes only: `reviewer pr-tmux --wait --unstaged`
- Last commit only: `reviewer pr-tmux --wait --last-commit`

Use unstaged scope when the user asks to review changes Codex has just made and those changes have not been committed. This is the normal default for reviewing the current task. Use last-commit scope when the work was committed or the user names the last commit. Use pull-request scope only when the user asks for the PR or the conversation is about reviewing an existing PR rather than current local work. An explicit scope from the user always wins.

The pull-request scope includes pushed and unpushed commits, staged changes, unstaged changes, and untracked files. The other scopes avoid pull-request lookup and review only the named local diff.

The command exits only after submission and its standard output is the complete, authoritative feedback prompt. Its output begins with `LOCAL REVIEW SUBMITTED`; each numbered `Inline feedback` item is mandatory review work. Read that output directly and address it in this session.

If and only if the completed Reviewer command has no output or does not contain `LOCAL REVIEW SUBMITTED`, run `reviewer latest-codex-prompt`. This retrieves the most recently submitted local review for the current repository from Reviewer’s persistent local cache. Read its output and address every inline item. Do not inspect tmux windows, run `tmux list-windows`, search for Reviewer processes or cache files, or state that the review is awaiting feedback after either command has returned. Do not run `reviewer --help`. Do not submit a GitHub review.

Keep scope minimal and run relevant validation.
