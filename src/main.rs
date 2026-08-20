use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use serde::{Deserialize, Serialize};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

mod progress;

use progress::ReviewProgress;

type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;

const ACCENT: Color = Color::Cyan;
const ADDED: Color = Color::Green;
const ADDED_BACKGROUND: Color = Color::Rgb(34, 48, 34);
const REMOVED: Color = Color::Red;
const REMOVED_BACKGROUND: Color = Color::Rgb(48, 34, 34);
const MUTED: Color = Color::DarkGray;
const SEARCH_MATCH: Color = Color::Yellow;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequest {
    title: String,
    body: String,
    author: Author,
    head_ref_name: String,
    base_ref_name: String,
    base_ref_oid: String,
    head_ref_oid: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestChoice {
    number: u64,
    title: String,
    author: Author,
    head_ref_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Repository {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct Author {
    login: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum Side {
    Left,
    Right,
}

#[derive(Clone)]
struct DiffLine {
    text: String,
    old_line: Option<u32>,
    new_line: Option<u32>,
    kind: LineKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineKind {
    Context,
    Add,
    Remove,
    Hunk,
    Meta,
}

#[derive(Clone)]
struct ChangedFile {
    path: String,
    lines: Vec<DiffLine>,
    syntax_lines: Vec<Vec<CodeSpan>>,
}

#[derive(Clone)]
struct CodeSpan {
    text: String,
    color: Color,
}

#[derive(Clone, Serialize)]
struct PendingComment {
    path: String,
    line: u32,
    side: Side,
    body: String,
}

enum Focus {
    Files,
    Diff,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffNavigation {
    Block,
    Line,
}

enum Mode {
    Browse,
    Search { previous_query: String },
    Description,
    Compose,
    Submit,
    ReviewSummary(&'static str),
    Comments,
    Message(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorMode {
    Insert,
    Normal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorOperator {
    Change,
    Delete,
}

struct TextEditor {
    text: String,
    cursor: usize,
    mode: EditorMode,
    pending_operator: Option<EditorOperator>,
    pending_inner: bool,
}

enum EditorAction {
    Continue,
    Submit,
    Cancel,
}

impl TextEditor {
    fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            mode: EditorMode::Insert,
            pending_operator: None,
            pending_inner: false,
        }
    }

    fn reset(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.mode = EditorMode::Insert;
        self.pending_operator = None;
        self.pending_inner = false;
    }

    fn previous_boundary(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map_or(self.text.len(), |(index, _)| self.cursor + index)
    }

    fn insert(&mut self, value: char) {
        self.text.insert(self.cursor, value);
        self.cursor += value.len_utf8();
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let start = self.previous_boundary();
            self.text.drain(start..self.cursor);
            self.cursor = start;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let end = self.next_boundary();
            self.text.drain(self.cursor..end);
        }
    }

    fn move_word_forward(&mut self) {
        while self.cursor < self.text.len()
            && self.text[self.cursor..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        {
            self.cursor = self.next_boundary();
        }
        while self.cursor < self.text.len()
            && self.text[self.cursor..]
                .chars()
                .next()
                .is_some_and(|value| !value.is_whitespace())
        {
            self.cursor = self.next_boundary();
        }
    }

    fn move_word_backward(&mut self) {
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            self.cursor = self.previous_boundary();
        }
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(|value| !value.is_whitespace())
        {
            self.cursor = self.previous_boundary();
        }
    }

    fn change_line(&mut self) {
        let start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index);
        self.text.drain(start..end);
        self.cursor = start;
        self.mode = EditorMode::Insert;
    }

    fn delete_line(&mut self) {
        let mut start = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let mut end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index + 1);
        if end == self.text.len() && start > 0 {
            start -= 1;
        }
        if start == end {
            end = self.text.len();
        }
        self.text.drain(start..end);
        self.cursor = start.min(self.text.len());
    }

    fn inner_word_range(&self) -> Option<(usize, usize)> {
        if self.text.is_empty() {
            return None;
        }
        let anchor = if self.cursor == self.text.len() {
            self.previous_boundary()
        } else {
            self.cursor
        };
        let class = |character: char| {
            if character.is_alphanumeric() || character == '_' {
                0
            } else if character.is_whitespace() {
                1
            } else {
                2
            }
        };
        let target_class = class(self.text[anchor..].chars().next()?);
        let mut start = anchor;
        while start > 0 {
            let previous = self.text[..start].char_indices().next_back()?.0;
            if class(self.text[previous..].chars().next()?) != target_class {
                break;
            }
            start = previous;
        }
        let mut end = anchor;
        while end < self.text.len() {
            let character = self.text[end..].chars().next()?;
            if class(character) != target_class {
                break;
            }
            end += character.len_utf8();
        }
        Some((start, end))
    }

    fn apply_range_operator(&mut self, operator: EditorOperator, start: usize, end: usize) {
        self.text.drain(start..end);
        self.cursor = start;
        if matches!(operator, EditorOperator::Change) {
            self.mode = EditorMode::Insert;
        }
    }

    fn apply_word_operator(&mut self, operator: EditorOperator) {
        let start = self.cursor;
        self.move_word_forward();
        let end = self.cursor;
        self.apply_range_operator(operator, start, end);
    }

    fn handle(&mut self, key: KeyEvent) -> EditorAction {
        if matches!(self.mode, EditorMode::Normal) {
            if let Some(operator) = self.pending_operator.take() {
                if self.pending_inner {
                    self.pending_inner = false;
                    if matches!(key.code, KeyCode::Char('w')) {
                        if let Some((start, end)) = self.inner_word_range() {
                            self.apply_range_operator(operator, start, end);
                        }
                        return EditorAction::Continue;
                    }
                } else {
                    match key.code {
                        KeyCode::Char('i') => {
                            self.pending_operator = Some(operator);
                            self.pending_inner = true;
                            return EditorAction::Continue;
                        }
                        KeyCode::Char('w') => {
                            self.apply_word_operator(operator);
                            return EditorAction::Continue;
                        }
                        KeyCode::Char('c') if matches!(operator, EditorOperator::Change) => {
                            self.change_line();
                            return EditorAction::Continue;
                        }
                        KeyCode::Char('d') if matches!(operator, EditorOperator::Delete) => {
                            self.delete_line();
                            return EditorAction::Continue;
                        }
                        _ => {}
                    }
                }
            }
            match key.code {
                KeyCode::Char('c') => {
                    self.pending_operator = Some(EditorOperator::Change);
                    return EditorAction::Continue;
                }
                KeyCode::Char('d') => {
                    self.pending_operator = Some(EditorOperator::Delete);
                    return EditorAction::Continue;
                }
                _ => {}
            }
        }
        match self.mode {
            EditorMode::Insert => match key.code {
                KeyCode::Esc => {
                    self.mode = EditorMode::Normal;
                    EditorAction::Continue
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.insert('\n');
                    EditorAction::Continue
                }
                KeyCode::Enter => EditorAction::Submit,
                KeyCode::Backspace => {
                    self.backspace();
                    EditorAction::Continue
                }
                KeyCode::Left => {
                    self.cursor = self.previous_boundary();
                    EditorAction::Continue
                }
                KeyCode::Right => {
                    self.cursor = self.next_boundary();
                    EditorAction::Continue
                }
                KeyCode::Char(value) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.insert(value);
                    EditorAction::Continue
                }
                _ => EditorAction::Continue,
            },
            EditorMode::Normal => match key.code {
                KeyCode::Esc => EditorAction::Cancel,
                KeyCode::Char('i') => {
                    self.mode = EditorMode::Insert;
                    EditorAction::Continue
                }
                KeyCode::Char('a') => {
                    self.cursor = self.next_boundary();
                    self.mode = EditorMode::Insert;
                    EditorAction::Continue
                }
                KeyCode::Char('I') | KeyCode::Char('0') => {
                    self.cursor = self.text[..self.cursor]
                        .rfind('\n')
                        .map_or(0, |index| index + 1);
                    if matches!(key.code, KeyCode::Char('I')) {
                        self.mode = EditorMode::Insert;
                    }
                    EditorAction::Continue
                }
                KeyCode::Char('A') | KeyCode::Char('$') => {
                    self.cursor = self.text[self.cursor..]
                        .find('\n')
                        .map_or(self.text.len(), |index| self.cursor + index);
                    if matches!(key.code, KeyCode::Char('A')) {
                        self.mode = EditorMode::Insert;
                    }
                    EditorAction::Continue
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    self.cursor = self.previous_boundary();
                    EditorAction::Continue
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    self.cursor = self.next_boundary();
                    EditorAction::Continue
                }
                KeyCode::Char('w') => {
                    self.move_word_forward();
                    EditorAction::Continue
                }
                KeyCode::Char('b') => {
                    self.move_word_backward();
                    EditorAction::Continue
                }
                KeyCode::Char('x') | KeyCode::Delete => {
                    self.delete();
                    EditorAction::Continue
                }
                _ => EditorAction::Continue,
            },
        }
    }
}

struct App {
    pr_number: String,
    repo: String,
    pull: PullRequest,
    files: Vec<ChangedFile>,
    file_index: usize,
    line_index: usize,
    focus: Focus,
    diff_navigation: DiffNavigation,
    mode: Mode,
    editor: TextEditor,
    comments: Vec<PendingComment>,
    comment_index: usize,
    should_quit: bool,
    return_to_picker: bool,
    should_redraw: bool,
    sidebar_visible: bool,
    files_state: ListState,
    search_query: String,
    progress: ReviewProgress,
}

fn gh(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .context("could not start gh")?;
    if output.status.success() {
        Ok(String::from_utf8(output.stdout).context("gh returned invalid UTF-8")?)
    } else {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim())
    }
}

fn current_repo() -> Result<String> {
    let repo_output = gh(&["repo", "view", "--json", "nameWithOwner"])
        .context("could not determine the current repository")?;
    let repo: Repository =
        serde_json::from_str(&repo_output).context("could not parse the current repository")?;
    Ok(repo.name_with_owner)
}

fn open_pull_requests(repo: &str) -> Result<Vec<PullRequestChoice>> {
    search_pull_requests(repo, "is:open")
}

fn search_pull_requests(repo: &str, query: &str) -> Result<Vec<PullRequestChoice>> {
    let output = gh(&[
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "all",
        "--search",
        query,
        "--limit",
        "100",
        "--json",
        "number,title,author,headRefName",
    ])
    .context("could not list pull requests")?;
    serde_json::from_str(&output).context("could not parse pull request list")
}

fn repository_contributors(repo: &str, pulls: &[PullRequestChoice]) -> Vec<String> {
    let mut contributors: Vec<String> =
        pulls.iter().map(|pull| pull.author.login.clone()).collect();
    if let Ok(output) = gh(&[
        "api",
        &format!("repos/{repo}/contributors?per_page=100"),
        "--paginate",
        "--jq",
        ".[] | .login // empty",
    ]) {
        contributors.extend(output.lines().map(str::to_string));
    }
    contributors.sort_by_key(|login| login.to_lowercase());
    contributors.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    contributors
}

fn active_author_prefix(editor: &TextEditor) -> Option<(usize, &str)> {
    let before_cursor = &editor.text[..editor.cursor];
    let token_start = before_cursor
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(index, character)| index + character.len_utf8());
    before_cursor[token_start..]
        .strip_prefix("author:")
        .map(|prefix| (token_start, prefix))
}

fn author_suggestions<'a>(editor: &TextEditor, contributors: &'a [String]) -> Vec<&'a str> {
    let Some((_, prefix)) = active_author_prefix(editor) else {
        return Vec::new();
    };
    let prefix = prefix.to_lowercase();
    contributors
        .iter()
        .filter(|login| login.to_lowercase().starts_with(&prefix))
        .take(5)
        .map(String::as_str)
        .collect()
}

fn complete_author(editor: &mut TextEditor, login: &str) {
    let Some((token_start, _)) = active_author_prefix(editor) else {
        return;
    };
    editor
        .text
        .replace_range(token_start..editor.cursor, &format!("author:{login} "));
    editor.cursor = token_start + "author:".len() + login.len() + 1;
}

struct PullPickerView<'a> {
    repo: &'a str,
    pulls: &'a [PullRequestChoice],
    state: &'a mut ListState,
    editor: &'a TextEditor,
    searching: bool,
    suggestions: &'a [&'a str],
    suggestion_index: usize,
    error: Option<&'a str>,
}

fn draw_pr_picker(frame: &mut ratatui::Frame, view: PullPickerView<'_>) {
    let PullPickerView {
        repo,
        pulls,
        state,
        editor,
        searching,
        suggestions,
        suggestion_index,
        error,
    } = view;
    let area = frame.area();
    let search_height = 3 + if searching {
        suggestions.len() as u16
    } else {
        0
    };
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(search_height),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(format!(" Choose a pull request in {repo} "))
            .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL)),
        layout[0],
    );
    let search_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(layout[1]);
    let search_marker = if searching { "› " } else { "  " };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(search_marker, Style::default().fg(ACCENT)),
            Span::raw(&editor.text),
            Span::styled(
                if searching {
                    format!("  [{}]", editor_mode_label(editor))
                } else {
                    String::new()
                },
                Style::default().fg(MUTED),
            ),
        ]))
        .block(
            Block::default()
                .title(" Search ")
                .borders(Borders::ALL)
                .border_style(if searching {
                    Style::default().fg(ACCENT)
                } else {
                    Style::default()
                }),
        ),
        search_layout[0],
    );
    if searching && !suggestions.is_empty() {
        let items = suggestions.iter().enumerate().map(|(index, login)| {
            let style = if index == suggestion_index {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(format!("  author:{login}")).style(style)
        });
        frame.render_widget(List::new(items), search_layout[1]);
    }
    let items = pulls.iter().map(|pull| {
        ListItem::new(Line::from(vec![
            Span::styled(format!("#{:<6}", pull.number), Style::default().fg(ACCENT)),
            Span::raw(&pull.title),
            Span::styled(
                format!("  @{}  {}", pull.author.login, pull.head_ref_name),
                Style::default().fg(MUTED),
            ),
        ]))
    });
    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Pull requests ({}) ", pulls.len()))
                .borders(Borders::ALL),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, layout[2], state);
    let status = if let Some(error) = error {
        format!(" {error}")
    } else if searching {
        " ↑/↓ choose author  Tab complete  Enter search  Esc normal/cancel".to_string()
    } else {
        " j/k or ↑/↓ select  Enter open  / search  q/Esc quit".to_string()
    };
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(if error.is_some() {
            REMOVED
        } else {
            MUTED
        })),
        layout[3],
    );
}

fn pick_pull_request(
    terminal: &mut AppTerminal,
    repo: &str,
    pulls: &mut Vec<PullRequestChoice>,
    contributors: &mut Vec<String>,
    query: &mut String,
    initially_selected: usize,
) -> Result<Option<(String, usize)>> {
    let mut selected = initially_selected.min(pulls.len().saturating_sub(1));
    let mut state = ListState::default().with_selected((!pulls.is_empty()).then_some(selected));
    let mut editor = TextEditor::new();
    editor.text = query.clone();
    editor.cursor = editor.text.len();
    let mut searching = false;
    let mut suggestion_index = 0usize;
    let mut error: Option<String> = None;
    loop {
        let suggestions = author_suggestions(&editor, contributors);
        suggestion_index = suggestion_index.min(suggestions.len().saturating_sub(1));
        terminal.draw(|frame| {
            draw_pr_picker(
                frame,
                PullPickerView {
                    repo,
                    pulls,
                    state: &mut state,
                    editor: &editor,
                    searching,
                    suggestions: &suggestions,
                    suggestion_index,
                    error: error.as_deref(),
                },
            )
        })?;
        if let Event::Key(key) = event::read()?
            && key.kind == event::KeyEventKind::Press
        {
            if searching {
                error = None;
                if !suggestions.is_empty() && matches!(key.code, KeyCode::Up) {
                    suggestion_index = suggestion_index.saturating_sub(1);
                    continue;
                }
                if !suggestions.is_empty() && matches!(key.code, KeyCode::Down) {
                    suggestion_index = (suggestion_index + 1).min(suggestions.len() - 1);
                    continue;
                }
                if !suggestions.is_empty() && matches!(key.code, KeyCode::Tab) {
                    let login = suggestions[suggestion_index].to_string();
                    complete_author(&mut editor, &login);
                    suggestion_index = 0;
                    continue;
                }
                match editor.handle(key) {
                    EditorAction::Cancel => {
                        searching = false;
                        editor.text = query.clone();
                        editor.cursor = editor.text.len();
                        editor.mode = EditorMode::Insert;
                    }
                    EditorAction::Submit => {
                        let submitted = editor.text.trim();
                        let submitted = if submitted.is_empty() {
                            "is:open"
                        } else {
                            submitted
                        };
                        match search_pull_requests(repo, submitted) {
                            Ok(results) => {
                                *query = submitted.to_string();
                                *pulls = results;
                                contributors
                                    .extend(pulls.iter().map(|pull| pull.author.login.clone()));
                                contributors.sort_by_key(|login| login.to_lowercase());
                                contributors
                                    .dedup_by(|left, right| left.eq_ignore_ascii_case(right));
                                selected = 0;
                                state.select((!pulls.is_empty()).then_some(0));
                                searching = false;
                                editor.mode = EditorMode::Insert;
                            }
                            Err(search_error) => error = Some(search_error.to_string()),
                        }
                    }
                    EditorAction::Continue => {
                        suggestion_index = 0;
                    }
                }
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
                KeyCode::Char('/') => {
                    searching = true;
                    editor.text = if query.is_empty() {
                        String::new()
                    } else {
                        format!("{query} ")
                    };
                    editor.cursor = editor.text.len();
                    editor.mode = EditorMode::Insert;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if !pulls.is_empty() {
                        selected = (selected + 1).min(pulls.len() - 1);
                        state.select(Some(selected));
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if !pulls.is_empty() {
                        selected = selected.saturating_sub(1);
                        state.select(Some(selected));
                    }
                }
                KeyCode::Enter if !pulls.is_empty() => {
                    return Ok(Some((pulls[selected].number.to_string(), selected)));
                }
                _ => {}
            }
        }
    }
}

fn git_at(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .context("could not start git")?;
    if output.status.success() {
        Ok(String::from_utf8(output.stdout).context("git returned invalid UTF-8")?)
    } else {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim())
    }
}

fn remote_for_repo(repo: &str) -> Result<String> {
    let root = Path::new(".");
    let remotes = git_at(root, &["remote"])?;
    for remote in remotes.lines() {
        let Ok(url) = git_at(root, &["remote", "get-url", remote]) else {
            continue;
        };
        if remote_matches_repo(&url, repo) {
            return Ok(remote.to_string());
        }
    }
    bail!("no Git remote matches repository {repo}")
}

fn local_pr_diff(pr_number: &str, repo: &str, pull: &PullRequest) -> Result<String> {
    let remote = remote_for_repo(repo)?;
    let head_ref = format!("refs/pull/{pr_number}/head");
    let base_ref = format!("refs/heads/{}", pull.base_ref_name);
    let root = Path::new(".");
    git_at(root, &["fetch", "--no-tags", &remote, &head_ref, &base_ref])
        .context("could not fetch the pull request commits")?;
    git_at(
        root,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            &pull.base_ref_oid,
            &pull.head_ref_oid,
        ],
    )
    .context("could not generate a local pull request diff")
}

fn parse_hunk(value: &str) -> Option<u32> {
    value
        .trim_start_matches(['-', '+'])
        .split(',')
        .next()?
        .parse()
        .ok()
}

fn parse_diff(diff: &str) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    let mut current: Option<ChangedFile> = None;
    let mut old_line = 0;
    let mut new_line = 0;
    for raw in diff.lines() {
        if let Some(path) = raw
            .strip_prefix("diff --git a/")
            .and_then(|value| value.split(" b/").nth(1))
        {
            if let Some(file) = current.take() {
                files.push(file);
            }
            current = Some(ChangedFile {
                path: path.to_string(),
                lines: Vec::new(),
                syntax_lines: Vec::new(),
            });
            continue;
        }
        let Some(file) = current.as_mut() else {
            continue;
        };
        if raw.starts_with("@@") {
            let mut pieces = raw.split_whitespace();
            let _ = pieces.next();
            old_line = pieces.next().and_then(parse_hunk).unwrap_or(1);
            new_line = pieces.next().and_then(parse_hunk).unwrap_or(1);
            file.lines.push(DiffLine {
                text: raw.to_string(),
                old_line: None,
                new_line: None,
                kind: LineKind::Hunk,
            });
        } else if let Some(text) = raw.strip_prefix('+') {
            if !raw.starts_with("+++") {
                file.lines.push(DiffLine {
                    text: text.to_string(),
                    old_line: None,
                    new_line: Some(new_line),
                    kind: LineKind::Add,
                });
                new_line += 1;
            }
        } else if let Some(text) = raw.strip_prefix('-') {
            if !raw.starts_with("---") {
                file.lines.push(DiffLine {
                    text: text.to_string(),
                    old_line: Some(old_line),
                    new_line: None,
                    kind: LineKind::Remove,
                });
                old_line += 1;
            }
        } else if let Some(text) = raw.strip_prefix(' ') {
            file.lines.push(DiffLine {
                text: text.to_string(),
                old_line: Some(old_line),
                new_line: Some(new_line),
                kind: LineKind::Context,
            });
            old_line += 1;
            new_line += 1;
        } else if raw.starts_with("index ")
            || raw.starts_with("new file")
            || raw.starts_with("deleted file")
            || raw.starts_with("rename ")
        {
            file.lines.push(DiffLine {
                text: raw.to_string(),
                old_line: None,
                new_line: None,
                kind: LineKind::Meta,
            });
        }
    }
    if let Some(file) = current {
        files.push(file);
    }
    files
}

fn selected_comment_target(app: &App) -> Option<(u32, Side)> {
    let line = app.files.get(app.file_index)?.lines.get(app.line_index)?;
    if let Some(number) = line.new_line {
        return Some((number, Side::Right));
    }
    line.old_line.map(|number| (number, Side::Left))
}

fn selected_file(app: &App) -> &ChangedFile {
    &app.files[app.file_index]
}

fn is_change_line(line: &DiffLine) -> bool {
    matches!(line.kind, LineKind::Add | LineKind::Remove)
}

fn change_block_at(file: &ChangedFile, index: usize) -> Option<(usize, usize)> {
    if !file.lines.get(index).is_some_and(is_change_line) {
        return None;
    }
    let mut start = index;
    while start > 0 && is_change_line(&file.lines[start - 1]) {
        start -= 1;
    }
    let mut end = index;
    while end + 1 < file.lines.len() && is_change_line(&file.lines[end + 1]) {
        end += 1;
    }
    Some((start, end))
}

fn move_change_block(app: &mut App, amount: isize) {
    let lines = &selected_file(app).lines;
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(index, line)| {
            is_change_line(line) && (*index == 0 || !is_change_line(&lines[index - 1]))
        })
        .map(|(index, _)| index)
        .collect();
    if starts.is_empty() {
        return;
    }
    let current = starts
        .iter()
        .rposition(|start| *start <= app.line_index)
        .unwrap_or(0);
    let target = current.saturating_add_signed(amount).min(starts.len() - 1);
    app.line_index = starts[target];
}

fn move_line_in_block(app: &mut App, amount: isize) {
    let Some((start, end)) = change_block_at(selected_file(app), app.line_index) else {
        return;
    };
    app.line_index = app
        .line_index
        .saturating_add_signed(amount)
        .clamp(start, end);
}

fn selected_block_has_one_line(app: &App) -> bool {
    change_block_at(selected_file(app), app.line_index).is_some_and(|(start, end)| start == end)
}

fn change_file(app: &mut App, amount: isize) {
    if app.files.is_empty() {
        return;
    }
    app.file_index = app
        .file_index
        .saturating_add_signed(amount)
        .min(app.files.len() - 1);
    app.line_index = 0;
    app.diff_navigation = DiffNavigation::Block;
    move_change_block(app, 0);
    app.files_state.select(Some(app.file_index));
}

fn page_size(app: &App) -> isize {
    let line_count = selected_file(app).lines.len();
    (line_count.clamp(1, 20) as isize).saturating_sub(1)
}

fn move_file(app: &mut App, amount: isize) {
    change_file(app, amount);
}

fn compact_path(path: &str, max_width: usize) -> String {
    if path.chars().count() <= max_width {
        return path.to_string();
    }
    let first = path.split('/').next().unwrap_or(path);
    let last = path.rsplit('/').next().unwrap_or(path);
    let abbreviated = format!("{first}/../{last}");
    if abbreviated.chars().count() <= max_width {
        return abbreviated;
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let prefix_width = (max_width / 3).max(1);
    let suffix_width = max_width.saturating_sub(prefix_width + 3);
    let prefix: String = first.chars().take(prefix_width).collect();
    let suffix: String = last
        .chars()
        .rev()
        .take(suffix_width)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{prefix}/…/{suffix}")
}

fn search(app: &mut App, forward: bool) -> bool {
    if app.search_query.is_empty() || app.files.is_empty() {
        return false;
    }
    let query = app.search_query.to_lowercase();
    let file_count = app.files.len();
    let starting_file = app.file_index;
    for file_offset in 0..file_count {
        let file_index = if forward {
            (starting_file + file_offset) % file_count
        } else {
            (starting_file + file_count - file_offset % file_count) % file_count
        };
        let lines = &app.files[file_index].lines;
        if lines.is_empty() {
            continue;
        }
        let starting_line = if file_offset == 0 {
            if forward {
                (app.line_index + 1) % lines.len()
            } else {
                (app.line_index + lines.len() - 1) % lines.len()
            }
        } else if forward {
            0
        } else {
            lines.len() - 1
        };
        let lines_to_search = if file_offset == 0 {
            lines.len().saturating_sub(1)
        } else {
            lines.len()
        };
        for line_offset in 0..lines_to_search {
            let line_index = if forward {
                (starting_line + line_offset) % lines.len()
            } else {
                (starting_line + lines.len() - line_offset % lines.len()) % lines.len()
            };
            if line_matches(&lines[line_index], &query) {
                app.file_index = file_index;
                app.line_index = line_index;
                app.files_state.select(Some(file_index));
                app.focus = Focus::Diff;
                return true;
            }
        }
    }
    false
}

fn line_matches(line: &DiffLine, query: &str) -> bool {
    line.text.to_lowercase().contains(query)
}

fn active_search_query(app: &App) -> &str {
    match &app.mode {
        Mode::Search { .. } => app.editor.text.trim(),
        _ => app.search_query.as_str(),
    }
}

fn editor_overlay(area: Rect, editor: &TextEditor) -> Rect {
    let width = area.width * 8 / 10;
    let content_width = width.saturating_sub(2).max(1) as usize;
    let mut lines = 1usize;
    let mut line_width = 0usize;
    for character in editor.text.chars() {
        if character == '\n' {
            lines += 1;
            line_width = 0;
        } else {
            line_width += 1;
            if line_width > content_width {
                lines += 1;
                line_width = 1;
            }
        }
    }
    let height = (lines as u16 + 2).clamp(3, area.height * 3 / 4);
    Rect {
        x: area.x + area.width / 10,
        y: area.y + area.height / 8,
        width,
        height,
    }
}

fn editor_mode_label(editor: &TextEditor) -> &'static str {
    match editor.mode {
        EditorMode::Insert => "INSERT",
        EditorMode::Normal => "NORMAL",
    }
}

fn place_editor_cursor(frame: &mut ratatui::Frame, overlay: Rect, editor: &TextEditor) {
    let content_width = overlay.width.saturating_sub(2).max(1);
    let mut x = 0u16;
    let mut y = 0u16;
    for character in editor.text[..editor.cursor].chars() {
        if character == '\n' {
            x = 0;
            y += 1;
        } else {
            x += 1;
            if x == content_width {
                x = 0;
                y += 1;
            }
        }
    }
    frame.set_cursor_position((
        overlay.x + 1 + x,
        (overlay.y + 1 + y).min(overlay.bottom().saturating_sub(2)),
    ));
}

fn mark_search_matches(spans: Vec<Span<'static>>, query: &str) -> Vec<Span<'static>> {
    if query.is_empty() {
        return spans;
    }
    let query = query.to_lowercase();
    spans
        .into_iter()
        .flat_map(|span| {
            let text = span.content.to_string();
            let lower_text = text.to_lowercase();
            let mut fragments = Vec::new();
            let mut offset = 0;
            while let Some(index) = lower_text[offset..].find(&query) {
                let start = offset + index;
                let end = start + query.len();
                if start > offset {
                    fragments.push(Span::styled(text[offset..start].to_string(), span.style));
                }
                fragments.push(Span::styled(
                    text[start..end].to_string(),
                    span.style.bg(SEARCH_MATCH).fg(Color::Black),
                ));
                offset = end;
            }
            if fragments.is_empty() {
                fragments.push(span);
            } else if offset < text.len() {
                fragments.push(Span::styled(text[offset..].to_string(), span.style));
            }
            fragments
        })
        .collect()
}

fn line_style(kind: LineKind) -> Style {
    match kind {
        LineKind::Add => Style::default().fg(ADDED).bg(ADDED_BACKGROUND),
        LineKind::Remove => Style::default().fg(REMOVED).bg(REMOVED_BACKGROUND),
        LineKind::Hunk => Style::default().fg(ACCENT),
        LineKind::Meta => Style::default().fg(MUTED),
        LineKind::Context => Style::default(),
    }
}

fn syntax_color(color: syntect::highlighting::Color) -> Color {
    let red = color.r as i16;
    let green = color.g as i16;
    let blue = color.b as i16;
    let high = red.max(green).max(blue);
    let low = red.min(green).min(blue);
    if high - low < 24 {
        return Color::White;
    }
    if red >= green && red >= blue {
        if blue > green + 20 {
            Color::LightMagenta
        } else {
            Color::LightRed
        }
    } else if green >= red && green >= blue {
        if blue > red + 20 {
            Color::LightCyan
        } else {
            Color::LightGreen
        }
    } else if red > green + 20 {
        Color::LightMagenta
    } else {
        Color::LightBlue
    }
}

fn syntax_for_path<'a>(syntax_set: &'a SyntaxSet, path: &str) -> &'a SyntaxReference {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let frontend_syntax = match extension.as_str() {
        "ts" => syntax_set
            .find_syntax_by_token("TypeScript")
            .or_else(|| syntax_set.find_syntax_by_token("JavaScript")),
        "tsx" | "jsx" => syntax_set.find_syntax_by_token("JavaScript"),
        "html" | "htm" | "vue" | "svelte" => syntax_set.find_syntax_by_token("HTML"),
        "css" | "scss" | "sass" | "less" => syntax_set.find_syntax_by_token("CSS"),
        _ => None,
    };
    frontend_syntax
        .or_else(|| syntax_set.find_syntax_by_extension(&extension))
        .or_else(|| syntax_set.find_syntax_for_file(path).ok().flatten())
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text())
}

fn highlight_files(files: &mut [ChangedFile], syntax_set: &SyntaxSet, syntax_theme: &Theme) {
    for file in files {
        let syntax = syntax_for_path(syntax_set, &file.path);
        let mut highlighter = HighlightLines::new(syntax, syntax_theme);
        file.syntax_lines = file
            .lines
            .iter()
            .map(|line| {
                let source = format!("{}\n", line.text);
                match highlighter.highlight_line(&source, syntax_set) {
                    Ok(ranges) => ranges
                        .into_iter()
                        .filter_map(|(style, text)| {
                            let text = text.trim_end_matches(['\r', '\n']);
                            (!text.is_empty()).then(|| CodeSpan {
                                text: text.to_string(),
                                color: syntax_color(style.foreground),
                            })
                        })
                        .collect(),
                    Err(_) => Vec::new(),
                }
            })
            .collect();
    }
}

fn draw(app: &mut App, frame: &mut ratatui::Frame) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(area);
    let title = format!(" #{}  {} ", app.pr_number, app.pull.title);
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{} → {}  @{}",
                app.pull.head_ref_name, app.pull.base_ref_name, app.pull.author.login
            ),
            Style::default().fg(MUTED),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(header, layout[0]);
    let diff_area = if app.sidebar_visible {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
            .split(layout[1]);
        let file_label_width = columns[0].width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = app
            .files
            .iter()
            .map(|file| {
                let marked = app
                    .comments
                    .iter()
                    .filter(|comment| comment.path == file.path)
                    .count();
                let viewed = if app.progress.is_viewed(&file.path) {
                    "✓ "
                } else {
                    "  "
                };
                let label = if marked == 0 {
                    format!(
                        "{viewed}{}",
                        compact_path(&file.path, file_label_width.saturating_sub(2))
                    )
                } else {
                    format!(
                        "{viewed}{}  [{marked}]",
                        compact_path(&file.path, file_label_width.saturating_sub(2))
                    )
                };
                ListItem::new(label)
            })
            .collect();
        let files = List::new(items)
            .block(
                Block::default()
                    .title(" Files ")
                    .borders(Borders::ALL)
                    .border_style(if matches!(app.focus, Focus::Files) {
                        Style::default().fg(ACCENT)
                    } else {
                        Style::default()
                    }),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        frame.render_stateful_widget(files, columns[0], &mut app.files_state);
        columns[1]
    } else {
        layout[1]
    };
    let file = selected_file(app);
    let visible_height = diff_area.height.saturating_sub(2) as usize;
    let start = app.line_index.saturating_sub(visible_height / 2);
    let search_query = active_search_query(app).to_string();
    let normalized_search_query = search_query.to_lowercase();
    let diff_lines: Vec<Line> = file
        .lines
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_height)
        .map(|(index, line)| {
            let old = line
                .old_line
                .map_or("    ".to_string(), |number| format!("{number:>4}"));
            let new = line
                .new_line
                .map_or("    ".to_string(), |number| format!("{number:>4}"));
            let sign = match line.kind {
                LineKind::Add => "+",
                LineKind::Remove => "-",
                _ => " ",
            };
            let active_range = change_block_at(file, app.line_index);
            let active_block =
                active_range.is_some_and(|(start, end)| (start..=end).contains(&index));
            let selected =
                matches!(app.diff_navigation, DiffNavigation::Line) && index == app.line_index;
            let marker = if selected {
                "▶"
            } else if let Some((start, end)) = active_range.filter(|_| active_block) {
                if start == end {
                    "◆"
                } else if index == start {
                    "┏"
                } else if index == end {
                    "┗"
                } else {
                    "┃"
                }
            } else {
                " "
            };
            let matched =
                !normalized_search_query.is_empty() && line_matches(line, &normalized_search_query);
            let mut style = line_style(line.kind);
            if matched && !active_block {
                style = style.bg(Color::DarkGray);
            }
            let line_prefix = format!("{old} {new} {sign} ");
            let marker_style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else if active_block {
                line_style(line.kind)
                    .fg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                style
            };
            let mut spans = vec![
                Span::styled(marker, marker_style),
                Span::styled(line_prefix, style),
            ];
            let mut syntax_spans: Vec<Span> = file.syntax_lines[index]
                .iter()
                .map(|span| Span::styled(span.text.clone(), Style::default().fg(span.color)))
                .collect();
            if syntax_spans.is_empty() {
                syntax_spans.push(Span::raw(line.text.clone()));
            }
            for span in &mut syntax_spans {
                span.style = if matched && !active_block {
                    span.style.bg(Color::DarkGray)
                } else {
                    match line.kind {
                        LineKind::Add => span.style.bg(ADDED_BACKGROUND),
                        LineKind::Remove => span.style.bg(REMOVED_BACKGROUND),
                        _ => span.style,
                    }
                };
            }
            syntax_spans = mark_search_matches(syntax_spans, &search_query);
            spans.extend(syntax_spans);
            Line::from(spans)
        })
        .collect();
    let diff = Paragraph::new(Text::from(diff_lines))
        .block(
            Block::default()
                .title(format!(
                    " {} [{}] ",
                    file.path,
                    match app.diff_navigation {
                        DiffNavigation::Block => "BLOCK",
                        DiffNavigation::Line => "LINE",
                    }
                ))
                .borders(Borders::ALL)
                .border_style(if matches!(app.focus, Focus::Diff) {
                    Style::default().fg(ACCENT)
                } else {
                    Style::default()
                }),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(diff, diff_area);
    let status = match &app.mode {
        Mode::Browse
            if matches!(app.focus, Focus::Diff)
                && matches!(app.diff_navigation, DiffNavigation::Line) =>
        {
            "LINE  j/k line  Enter comment  Esc blocks  v viewed  / search  o editor  c pending  s submit  q quit"
        }
        Mode::Browse if app.sidebar_visible => {
            "BLOCK  j/k change  Enter lines  Ctrl-d/u page  v viewed  Esc files  / search  d description  c pending  s submit  q quit"
        }
        Mode::Browse => {
            "BLOCK  j/k change  Enter lines  Ctrl-d/u page  v viewed  Esc show files  / search  d description  c pending  s submit  q quit"
        }
        Mode::Search { .. } => "Enter confirm  Esc normal  Esc again cancel",
        Mode::Compose => "Enter save  Ctrl+Enter newline  Esc normal  Esc again cancel",
        Mode::Description => "Esc or d to return",
        Mode::Submit => "a approve  r request changes  c comment  Esc cancel",
        Mode::ReviewSummary(_) => "Enter submit  Ctrl+Enter newline  Esc normal  Esc again cancel",
        Mode::Comments => "j/k select  x remove  Esc or c close",
        Mode::Message(_) => "Enter or Esc close",
    };
    let search_status = if app.search_query.is_empty() && !matches!(&app.mode, Mode::Search { .. })
    {
        String::new()
    } else {
        format!("    /{search_query}")
    };
    frame.render_widget(
        Paragraph::new(format!(
            " {status}{search_status}    {} pending  {}/{} viewed",
            app.comments.len(),
            app.progress.viewed_count(),
            app.files.len()
        ))
        .style(Style::default().fg(MUTED)),
        layout[2],
    );
    draw_overlay(app, frame, area);
}

fn draw_overlay(app: &App, frame: &mut ratatui::Frame, area: Rect) {
    let standard_overlay = Rect {
        x: area.x + area.width / 10,
        y: area.y + area.height / 8,
        width: area.width * 8 / 10,
        height: area.height * 3 / 4,
    };
    let overlay = match app.mode {
        Mode::Search { .. } | Mode::Compose | Mode::ReviewSummary(_) => {
            editor_overlay(area, &app.editor)
        }
        _ => standard_overlay,
    };
    match &app.mode {
        Mode::Browse => {}
        Mode::Search { .. } => {
            frame.render_widget(Clear, overlay);
            frame.render_widget(
                Paragraph::new(app.editor.text.as_str())
                    .block(
                        Block::default()
                            .title(format!(" Search [{}] ", editor_mode_label(&app.editor)))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    )
                    .wrap(Wrap { trim: false }),
                overlay,
            );
            place_editor_cursor(frame, overlay, &app.editor);
        }
        Mode::Description => {
            frame.render_widget(Clear, overlay);
            frame.render_widget(
                Paragraph::new(app.pull.body.as_str())
                    .block(
                        Block::default()
                            .title(" Description ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    )
                    .wrap(Wrap { trim: false }),
                overlay,
            );
        }
        Mode::Compose => {
            frame.render_widget(Clear, overlay);
            let target = selected_comment_target(app)
                .map(|(line, side)| format!("{}:{line} ({side:?})", selected_file(app).path))
                .unwrap_or_else(|| "not commentable".to_string());
            frame.render_widget(
                Paragraph::new(app.editor.text.as_str())
                    .block(
                        Block::default()
                            .title(format!(
                                " Comment: {target} [{}] ",
                                editor_mode_label(&app.editor)
                            ))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    )
                    .wrap(Wrap { trim: false }),
                overlay,
            );
            place_editor_cursor(frame, overlay, &app.editor);
        }
        Mode::Submit => {
            let choices =
                "Submit review\n\n[a] Approve\n[r] Request changes\n[c] Comment\n\nEsc cancels";
            frame.render_widget(Clear, overlay);
            frame.render_widget(
                Paragraph::new(choices).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(ACCENT)),
                ),
                overlay,
            );
        }
        Mode::ReviewSummary(_) => {
            frame.render_widget(Clear, overlay);
            frame.render_widget(
                Paragraph::new(app.editor.text.as_str())
                    .block(
                        Block::default()
                            .title(format!(
                                " Review summary (optional) [{}] ",
                                editor_mode_label(&app.editor)
                            ))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    )
                    .wrap(Wrap { trim: false }),
                overlay,
            );
            place_editor_cursor(frame, overlay, &app.editor);
        }
        Mode::Comments => {
            let content = if app.comments.is_empty() {
                "No pending comments.".to_string()
            } else {
                app.comments
                    .iter()
                    .enumerate()
                    .map(|(index, comment)| {
                        format!(
                            "{} {}  {}:{}  {}",
                            if index == app.comment_index {
                                "›"
                            } else {
                                " "
                            },
                            index + 1,
                            comment.path,
                            comment.line,
                            comment.body.replace('\n', " ")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            frame.render_widget(Clear, overlay);
            frame.render_widget(
                Paragraph::new(content)
                    .block(
                        Block::default()
                            .title(" Pending comments ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    )
                    .wrap(Wrap { trim: false }),
                overlay,
            );
        }
        Mode::Message(message) => {
            frame.render_widget(Clear, overlay);
            frame.render_widget(
                Paragraph::new(message.as_str())
                    .block(
                        Block::default()
                            .title(" reviewer ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    )
                    .wrap(Wrap { trim: false }),
                overlay,
            );
        }
    }
}

fn remote_matches_repo(remote: &str, repo: &str) -> bool {
    let remote = remote.trim().trim_end_matches(".git");
    remote.ends_with(&format!("/{repo}")) || remote.ends_with(&format!(":{repo}"))
}

fn git_worktrees(path: &Path) -> Vec<PathBuf> {
    let Ok(output) = git_at(path, &["worktree", "list", "--porcelain"]) else {
        return Vec::new();
    };
    output
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

fn local_checkout_roots(repo: &str) -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let dev_root = PathBuf::from(home).join("dev");
    let Ok(entries) = fs::read_dir(dev_root) else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("genny") {
            continue;
        }
        let Ok(remote) = git_at(&path, &["config", "--get", "remote.origin.url"]) else {
            continue;
        };
        if !remote_matches_repo(&remote, repo) {
            continue;
        }
        for worktree in git_worktrees(&path) {
            if !roots.contains(&worktree) {
                roots.push(worktree);
            }
        }
    }
    roots
}

fn local_source_path(app: &App) -> Option<PathBuf> {
    let mut branch_match = None;
    for root in local_checkout_roots(&app.repo) {
        let candidate = root.join(&selected_file(app).path);
        if !candidate.is_file() {
            continue;
        }
        if git_at(&root, &["rev-parse", "HEAD"])
            .ok()
            .is_some_and(|head| head.trim() == app.pull.head_ref_oid)
        {
            return Some(candidate);
        }
        if git_at(&root, &["branch", "--show-current"])
            .ok()
            .is_some_and(|branch| branch.trim() == app.pull.head_ref_name)
        {
            branch_match = Some(candidate);
        }
    }
    branch_match
}

fn open_in_editor(app: &App, workspace: &Path) -> Result<()> {
    let file = selected_file(app);
    let destination = if let Some(path) = local_source_path(app) {
        path
    } else {
        let path = workspace.join(&file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let endpoint = format!(
            "repos/{}/contents/{}?ref={}",
            app.repo, file.path, app.pull.head_ref_oid
        );
        let response = gh(&["api", &endpoint])?;
        let value: serde_json::Value = serde_json::from_str(&response)?;
        let content = value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .context("GitHub did not return file content")?
            .replace('\n', "");
        fs::write(
            &path,
            base64::engine::general_purpose::STANDARD.decode(content)?,
        )?;
        path
    };
    let line = selected_comment_target(app)
        .map(|(number, _)| number)
        .unwrap_or(1);
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
    let status = Command::new(editor)
        .arg(format!("+{line}"))
        .arg(&destination)
        .status();
    execute!(io::stdout(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    status.context("could not open editor")?;
    Ok(())
}

fn submit(app: &App, event: &str, summary: &str) -> Result<()> {
    let payload = serde_json::json!({ "commit_id": app.pull.head_ref_oid, "event": event, "body": summary, "comments": app.comments });
    let mut child = Command::new("gh")
        .args([
            "api",
            "--method",
            "POST",
            &format!("repos/{}/pulls/{}/reviews", app.repo, app.pr_number),
            "--input",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .context("could not write review")?
        .write_all(serde_json::to_string(&payload)?.as_bytes())?;
    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim())
    }
}

fn handle_browse(app: &mut App, key: KeyEvent, workspace: &Path) {
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('b') => app.return_to_picker = true,
        KeyCode::Esc => {
            if matches!(app.focus, Focus::Diff)
                && matches!(app.diff_navigation, DiffNavigation::Line)
            {
                app.diff_navigation = DiffNavigation::Block;
            } else if !app.sidebar_visible {
                app.sidebar_visible = true;
                app.focus = Focus::Files;
            } else if matches!(app.focus, Focus::Diff) {
                app.focus = Focus::Files;
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if matches!(app.focus, Focus::Files) {
                change_file(app, 1)
            } else if matches!(app.diff_navigation, DiffNavigation::Line) {
                move_line_in_block(app, 1)
            } else {
                move_change_block(app, 1)
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if matches!(app.focus, Focus::Files) {
                change_file(app, -1)
            } else if matches!(app.diff_navigation, DiffNavigation::Line) {
                move_line_in_block(app, -1)
            } else {
                move_change_block(app, -1)
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if matches!(app.focus, Focus::Files) {
                move_file(app, page_size(app));
            } else if matches!(app.diff_navigation, DiffNavigation::Line) {
                move_line_in_block(app, page_size(app));
            } else {
                move_change_block(app, page_size(app));
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if matches!(app.focus, Focus::Files) {
                move_file(app, -page_size(app));
            } else if matches!(app.diff_navigation, DiffNavigation::Line) {
                move_line_in_block(app, -page_size(app));
            } else {
                move_change_block(app, -page_size(app));
            }
        }
        KeyCode::Char('h') | KeyCode::Left => change_file(app, -1),
        KeyCode::Char('l') | KeyCode::Right => change_file(app, 1),
        KeyCode::Char('g') => {
            if matches!(app.diff_navigation, DiffNavigation::Line) {
                if let Some((start, _)) = change_block_at(selected_file(app), app.line_index) {
                    app.line_index = start;
                }
            } else {
                app.line_index = 0;
                move_change_block(app, 0);
            }
        }
        KeyCode::Char('G') => {
            if matches!(app.diff_navigation, DiffNavigation::Line) {
                if let Some((_, end)) = change_block_at(selected_file(app), app.line_index) {
                    app.line_index = end;
                }
            } else {
                move_change_block(app, isize::MAX);
            }
        }
        KeyCode::Tab => {
            if app.sidebar_visible {
                app.focus = match app.focus {
                    Focus::Files => Focus::Diff,
                    Focus::Diff => Focus::Files,
                }
            }
        }
        KeyCode::Char('/') => {
            app.editor.reset();
            app.mode = Mode::Search {
                previous_query: app.search_query.clone(),
            };
        }
        KeyCode::Char('n') if !app.search_query.is_empty() => {
            search(app, true);
        }
        KeyCode::Char('N') if !app.search_query.is_empty() => {
            search(app, false);
        }
        KeyCode::Char('d') => app.mode = Mode::Description,
        KeyCode::Char('v') => {
            let path = selected_file(app).path.clone();
            if let Err(error) = app.progress.toggle(&path) {
                app.mode = Mode::Message(error.to_string());
            }
        }
        KeyCode::Char('c') => {
            app.comment_index = app.comments.len().saturating_sub(1);
            app.mode = Mode::Comments;
        }
        KeyCode::Char('s') => {
            if app.comments.is_empty() {
                app.mode = Mode::Message("Add at least one comment before submitting.".to_string())
            } else {
                app.mode = Mode::Submit
            }
        }
        KeyCode::Enter => {
            if matches!(app.focus, Focus::Files) {
                app.sidebar_visible = false;
                app.focus = Focus::Diff;
                app.diff_navigation = DiffNavigation::Block;
                move_change_block(app, 0);
            } else if matches!(app.diff_navigation, DiffNavigation::Block) {
                if selected_block_has_one_line(app) && selected_comment_target(app).is_some() {
                    app.editor.reset();
                    app.mode = Mode::Compose;
                } else {
                    app.diff_navigation = DiffNavigation::Line;
                }
            } else if selected_comment_target(app).is_some() {
                app.editor.reset();
                app.mode = Mode::Compose
            }
        }
        KeyCode::Char('o') => match open_in_editor(app, workspace) {
            Ok(()) => app.should_redraw = true,
            Err(error) => app.mode = Mode::Message(error.to_string()),
        },
        _ => {}
    }
}

fn handle_event(app: &mut App, key: KeyEvent, workspace: &Path) {
    match &mut app.mode {
        Mode::Browse => handle_browse(app, key, workspace),
        Mode::Search { previous_query } => match app.editor.handle(key) {
            EditorAction::Cancel => {
                app.search_query = previous_query.clone();
                app.mode = Mode::Browse;
            }
            EditorAction::Submit => {
                app.search_query = app.editor.text.trim().to_string();
                app.mode = Mode::Browse;
                search(app, true);
            }
            EditorAction::Continue => {}
        },
        Mode::Description => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('d')) {
                app.mode = Mode::Browse
            }
        }
        Mode::Comments => match key.code {
            KeyCode::Esc | KeyCode::Char('c') => app.mode = Mode::Browse,
            KeyCode::Char('j') | KeyCode::Down => {
                app.comment_index =
                    (app.comment_index + 1).min(app.comments.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.comment_index = app.comment_index.saturating_sub(1);
            }
            KeyCode::Char('x') if !app.comments.is_empty() => {
                app.comments.remove(app.comment_index);
                app.comment_index = app.comment_index.min(app.comments.len().saturating_sub(1));
            }
            _ => {}
        },
        Mode::Message(_) => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                app.mode = Mode::Browse
            }
        }
        Mode::Submit => match key.code {
            KeyCode::Esc => app.mode = Mode::Browse,
            KeyCode::Char(choice @ ('a' | 'r' | 'c')) => {
                let event = match choice {
                    'a' => "APPROVE",
                    'r' => "REQUEST_CHANGES",
                    _ => "COMMENT",
                };
                app.editor.reset();
                app.mode = Mode::ReviewSummary(event);
            }
            _ => {}
        },
        Mode::ReviewSummary(event) => match app.editor.handle(key) {
            EditorAction::Cancel => app.mode = Mode::Browse,
            EditorAction::Submit => {
                let event = *event;
                let summary = app.editor.text.trim().to_string();
                match submit(app, event, &summary) {
                    Ok(()) => {
                        app.mode = Mode::Message(format!("Review submitted as {}.", event));
                        app.comments.clear();
                    }
                    Err(error) => app.mode = Mode::Message(error.to_string()),
                }
            }
            EditorAction::Continue => {}
        },
        Mode::Compose => match app.editor.handle(key) {
            EditorAction::Cancel => app.mode = Mode::Browse,
            EditorAction::Submit => {
                let body = app.editor.text.trim().to_string();
                if let Some((line, side)) = selected_comment_target(app)
                    && !body.is_empty()
                {
                    app.comments.push(PendingComment {
                        path: selected_file(app).path.clone(),
                        line,
                        side,
                        body,
                    });
                }
                app.mode = Mode::Browse;
            }
            EditorAction::Continue => {}
        },
    }
}

fn review_pull_request(
    terminal: &mut AppTerminal,
    pr_number: String,
    repo: String,
    syntax_set: &SyntaxSet,
    syntax_theme: &Theme,
) -> Result<bool> {
    let (pull_json, diff_result) = std::thread::scope(|scope| {
        let pull_request = scope.spawn(|| {
            gh(&[
                "pr",
                "view",
                &pr_number,
                "--repo",
                &repo,
                "--json",
                "title,body,author,headRefName,baseRefName,baseRefOid,headRefOid",
            ])
        });
        let diff_request = scope.spawn(|| gh(&["pr", "diff", &pr_number, "--repo", &repo]));
        (pull_request.join(), diff_request.join())
    });
    let pull_json =
        pull_json.map_err(|_| anyhow::anyhow!("pull request metadata request panicked"))??;
    let pull: PullRequest =
        serde_json::from_str(&pull_json).context("could not parse pull request")?;
    let diff_result =
        diff_result.map_err(|_| anyhow::anyhow!("pull request diff request panicked"))?;
    let diff = match diff_result {
        Ok(diff) => diff,
        Err(error) if error.to_string().contains("PullRequest.diff too_large") => {
            local_pr_diff(&pr_number, &repo, &pull).context(
                "could not generate a local diff after GitHub rejected the large pull request diff",
            )?
        }
        Err(error) => return Err(error).context("could not find pull request diff"),
    };
    let mut files = parse_diff(&diff);
    if files.is_empty() {
        bail!(
            "No reviewable diff lines found for PR #{} in {}.",
            pr_number,
            repo
        );
    }
    highlight_files(&mut files, syntax_set, syntax_theme);
    let progress = ReviewProgress::load(&repo, &pr_number, &pull.head_ref_oid)?;
    let mut app = App {
        pr_number,
        repo,
        pull,
        files,
        file_index: 0,
        line_index: 0,
        focus: Focus::Files,
        diff_navigation: DiffNavigation::Block,
        mode: Mode::Browse,
        editor: TextEditor::new(),
        comments: Vec::new(),
        comment_index: 0,
        should_quit: false,
        return_to_picker: false,
        should_redraw: false,
        sidebar_visible: true,
        files_state: ListState::default(),
        search_query: String::new(),
        progress,
    };
    app.files_state.select(Some(0));
    move_change_block(&mut app, 0);
    let workspace = tempfile::tempdir().context("could not create editor workspace")?;
    loop {
        terminal.draw(|frame| draw(&mut app, frame))?;
        if app.should_quit {
            return Ok(false);
        }
        if app.return_to_picker {
            return Ok(true);
        }
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == event::KeyEventKind::Press
        {
            handle_event(&mut app, key, workspace.path());
            if app.should_redraw {
                terminal.clear()?;
                app.should_redraw = false;
            }
        }
    }
}

fn run_in_terminal(
    repo: String,
    mut pulls: Vec<PullRequestChoice>,
    initial_pr: Option<String>,
) -> Result<()> {
    let mut contributors = repository_contributors(&repo, &pulls);
    let syntax_set = SyntaxSet::load_defaults_newlines();
    let syntax_theme = ThemeSet::load_defaults()
        .themes
        .remove("base16-ocean.dark")
        .context("could not load default syntax theme")?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let outcome: Result<()> = (|| {
        let mut selected = 0usize;
        let mut next_pr = initial_pr;
        let mut picker_query = "is:open".to_string();
        loop {
            let pr_number = match next_pr.take() {
                Some(pr_number) => {
                    if let Some(index) = pulls
                        .iter()
                        .position(|pull| pull.number.to_string() == pr_number)
                    {
                        selected = index;
                    }
                    pr_number
                }
                None => match pick_pull_request(
                    &mut terminal,
                    &repo,
                    &mut pulls,
                    &mut contributors,
                    &mut picker_query,
                    selected,
                )? {
                    Some((pr_number, index)) => {
                        selected = index;
                        pr_number
                    }
                    None => return Ok(()),
                },
            };
            if !review_pull_request(
                &mut terminal,
                pr_number,
                repo.clone(),
                &syntax_set,
                &syntax_theme,
            )? {
                return Ok(());
            }
        }
    })();
    let leave_result = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let raw_mode_result = disable_raw_mode();
    outcome?;
    leave_result?;
    raw_mode_result?;
    Ok(())
}

fn parse_cli_args(args: &[String]) -> Result<(Option<String>, Option<String>)> {
    let mut repo = None;
    let mut pr_number = None;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if matches!(argument.as_str(), "--repo" | "-R") {
            index += 1;
            let value = args
                .get(index)
                .filter(|value| !value.is_empty())
                .context("--repo requires a repository in owner/name format")?;
            if repo.replace(value.clone()).is_some() {
                bail!("--repo may only be supplied once");
            }
        } else if let Some(value) = argument.strip_prefix("--repo=") {
            if value.is_empty() {
                bail!("--repo requires a repository in owner/name format");
            }
            if repo.replace(value.to_string()).is_some() {
                bail!("--repo may only be supplied once");
            }
        } else if argument.starts_with('-') {
            bail!("unknown option: {argument}");
        } else if pr_number.replace(argument.clone()).is_some() {
            bail!("only one pull-request number may be supplied");
        }
        index += 1;
    }
    Ok((repo, pr_number))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (repo_override, pr_number) = parse_cli_args(&args).map_err(|error| {
        anyhow::anyhow!("{error}\nusage: reviewer [--repo <owner/name>] [<pr-number>]")
    })?;
    let repo = match repo_override {
        Some(repo) => repo,
        None => current_repo()?,
    };
    let pulls = open_pull_requests(&repo)?;
    run_in_terminal(repo, pulls, pr_number)
}

#[cfg(test)]
mod tests {
    use super::{
        App, Author, Color, DiffNavigation, Focus, LineKind, ListState, Mode, PullRequest,
        ReviewProgress, TextEditor, active_author_prefix, author_suggestions, change_block_at,
        complete_author, editor_overlay, handle_browse, highlight_files, line_matches,
        parse_cli_args, parse_diff, search,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use std::path::Path;
    use syntect::highlighting::ThemeSet;
    use syntect::parsing::SyntaxSet;

    #[test]
    fn parses_both_sides_of_a_unified_diff() {
        let files = parse_diff(
            "diff --git a/src/main.rs b/src/main.rs\nindex 123..456 100644\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -4,2 +4,2 @@\n context\n-removed\n+added\n",
        );
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/main.rs");
        assert_eq!(files[0].lines[2].old_line, Some(4));
        assert_eq!(files[0].lines[2].new_line, Some(4));
        assert_eq!(files[0].lines[3].old_line, Some(5));
        assert_eq!(files[0].lines[3].new_line, None);
        assert_eq!(files[0].lines[3].kind, LineKind::Remove);
        assert_eq!(files[0].lines[4].old_line, None);
        assert_eq!(files[0].lines[4].new_line, Some(5));
        assert_eq!(files[0].lines[4].kind, LineKind::Add);
    }

    #[test]
    fn highlights_rust_code() {
        let mut files = parse_diff(
            "diff --git a/src/main.rs b/src/main.rs\n@@ -1,1 +1,1 @@\n-fn old() {}\n+fn main() { println!(\"review\"); }\n",
        );
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let syntax_theme = ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .expect("default syntax theme exists");
        highlight_files(&mut files, &syntax_set, &syntax_theme);
        assert!(files[0].syntax_lines[1].len() > 1);
        assert!(
            files[0].syntax_lines[1]
                .iter()
                .any(|span| span.color != Color::White)
        );
    }

    #[test]
    fn highlights_typescript_react_code() {
        let mut files = parse_diff(
            "diff --git a/src/Button.tsx b/src/Button.tsx\n@@ -1,1 +1,1 @@\n-export const oldButton = () => null;\n+export function Button({ label }: { label: string }) { return <button>{label}</button>; }\n",
        );
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let syntax_theme = ThemeSet::load_defaults()
            .themes
            .remove("base16-ocean.dark")
            .expect("default syntax theme exists");
        highlight_files(&mut files, &syntax_set, &syntax_theme);
        assert!(files[0].syntax_lines[1].len() > 1);
        assert!(
            files[0].syntax_lines[1]
                .iter()
                .any(|span| span.color != Color::White)
        );
    }

    #[test]
    fn highlights_removed_lines_with_a_dark_red_background() {
        assert_eq!(
            super::line_style(LineKind::Remove).bg,
            Some(super::REMOVED_BACKGROUND)
        );
    }

    #[test]
    fn highlights_added_lines_with_a_dark_green_background() {
        assert_eq!(
            super::line_style(LineKind::Add).bg,
            Some(super::ADDED_BACKGROUND)
        );
    }

    #[test]
    fn search_moves_between_matches_in_both_directions() {
        let files = parse_diff(
            "diff --git a/one.rs b/one.rs\n@@ -1 +1,2 @@\n+needle first\n+companion line\ndiff --git a/two.rs b/two.rs\n@@ -1 +1 @@\n+needle second\n",
        );
        let mut app = App {
            pr_number: "1".to_string(),
            repo: "owner/repo".to_string(),
            pull: PullRequest {
                title: String::new(),
                body: String::new(),
                author: Author {
                    login: String::new(),
                },
                head_ref_name: String::new(),
                base_ref_name: String::new(),
                base_ref_oid: String::new(),
                head_ref_oid: String::new(),
            },
            files,
            file_index: 0,
            line_index: 0,
            focus: Focus::Files,
            diff_navigation: DiffNavigation::Block,
            mode: Mode::Browse,
            editor: TextEditor::new(),
            comments: Vec::new(),
            comment_index: 0,
            should_quit: false,
            return_to_picker: false,
            should_redraw: false,
            sidebar_visible: true,
            files_state: ListState::default(),
            search_query: "needle".to_string(),
            progress: ReviewProgress::load("owner/repo", "1", "head").unwrap(),
        };
        assert!(search(&mut app, true));
        assert_eq!((app.file_index, app.line_index), (0, 1));
        assert!(search(&mut app, true));
        assert_eq!((app.file_index, app.line_index), (1, 1));
        assert!(search(&mut app, false));
        assert_eq!((app.file_index, app.line_index), (0, 1));
        assert!(line_matches(&app.files[0].lines[1], "needle"));

        app.focus = Focus::Files;
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(!app.sidebar_visible);
        assert!(matches!(app.focus, Focus::Diff));
        assert!(matches!(app.diff_navigation, DiffNavigation::Block));

        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(matches!(app.diff_navigation, DiffNavigation::Line));

        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(matches!(app.focus, Focus::Diff));
        assert!(matches!(app.diff_navigation, DiffNavigation::Block));

        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(app.sidebar_visible);
        assert!(matches!(app.focus, Focus::Files));

        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            Path::new("."),
        );
        assert_eq!(app.line_index, 1);
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Path::new("."),
        );
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(matches!(app.mode, Mode::Compose));
    }

    #[test]
    fn groups_adjacent_additions_and_removals_into_change_blocks() {
        let files = parse_diff(
            "diff --git a/example.rs b/example.rs\n@@ -1,4 +1,5 @@\n context\n-old one\n-old two\n+new one\n+new two\n context\n+another change\n context\n",
        );

        assert_eq!(change_block_at(&files[0], 2), Some((2, 5)));
        assert_eq!(change_block_at(&files[0], 5), Some((2, 5)));
        assert_eq!(change_block_at(&files[0], 6), None);
        assert_eq!(change_block_at(&files[0], 7), Some((7, 7)));
    }

    #[test]
    fn compacts_deep_paths_without_hiding_the_filename() {
        assert_eq!(
            super::compact_path(
                "backend/services/opportunities/create-opportunity-card.tsx",
                48
            ),
            "backend/../create-opportunity-card.tsx"
        );
        assert_eq!(
            super::compact_path(
                "backend/services/opportunities/create-opportunity-card.tsx",
                24
            ),
            "backend/…/nity-card.tsx"
        );
    }

    #[test]
    fn recognizes_common_github_remote_formats() {
        assert!(super::remote_matches_repo(
            "git@github.com:acme/genny.git",
            "acme/genny"
        ));
        assert!(super::remote_matches_repo(
            "https://github.com/acme/genny",
            "acme/genny"
        ));
        assert!(!super::remote_matches_repo(
            "git@github.com:acme/other.git",
            "acme/genny"
        ));
    }

    #[test]
    fn input_editor_supports_vim_normal_mode_edits() {
        let mut editor = TextEditor::new();
        for value in ['a', 'b', 'c'] {
            editor.handle(KeyEvent::new(KeyCode::Char(value), KeyModifiers::NONE));
        }
        editor.handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        editor.handle(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        editor.handle(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        editor.handle(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        editor.handle(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));

        assert_eq!(editor.text, "abz");
    }

    #[test]
    fn input_editor_supports_vim_change_line() {
        let mut editor = TextEditor::new();
        editor.text = "keep\nreplace me\nkeep too".to_string();
        editor.cursor = "keep\nreplace".len();
        editor.mode = super::EditorMode::Normal;

        editor.handle(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        editor.handle(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        editor.handle(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));

        assert_eq!(editor.text, "keep\nn\nkeep too");
        assert!(matches!(editor.mode, super::EditorMode::Insert));
    }

    #[test]
    fn input_editor_supports_vim_inner_word_operators() {
        let mut editor = TextEditor::new();
        editor.text = "alpha target omega".to_string();
        editor.cursor = "alpha tar".len();
        editor.mode = super::EditorMode::Normal;

        for key in ['c', 'i', 'w'] {
            editor.handle(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        assert_eq!(editor.text, "alpha  omega");
        assert_eq!(editor.cursor, "alpha ".len());
        assert!(matches!(editor.mode, super::EditorMode::Insert));

        editor.text = "alpha target omega".to_string();
        editor.cursor = "alpha tar".len();
        editor.mode = super::EditorMode::Normal;
        for key in ['d', 'i', 'w'] {
            editor.handle(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        assert_eq!(editor.text, "alpha  omega");
        assert!(matches!(editor.mode, super::EditorMode::Normal));
    }

    #[test]
    fn input_editor_supports_vim_word_and_line_delete_operators() {
        let mut editor = TextEditor::new();
        editor.text = "one two three".to_string();
        editor.cursor = "one ".len();
        editor.mode = super::EditorMode::Normal;
        for key in ['d', 'w'] {
            editor.handle(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        assert_eq!(editor.text, "one  three");

        editor.text = "keep\ndelete\nkeep too".to_string();
        editor.cursor = "keep\ndelete".len();
        for key in ['d', 'd'] {
            editor.handle(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        assert_eq!(editor.text, "keep\nkeep too");
    }

    #[test]
    fn editor_overlay_expands_only_after_wrapping() {
        let mut editor = TextEditor::new();
        let area = Rect::new(0, 0, 100, 40);
        assert_eq!(editor_overlay(area, &editor).height, 3);

        editor.text = "x".repeat(79);
        assert_eq!(editor_overlay(area, &editor).height, 4);
    }

    #[test]
    fn parses_pull_request_choices_and_current_repo() {
        let pulls: Vec<super::PullRequestChoice> = serde_json::from_str(
            r#"[{"number":447,"title":"A focused picker","author":{"login":"octocat"},"headRefName":"picker"}]"#,
        )
        .unwrap();
        let repo: super::Repository =
            serde_json::from_str(r#"{"nameWithOwner":"dlvhdr/gh-dash"}"#).unwrap();
        assert_eq!(pulls[0].number, 447);
        assert_eq!(pulls[0].title, "A focused picker");
        assert_eq!(pulls[0].author.login, "octocat");
        assert_eq!(pulls[0].head_ref_name, "picker");
        assert_eq!(repo.name_with_owner, "dlvhdr/gh-dash");
    }

    #[test]
    fn completes_author_qualifiers_from_contributors() {
        let mut editor = TextEditor::new();
        editor.text = "is:open author:oc".to_string();
        editor.cursor = editor.text.len();
        let contributors = vec!["alice".to_string(), "octocat".to_string()];

        assert_eq!(active_author_prefix(&editor), Some((8, "oc")));
        assert_eq!(author_suggestions(&editor, &contributors), vec!["octocat"]);

        complete_author(&mut editor, "octocat");
        assert_eq!(editor.text, "is:open author:octocat ");
        assert_eq!(editor.cursor, editor.text.len());
    }

    #[test]
    fn parses_remote_repository_cli_options_in_either_order() {
        let args = ["--repo", "octo/repo", "42"].map(str::to_string);
        assert_eq!(
            parse_cli_args(&args).unwrap(),
            (Some("octo/repo".to_string()), Some("42".to_string()))
        );

        let args = ["42", "-R", "octo/repo"].map(str::to_string);
        assert_eq!(
            parse_cli_args(&args).unwrap(),
            (Some("octo/repo".to_string()), Some("42".to_string()))
        );

        let args = ["--repo=octo/repo"].map(str::to_string);
        assert_eq!(
            parse_cli_args(&args).unwrap(),
            (Some("octo/repo".to_string()), None)
        );
    }
}
