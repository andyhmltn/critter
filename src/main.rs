use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
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
use vimltui::{
    EditorAction as VimEditorAction, Operator, VimEditor, VimMode, VimModeConfig, VisualKind,
};

mod briefing;
mod cache;
mod flow;
mod progress;

use briefing::BriefingEvent;
use flow::{FlowEvent, FlowPlan, FlowStep};
use progress::{ReviewProgress, load_picker_query, save_picker_query};

type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;

const ACCENT: Color = Color::Cyan;
const ADDED: Color = Color::Green;
const ADDED_BACKGROUND: Color = Color::Rgb(34, 48, 34);
const REMOVED: Color = Color::Red;
const REMOVED_BACKGROUND: Color = Color::Rgb(48, 34, 34);
const MUTED: Color = Color::DarkGray;
const SEARCH_MATCH: Color = Color::Yellow;
const REPORT_EVIDENCE_BACKGROUND: Color = Color::Rgb(55, 42, 76);
const LINE_SELECTION_BACKGROUND: Color = Color::Rgb(0, 82, 96);

const CLI_COMMANDS: &[&str] = &[
    "reviewer [OPTIONS] [PR_NUMBER]",
    "reviewer local [--base REVISION | --unstaged | --last-commit] [--session SESSION_ID]",
    "reviewer local-tmux [--base REVISION | --unstaged | --last-commit] [--wait]",
    "reviewer pr-tmux [PR_NUMBER | --unstaged | --last-commit] [--wait]",
    "reviewer codex-tmux [PR_NUMBER | --unstaged | --last-commit | --unstaged-or-pr]",
    "reviewer peer-review --repo OWNER/NAME PR_NUMBER",
    "reviewer peer-review-status --repo OWNER/NAME PR_NUMBER",
];
const CLI_OPTIONS: &str =
    "  -R, --repo OWNER/NAME  Review a repository other than the current checkout
  -h, --help             Print help
  -V, --version          Print version";

fn cli_usage() -> String {
    let commands = CLI_COMMANDS
        .iter()
        .map(|command| format!("  {command}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Reviewer — interactive GitHub pull-request review in the terminal\n\nUsage:\n{commands}\n\nOptions:\n{CLI_OPTIONS}"
    )
}

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

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
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

#[derive(Clone, Deserialize, Serialize)]
struct PendingComment {
    path: String,
    line: u32,
    side: Side,
    body: String,
}

#[derive(Clone)]
struct LocalReview {
    workspace: PathBuf,
    session_id: Option<String>,
    base_revision: String,
    review_id: String,
}

#[derive(Deserialize, Serialize)]
struct LocalReviewSubmission {
    version: u8,
    review_id: String,
    workspace: PathBuf,
    session_id: Option<String>,
    base_revision: String,
    submitted_at: u64,
    summary: String,
    comments: Vec<PendingComment>,
    diff: String,
}

#[derive(Deserialize, Serialize)]
struct ActiveCodexSession {
    session_id: String,
    workspace: PathBuf,
    updated_at: u64,
}

enum Focus {
    Description,
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
    Command,
    Compose,
    Submit,
    ReviewSummary(&'static str),
    Comments,
    Message(String),
}

enum BriefingState {
    Idle,
    Loading {
        receiver: std::sync::mpsc::Receiver<BriefingEvent>,
        symbols: Vec<briefing::SymbolChange>,
        transcript: String,
    },
    Chat(String),
    Failed {
        error: String,
        transcript: String,
    },
}

enum SectionState {
    Ready(String),
}

struct ReportSection {
    title: &'static str,
    state: SectionState,
}

#[derive(Clone)]
struct CodeReference {
    raw: String,
    path: String,
    start: u32,
    end: u32,
}

fn changed_line_citation(file: &ChangedFile, index: usize) -> String {
    let line = &file.lines[index];
    format!(
        "`{}:{}`",
        file.path,
        line.new_line.or(line.old_line).unwrap_or(1)
    )
}

fn matching_changes(files: &[ChangedFile], terms: &[&str]) -> Vec<String> {
    files
        .iter()
        .flat_map(|file| {
            file.lines
                .iter()
                .enumerate()
                .filter_map(move |(index, line)| {
                    let lower = line.text.to_ascii_lowercase();
                    (is_change_line(line) && terms.iter().any(|term| lower.contains(term))).then(
                        || {
                            format!(
                                "- {} {}",
                                line.text.trim().chars().take(90).collect::<String>(),
                                changed_line_citation(file, index)
                            )
                        },
                    )
                })
        })
        .take(24)
        .collect()
}

fn report_sections(files: &[ChangedFile], diff: &str) -> Vec<ReportSection> {
    let added = files
        .iter()
        .flat_map(|file| &file.lines)
        .filter(|line| line.kind == LineKind::Add)
        .count();
    let removed = files
        .iter()
        .flat_map(|file| &file.lines)
        .filter(|line| line.kind == LineKind::Remove)
        .count();
    let symbols = briefing::analyze_symbols(diff);
    let symbol_text = if symbols.is_empty() {
        "No changed top-level declarations were detected.".to_string()
    } else {
        symbols
            .iter()
            .map(|symbol| format!("- {} `{}` — `{}`", symbol.kind, symbol.name, symbol.path))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut hotspots = files
        .iter()
        .enumerate()
        .flat_map(|(file_index, file)| {
            file.lines
                .iter()
                .enumerate()
                .filter_map(move |(index, line)| {
                    if !is_change_line(line)
                        || (index > 0 && is_change_line(&file.lines[index - 1]))
                    {
                        return None;
                    }
                    let (_, end) = change_block_at(file, index)?;
                    Some((end - index + 1, file_index, index))
                })
        })
        .collect::<Vec<_>>();
    hotspots.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let hotspot_text = hotspots
        .iter()
        .take(12)
        .map(|(size, file_index, index)| {
            let file = &files[*file_index];
            format!(
                "- **{} changed lines** — {} {}",
                size,
                file.lines[*index]
                    .text
                    .trim()
                    .chars()
                    .take(72)
                    .collect::<String>(),
                changed_line_citation(file, *index)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let contracts = matching_changes(
        files,
        &[
            "pub ",
            "export ",
            "interface ",
            "struct ",
            "enum ",
            "type ",
            "schema",
            "serde",
            "request",
            "response",
            "config",
            "dependency",
        ],
    );
    let flow = matching_changes(
        files,
        &[
            "if ",
            "else",
            "match ",
            "await",
            "return",
            "spawn",
            "send",
            "lock",
            "transaction",
            "error",
            "retry",
            "timeout",
        ],
    );
    let tests = files
        .iter()
        .filter(|file| {
            let path = file.path.to_ascii_lowercase();
            path.contains("test") || path.contains("spec")
        })
        .map(|file| format!("- `{}`", file.path))
        .collect::<Vec<_>>();
    let risks = matching_changes(
        files,
        &[
            "unsafe",
            "unwrap",
            "expect(",
            "panic",
            "delete",
            "remove",
            "permission",
            "auth",
            "migration",
            "secret",
            "token",
            "sql",
            "concurrent",
        ],
    );
    let ready = |title, text| ReportSection {
        title,
        state: SectionState::Ready(text),
    };
    vec![
        ready(
            "Change map",
            format!(
                "## Scope\n\n- {} files, **+{} / -{}** lines\n\n## Changed declarations\n\n{}",
                files.len(),
                added,
                removed,
                symbol_text
            ),
        ),
        ready(
            "Hotspots",
            if hotspot_text.is_empty() {
                "No changed blocks found.".to_string()
            } else {
                format!(
                    "Largest change blocks first. Size is a review-order signal, not a risk verdict.\n\n{hotspot_text}"
                )
            },
        ),
        ready(
            "Contracts",
            if contracts.is_empty() {
                "No likely contract or public-surface changes detected.".to_string()
            } else {
                contracts.join("\n")
            },
        ),
        ready(
            "Control flow",
            if flow.is_empty() {
                "No changed control-flow markers detected.".to_string()
            } else {
                flow.join("\n")
            },
        ),
        ready(
            "Tests",
            if tests.is_empty() {
                "No test files changed. Verify whether production behavior changed without corresponding coverage.".to_string()
            } else {
                format!("Changed test files:\n\n{}", tests.join("\n"))
            },
        ),
        ready(
            "Risk signals",
            if risks.is_empty() {
                "No high-risk lexical signals detected. This is not proof that the change is safe."
                    .to_string()
            } else {
                risks.join("\n")
            },
        ),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorMode {
    Insert,
    Normal,
    Visual,
    Replace,
}

struct TextEditor {
    text: String,
    cursor: usize,
    mode: EditorMode,
    engine: VimEditor,
    pending_visual_inner: bool,
}

enum EditorAction {
    Continue,
    Submit,
    Cancel,
}

impl TextEditor {
    fn new() -> Self {
        let mut editor = Self {
            text: String::new(),
            cursor: 0,
            mode: EditorMode::Insert,
            engine: VimEditor::new_empty(VimModeConfig::default()),
            pending_visual_inner: false,
        };
        editor.sync_to_engine();
        editor
    }

    fn reset(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.mode = EditorMode::Insert;
        self.engine = VimEditor::new_empty(VimModeConfig::default());
        self.pending_visual_inner = false;
        self.sync_to_engine();
    }

    fn sync_to_engine(&mut self) {
        if self.engine.content() != self.text {
            self.engine.set_content(&self.text);
        }
        let cursor = self.cursor.min(self.text.len());
        let line_start = self.text[..cursor].rfind('\n').map_or(0, |index| index + 1);
        self.engine.cursor_row = self.text[..cursor]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        self.engine.cursor_col = cursor - line_start;
        let engine_mode_matches = matches!(
            (&self.mode, &self.engine.mode),
            (EditorMode::Insert, VimMode::Insert)
                | (EditorMode::Normal, VimMode::Normal)
                | (EditorMode::Visual, VimMode::Visual(_))
                | (EditorMode::Replace, VimMode::Replace)
        );
        if !engine_mode_matches {
            self.engine.mode = match self.mode {
                EditorMode::Insert => VimMode::Insert,
                EditorMode::Normal | EditorMode::Visual => VimMode::Normal,
                EditorMode::Replace => VimMode::Replace,
            };
        }
        self.engine.clamp_cursor();
    }

    fn sync_from_engine(&mut self) {
        self.text = self.engine.content();
        self.cursor = self.engine.lines[..self.engine.cursor_row]
            .iter()
            .map(|line| line.len() + 1)
            .sum::<usize>()
            + self.engine.cursor_col;
        self.mode = match self.engine.mode {
            VimMode::Insert => EditorMode::Insert,
            VimMode::Normal => EditorMode::Normal,
            VimMode::Visual(_) => EditorMode::Visual,
            VimMode::Replace => EditorMode::Replace,
        };
    }

    fn select_inner_word(&mut self) {
        let row = self.engine.cursor_row;
        let line = &self.engine.lines[row];
        if line.is_empty() {
            return;
        }
        let cursor = self.engine.cursor_col.min(line.len().saturating_sub(1));
        let cursor = line
            .char_indices()
            .take_while(|(index, _)| *index <= cursor)
            .map(|(index, _)| index)
            .last()
            .unwrap_or(0);
        let class = |character: char| {
            if character.is_alphanumeric() || character == '_' {
                0
            } else if character.is_whitespace() {
                1
            } else {
                2
            }
        };
        let target_class = class(line[cursor..].chars().next().unwrap_or(' '));
        let mut start = cursor;
        while start > 0 {
            let previous = line[..start]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i);
            if class(line[previous..].chars().next().unwrap_or(' ')) != target_class {
                break;
            }
            start = previous;
        }
        let mut end = cursor;
        while end < line.len() {
            let character = line[end..].chars().next().unwrap_or(' ');
            if class(character) != target_class {
                break;
            }
            end += character.len_utf8();
        }
        let last = line[..end]
            .char_indices()
            .next_back()
            .map_or(start, |(index, _)| index);
        self.engine.visual_anchor = Some((row, start));
        self.engine.cursor_col = last;
        self.sync_from_engine();
    }

    fn handle(&mut self, mut key: KeyEvent) -> EditorAction {
        self.sync_to_engine();
        if matches!(self.engine.mode, VimMode::Visual(VisualKind::Char)) {
            if self.pending_visual_inner {
                self.pending_visual_inner = false;
                if matches!(key.code, KeyCode::Char('w')) {
                    self.select_inner_word();
                    return EditorAction::Continue;
                }
            }
            if matches!(key.code, KeyCode::Char('i')) {
                self.pending_visual_inner = true;
                return EditorAction::Continue;
            }
        } else {
            self.pending_visual_inner = false;
        }
        if matches!(self.mode, EditorMode::Insert) && matches!(key.code, KeyCode::Enter) {
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL)
            {
                key.modifiers
                    .remove(KeyModifiers::SHIFT | KeyModifiers::CONTROL);
            } else {
                return EditorAction::Submit;
            }
        }
        if matches!(self.engine.mode, VimMode::Normal)
            && matches!(self.engine.pending_operator, Some(Operator::Change))
            && matches!(key.code, KeyCode::Char('c'))
        {
            self.engine.save_undo();
            self.engine.lines[self.engine.cursor_row].clear();
            self.engine.cursor_col = 0;
            self.engine.pending_operator = None;
            self.engine.mode = VimMode::Insert;
            self.engine.modified = true;
            self.sync_from_engine();
            return EditorAction::Continue;
        }
        let change_to_line_end = matches!(
            (&self.engine.pending_operator, key.code),
            (Some(Operator::Change), KeyCode::Char('$'))
        );
        let operator_cursor = (self.engine.cursor_row, self.engine.cursor_col);
        let was_normal = matches!(self.engine.mode, VimMode::Normal);
        let action = self.engine.handle_key(key);
        if change_to_line_end && matches!(self.engine.mode, VimMode::Insert) {
            self.engine.cursor_row = operator_cursor.0;
            self.engine.cursor_col = operator_cursor
                .1
                .min(self.engine.lines[operator_cursor.0].len());
        }
        self.sync_from_engine();
        match action {
            VimEditorAction::Unhandled(event)
                if was_normal && matches!(event.code, KeyCode::Esc) =>
            {
                EditorAction::Cancel
            }
            VimEditorAction::Close | VimEditorAction::ForceClose => EditorAction::Cancel,
            VimEditorAction::Save | VimEditorAction::SaveAndClose => EditorAction::Submit,
            _ => EditorAction::Continue,
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
    description_expanded: bool,
    description_scroll: u16,
    diff_scroll: usize,
    diff_view_height: usize,
    center_diff: bool,
    pending_z: bool,
    pending_space: bool,
    auto_line_mode_pending: bool,
    line_mode_locked: bool,
    files_state: ListState,
    search_query: String,
    progress: ReviewProgress,
    briefing_open: bool,
    briefing_state: BriefingState,
    briefing_scroll: u16,
    briefing_diff: String,
    briefing_target: usize,
    return_to_briefing: bool,
    report_sections: Vec<ReportSection>,
    report_section: usize,
    report_content_focus: bool,
    report_reference: usize,
    report_highlight: Option<(usize, u32, u32)>,
    flow_view: bool,
    flow_index: usize,
    flow_detail_scroll: usize,
    flow_state: FlowState,
    local_review: Option<LocalReview>,
}

enum FlowState {
    Ready(FlowPlan),
    Absent,
    Failed(String),
}

fn load_flow_analysis(repo: &str, pr: &str, head: &str) -> FlowState {
    match flow::load(repo, pr, head) {
        Ok(Some(plan)) => FlowState::Ready(plan),
        Ok(None) => FlowState::Absent,
        Err(error) => FlowState::Failed(format!("{error:#}")),
    }
}

fn activate_flow_step(app: &mut App, amount: isize) -> bool {
    let FlowState::Ready(plan) = &app.flow_state else {
        return false;
    };
    if plan.steps.is_empty() {
        return false;
    }
    let previous = app.flow_index;
    app.flow_index = app
        .flow_index
        .saturating_add_signed(amount)
        .min(plan.steps.len() - 1);
    if app.flow_index != previous {
        app.flow_detail_scroll = 0;
    }
    let Some(location) = plan.steps[app.flow_index]
        .locations
        .iter()
        .find(|location| app.files.iter().any(|file| file.path == location.path))
        .cloned()
    else {
        return true;
    };
    let Some(file_index) = app.files.iter().position(|file| file.path == location.path) else {
        return true;
    };
    app.file_index = file_index;
    app.files_state.select(Some(file_index));
    app.line_index = app.files[file_index]
        .lines
        .iter()
        .position(|line| {
            line.new_line
                .or(line.old_line)
                .is_some_and(|line| line >= location.line)
        })
        .unwrap_or(0);
    app.diff_navigation = DiffNavigation::Block;
    app.center_diff = true;
    true
}

fn flow_step_detail_lines(step: &FlowStep, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        "   CHANGE",
        Style::default().fg(Color::LightCyan),
    )];
    lines.extend(
        wrap_words(&step.summary, width)
            .into_iter()
            .map(|line| Line::raw(format!("   {line}"))),
    );
    lines.push(Line::styled(
        "   WHY NEXT",
        Style::default().fg(Color::LightCyan),
    ));
    lines.extend(
        wrap_words(&step.why_now, width)
            .into_iter()
            .map(|line| Line::raw(format!("   {line}"))),
    );
    lines.push(Line::styled(
        "   REVIEW",
        Style::default().fg(Color::LightCyan),
    ));
    for location in &step.locations {
        let check = format!(
            "• {} ({}:{})",
            location.reason,
            compact_path(&location.path, 24),
            location.line
        );
        lines.extend(
            wrap_words(&check, width)
                .into_iter()
                .map(|line| Line::raw(format!("   {line}"))),
        );
    }
    lines
}

fn flow_step_title_lines(index: usize, title: &str, width: usize) -> Vec<String> {
    let prefix = format!("{}. ", index + 1);
    let indent = " ".repeat(prefix.chars().count());
    wrap_words(title, width.saturating_sub(prefix.chars().count()))
        .into_iter()
        .enumerate()
        .map(|(line_index, line)| {
            if line_index == 0 {
                format!("{prefix}{line}")
            } else {
                format!("{indent}{line}")
            }
        })
        .collect()
}

struct ReviewTarget {
    file_index: usize,
    line_index: usize,
    label: String,
}

fn review_targets(app: &App) -> Vec<ReviewTarget> {
    let mut targets = Vec::new();
    for (file_index, file) in app.files.iter().enumerate() {
        for (line_index, line) in file.lines.iter().enumerate() {
            if is_change_line(line)
                && (line_index == 0 || !is_change_line(&file.lines[line_index - 1]))
            {
                targets.push(ReviewTarget {
                    file_index,
                    line_index,
                    label: line.text.trim().chars().take(42).collect(),
                });
            }
        }
    }
    targets
}

fn open_briefing_target(app: &mut App) {
    let targets = review_targets(app);
    let Some(target) = targets.get(app.briefing_target) else {
        return;
    };
    app.file_index = target.file_index;
    app.files_state.select(Some(target.file_index));
    app.line_index = target.line_index;
    app.sidebar_visible = false;
    app.focus = Focus::Diff;
    app.return_to_briefing = true;
    app.briefing_open = false;
}

fn local_report_skeleton(app: &App) -> String {
    let added = app
        .files
        .iter()
        .flat_map(|file| &file.lines)
        .filter(|line| matches!(line.kind, LineKind::Add))
        .count();
    let removed = app
        .files
        .iter()
        .flat_map(|file| &file.lines)
        .filter(|line| matches!(line.kind, LineKind::Remove))
        .count();
    let symbols = briefing::analyze_symbols(&app.briefing_diff);
    let tests = app
        .files
        .iter()
        .filter(|file| {
            let path = file.path.to_ascii_lowercase();
            path.contains("test") || path.contains("spec")
        })
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    let symbol_lines = if symbols.is_empty() {
        "No changed top-level signatures detected.".to_string()
    } else {
        symbols
            .iter()
            .map(|symbol| format!("- {} {} ({})", symbol.kind, symbol.name, symbol.path))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let test_lines = if tests.is_empty() {
        "No test files changed.".to_string()
    } else {
        tests.join(", ")
    };
    format!(
        "REVIEW BRIEFING\n\nChange surface\n- {} files, +{} / -{} lines\n- {} → {}\n\nChanged signatures\n{}\n\nTest surface\n{}\n\nPI enrichment (streaming)\n",
        app.files.len(),
        added,
        removed,
        app.pull.head_ref_name,
        app.pull.base_ref_name,
        symbol_lines,
        test_lines
    )
}

fn start_briefing(app: &mut App) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let title = app.pull.title.clone();
    let body = app.pull.body.clone();
    let diff = app.briefing_diff.clone();
    app.briefing_state = BriefingState::Loading {
        receiver,
        symbols: briefing::analyze_symbols(&diff),
        transcript: local_report_skeleton(app),
    };
    std::thread::spawn(move || {
        briefing::generate_stream(
            "Overview",
            "Summarize the PR.",
            &title,
            &body,
            &diff,
            &sender,
        );
    });
}

fn poll_briefing(app: &mut App) {
    let events = match &app.briefing_state {
        BriefingState::Loading { receiver, .. } => receiver.try_iter().collect::<Vec<_>>(),
        _ => return,
    };
    for event in events {
        match event {
            BriefingEvent::Delta(delta) => {
                if let BriefingState::Loading { transcript, .. } = &mut app.briefing_state {
                    transcript.push_str(&delta);
                }
            }
            BriefingEvent::Complete(result) => {
                let transcript = match &app.briefing_state {
                    BriefingState::Loading { transcript, .. } => transcript.clone(),
                    _ => String::new(),
                };
                app.briefing_state = match result {
                    Ok(_) => BriefingState::Chat(transcript),
                    Err(error) => BriefingState::Failed { error, transcript },
                };
            }
        }
    }
}

fn section_text(section: &ReportSection) -> &str {
    match &section.state {
        SectionState::Ready(text) => text,
    }
}

fn report_references(app: &App) -> Vec<CodeReference> {
    let text = section_text(&app.report_sections[app.report_section]);
    let mut references = Vec::new();
    for token in text.split('`').skip(1).step_by(2) {
        for file in &app.files {
            let Some(lines) = token
                .strip_prefix(&file.path)
                .and_then(|rest| rest.strip_prefix(':'))
            else {
                continue;
            };
            let mut parts = lines.splitn(2, '-');
            let Some(start) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
                continue;
            };
            let end = parts
                .next()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(start);
            references.push(CodeReference {
                raw: token.to_string(),
                path: file.path.clone(),
                start: start.min(end),
                end: start.max(end),
            });
            break;
        }
    }
    references
}

fn jump_to_report_reference(app: &mut App) {
    let references = report_references(app);
    let Some(reference) = references.get(app.report_reference) else {
        return;
    };
    let Some(file_index) = app
        .files
        .iter()
        .position(|file| file.path == reference.path)
    else {
        return;
    };
    app.file_index = file_index;
    app.files_state.select(Some(file_index));
    app.line_index = app.files[file_index]
        .lines
        .iter()
        .position(|line| {
            line.new_line
                .or(line.old_line)
                .is_some_and(|number| (reference.start..=reference.end).contains(&number))
        })
        .unwrap_or(0);
    app.sidebar_visible = false;
    app.focus = Focus::Diff;
    app.diff_navigation = DiffNavigation::Block;
    app.center_diff = true;
    app.return_to_briefing = true;
    app.report_highlight = Some((file_index, reference.start, reference.end));
    app.briefing_open = false;
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

fn replace_picker_results(
    pulls: &mut Vec<PullRequestChoice>,
    contributors: &mut Vec<String>,
    results: Vec<PullRequestChoice>,
    selected_number: Option<u64>,
) -> usize {
    *pulls = results;
    contributors.extend(pulls.iter().map(|pull| pull.author.login.clone()));
    contributors.sort_by_key(|login| login.to_lowercase());
    contributors.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
    selected_number
        .and_then(|number| pulls.iter().position(|pull| pull.number == number))
        .unwrap_or(0)
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
    let mut search_text = editor_render_text_with_prefix(editor, search_marker);
    if let Some(line) = search_text.lines.first_mut() {
        if let Some(marker) = line.spans.first_mut() {
            marker.style = Style::default().fg(ACCENT);
        }
        line.spans.push(Span::styled(
            if searching {
                format!("  [{}]", editor_status_label(editor))
            } else {
                String::new()
            },
            Style::default().fg(MUTED),
        ));
    }
    frame.render_widget(
        Paragraph::new(search_text).block(
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
        " j/k or ↑/↓ select  Enter open  / search  r refresh  q/Esc quit".to_string()
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
                                let cache_error =
                                    save_picker_query(repo, query).err().map(|error| {
                                        format!("Search updated, but could not cache it: {error}")
                                    });
                                selected =
                                    replace_picker_results(pulls, contributors, results, None);
                                state.select((!pulls.is_empty()).then_some(selected));
                                searching = false;
                                editor.mode = EditorMode::Insert;
                                error = cache_error;
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
                KeyCode::Char('r') => {
                    let selected_number = pulls.get(selected).map(|pull| pull.number);
                    match search_pull_requests(repo, query) {
                        Ok(results) => {
                            selected = replace_picker_results(
                                pulls,
                                contributors,
                                results,
                                selected_number,
                            );
                            state.select((!pulls.is_empty()).then_some(selected));
                            error = None;
                        }
                        Err(refresh_error) => error = Some(refresh_error.to_string()),
                    }
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
    remote_for_repo_at(Path::new("."), repo)
}

fn remote_for_repo_at(root: &Path, repo: &str) -> Result<String> {
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

fn local_pull_request_worktree_diff(
    workspace: &Path,
    repo: &str,
    pull: &PullRequest,
) -> Result<String> {
    let base_commit = format!("{}^{{commit}}", pull.base_ref_oid);
    if git_at(workspace, &["cat-file", "-e", &base_commit]).is_err() {
        let remote = remote_for_repo(repo)?;
        let base_ref = format!("refs/heads/{}", pull.base_ref_name);
        git_at(workspace, &["fetch", "--no-tags", &remote, &base_ref])
            .context("could not fetch the pull request base branch")?;
        git_at(workspace, &["cat-file", "-e", &base_commit]).context(
            "could not find the pull request base commit after fetching its base branch",
        )?;
    }
    local_working_tree_diff(workspace, &pull.base_ref_oid)
        .context("could not generate a local pull request working-tree diff")
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
    let file = app.files.get(app.file_index)?;
    comment_target(file, app.line_index, app.diff_navigation)
}

fn comment_target(
    file: &ChangedFile,
    line_index: usize,
    navigation: DiffNavigation,
) -> Option<(u32, Side)> {
    if matches!(navigation, DiffNavigation::Block)
        && let Some((start, end)) = change_block_at(file, line_index)
    {
        if let Some(number) = file.lines[start..=end]
            .iter()
            .find_map(|line| line.new_line.filter(|_| matches!(line.kind, LineKind::Add)))
        {
            return Some((number, Side::Right));
        }
        if let Some(number) = file.lines[start..=end].iter().find_map(|line| {
            line.old_line
                .filter(|_| matches!(line.kind, LineKind::Remove))
        }) {
            return Some((number, Side::Left));
        }
    }
    let line = file.lines.get(line_index)?;
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

fn is_entirely_added_or_removed(file: &ChangedFile) -> bool {
    let mut content = file.lines.iter().filter(|line| {
        matches!(
            line.kind,
            LineKind::Context | LineKind::Add | LineKind::Remove
        )
    });
    let Some(first) = content.next() else {
        return false;
    };
    matches!(first.kind, LineKind::Add | LineKind::Remove)
        && content.all(|line| line.kind == first.kind)
}

fn reset_diff_navigation(app: &mut App) {
    app.line_index = 0;
    app.diff_scroll = 0;
    app.auto_line_mode_pending = true;
    app.line_mode_locked = false;
    if is_entirely_added_or_removed(selected_file(app)) {
        app.diff_navigation = DiffNavigation::Line;
        app.center_diff = false;
        app.auto_line_mode_pending = false;
        app.line_mode_locked = true;
        app.line_index = selected_file(app)
            .lines
            .iter()
            .position(is_change_line)
            .unwrap_or(0);
    } else {
        app.diff_navigation = DiffNavigation::Block;
        move_change_block(app, 0);
    }
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
    app.center_diff = true;
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

fn single_change_block(file: &ChangedFile) -> Option<(usize, usize)> {
    let starts = file
        .lines
        .iter()
        .enumerate()
        .filter(|(index, line)| {
            is_change_line(line) && (*index == 0 || !is_change_line(&file.lines[index - 1]))
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    (starts.len() == 1)
        .then(|| change_block_at(file, starts[0]))
        .flatten()
}

fn scroll_oversized_block(app: &mut App, amount: isize) -> bool {
    let Some((start, end)) = change_block_at(selected_file(app), app.line_index) else {
        return false;
    };
    if end - start < app.diff_view_height.max(1) {
        return false;
    }
    let page = (app.diff_view_height / 2).max(1) as isize;
    let max_scroll = end.min(
        selected_file(app)
            .lines
            .len()
            .saturating_sub(app.diff_view_height),
    );
    app.diff_scroll = app
        .diff_scroll
        .saturating_add_signed(amount.saturating_mul(page))
        .clamp(start, max_scroll.max(start));
    true
}

fn change_file(app: &mut App, amount: isize) {
    if app.files.is_empty() {
        return;
    }
    app.file_index = app
        .file_index
        .saturating_add_signed(amount)
        .min(app.files.len() - 1);
    reset_diff_navigation(app);
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

fn wrap_words(value: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in value.split_whitespace() {
        let next_width =
            current.chars().count() + usize::from(!current.is_empty()) + word.chars().count();
        if !current.is_empty() && next_width > max_width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
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
        EditorMode::Visual => "VISUAL",
        EditorMode::Replace => "REPLACE",
    }
}

fn editor_status_label(editor: &TextEditor) -> String {
    let mode = editor_mode_label(editor);
    if editor.engine.command_line.is_empty() {
        mode.to_string()
    } else {
        format!("{mode}  {}", editor.engine.command_line)
    }
}

fn next_char_boundary(text: &str, index: usize) -> usize {
    text[index.min(text.len())..]
        .chars()
        .next()
        .map_or(text.len(), |character| index + character.len_utf8())
}

fn editor_render_text(editor: &TextEditor) -> Text<'static> {
    let VimMode::Visual(kind) = &editor.engine.mode else {
        return Text::from(editor.text.clone());
    };
    let Some(anchor) = editor.engine.visual_anchor else {
        return Text::from(editor.text.clone());
    };
    let cursor = (editor.engine.cursor_row, editor.engine.cursor_col);
    let (start, end) = if anchor <= cursor {
        (anchor, cursor)
    } else {
        (cursor, anchor)
    };
    let selection_style = Style::default()
        .fg(Color::White)
        .bg(Color::Rgb(55, 70, 90))
        .add_modifier(Modifier::BOLD);
    let lines = editor
        .engine
        .lines
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let range = match kind {
                VisualKind::Line if (start.0..=end.0).contains(&row) => Some((0, line.len())),
                VisualKind::Char if (start.0..=end.0).contains(&row) => {
                    let range_start = if row == start.0 { start.1 } else { 0 };
                    let range_end = if row == end.0 {
                        next_char_boundary(line, end.1)
                    } else {
                        line.len()
                    };
                    Some((range_start.min(line.len()), range_end.min(line.len())))
                }
                VisualKind::Block if (start.0..=end.0).contains(&row) => Some((
                    start.1.min(end.1).min(line.len()),
                    next_char_boundary(line, start.1.max(end.1)).min(line.len()),
                )),
                _ => None,
            };
            let Some((selection_start, selection_end)) = range else {
                return Line::raw(line.clone());
            };
            Line::from(vec![
                Span::raw(line[..selection_start].to_string()),
                Span::styled(
                    line[selection_start..selection_end].to_string(),
                    selection_style,
                ),
                Span::raw(line[selection_end..].to_string()),
            ])
        })
        .collect::<Vec<_>>();
    Text::from(lines)
}

fn editor_render_text_with_prefix(editor: &TextEditor, prefix: &str) -> Text<'static> {
    let mut text = editor_render_text(editor);
    if let Some(line) = text.lines.first_mut() {
        line.spans.insert(0, Span::raw(prefix.to_string()));
    }
    text
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

fn update_diff_scroll(app: &mut App, visible_height: usize) {
    app.diff_view_height = visible_height;
    if visible_height > 0
        && app.auto_line_mode_pending
        && matches!(app.diff_navigation, DiffNavigation::Block)
        && let Some((start, end)) = single_change_block(selected_file(app))
        && end - start + 1 >= visible_height
    {
        app.diff_navigation = DiffNavigation::Line;
        app.line_index = start;
        app.diff_scroll = 0;
        app.center_diff = false;
        app.line_mode_locked = true;
    }
    if visible_height > 0 {
        app.auto_line_mode_pending = false;
    }
    let max_start = selected_file(app)
        .lines
        .len()
        .saturating_sub(visible_height);
    if app.center_diff {
        let center_index = if matches!(app.diff_navigation, DiffNavigation::Block) {
            change_block_at(selected_file(app), app.line_index)
                .map_or(app.line_index, |(start, end)| start + (end - start) / 2)
        } else {
            app.line_index
        };
        app.diff_scroll = center_index
            .saturating_sub(visible_height / 2)
            .min(max_start);
        app.center_diff = false;
    } else if matches!(app.diff_navigation, DiffNavigation::Block) {
        if let Some((block_start, block_end)) = change_block_at(selected_file(app), app.line_index)
        {
            if block_end < app.diff_scroll {
                app.diff_scroll = block_start;
            } else if block_start >= app.diff_scroll.saturating_add(visible_height) {
                app.diff_scroll = block_start.saturating_sub(visible_height.saturating_sub(1));
            }
        }
    } else if app.line_index < app.diff_scroll {
        app.diff_scroll = app.line_index;
    } else if app.line_index >= app.diff_scroll.saturating_add(visible_height) {
        app.diff_scroll = app
            .line_index
            .saturating_sub(visible_height.saturating_sub(1));
    }
    app.diff_scroll = app.diff_scroll.min(max_start);
}

fn draw(app: &mut App, frame: &mut ratatui::Frame) {
    let area = frame.area();
    if app.briefing_open {
        draw_briefing(app, frame);
        draw_overlay(app, frame, area);
        return;
    }
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
            .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
            .split(layout[1]);
        let description_height = if app.description_expanded {
            columns[1].height.saturating_sub(4).max(6)
        } else {
            6.min(columns[1].height.saturating_sub(3))
        };
        let sidebar = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(description_height), Constraint::Min(3)])
            .split(columns[1]);
        let description = if app.pull.body.trim().is_empty() {
            "No description provided."
        } else {
            app.pull.body.as_str()
        };
        frame.render_widget(
            Paragraph::new(description)
                .block(
                    Block::default()
                        .title(if app.description_expanded {
                            " Description [expanded] "
                        } else {
                            " Description "
                        })
                        .borders(Borders::ALL)
                        .border_style(if matches!(app.focus, Focus::Description) {
                            Style::default().fg(ACCENT)
                        } else {
                            Style::default()
                        }),
                )
                .scroll((app.description_scroll, 0))
                .wrap(Wrap { trim: false }),
            sidebar[0],
        );
        let show_flow_footer = matches!(app.flow_state, FlowState::Failed(_));
        let file_panes = Layout::default()
            .direction(Direction::Vertical)
            .constraints(if show_flow_footer {
                [Constraint::Min(3), Constraint::Length(5)]
            } else {
                [Constraint::Percentage(100), Constraint::Length(0)]
            })
            .split(sidebar[1]);
        if app.flow_view
            && let FlowState::Ready(plan) = &app.flow_state
        {
            app.flow_index = app.flow_index.min(plan.steps.len().saturating_sub(1));
            let detail_width = file_panes[0].width.saturating_sub(8) as usize;
            let title_width = file_panes[0].width.saturating_sub(4) as usize;
            let title_lines = plan
                .steps
                .iter()
                .enumerate()
                .map(|(index, step)| flow_step_title_lines(index, &step.title, title_width))
                .collect::<Vec<_>>();
            let detail_lines = plan
                .steps
                .get(app.flow_index)
                .map(|step| flow_step_detail_lines(step, detail_width))
                .unwrap_or_default();
            let inner_height = file_panes[0].height.saturating_sub(2) as usize;
            let title_height = title_lines.iter().map(Vec::len).sum::<usize>();
            let detail_capacity = inner_height.saturating_sub(title_height);
            let max_detail_scroll = detail_lines.len().saturating_sub(detail_capacity);
            app.flow_detail_scroll = app.flow_detail_scroll.min(max_detail_scroll);
            let visible_details = detail_lines
                .into_iter()
                .skip(app.flow_detail_scroll)
                .take(detail_capacity)
                .collect::<Vec<_>>();
            let items = plan.steps.iter().enumerate().map(|(index, _step)| {
                let selected = index == app.flow_index;
                let title_style = if selected {
                    Style::default()
                        .fg(Color::LightMagenta)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                };
                let mut lines = title_lines[index]
                    .iter()
                    .cloned()
                    .map(|line| Line::styled(line, title_style))
                    .collect::<Vec<_>>();
                if selected {
                    lines.extend(visible_details.iter().cloned());
                }
                ListItem::new(Text::from(lines))
            });
            let mut state = ListState::default()
                .with_selected((!plan.steps.is_empty()).then_some(app.flow_index));
            frame.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .title(" ✦ Peer review · f files · [/] step · C-u/C-d detail ")
                            .borders(Borders::ALL)
                            .border_style(if matches!(app.focus, Focus::Files) {
                                Style::default().fg(Color::LightMagenta)
                            } else {
                                Style::default()
                            }),
                    )
                    .highlight_symbol("▌ "),
                file_panes[0],
                &mut state,
            );
        } else {
            let file_label_width = file_panes[0].width.saturating_sub(4) as usize;
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
                        .title(" Files · f peer review ")
                        .borders(Borders::ALL)
                        .border_style(if matches!(app.focus, Focus::Files) {
                            Style::default().fg(ACCENT)
                        } else {
                            Style::default()
                        }),
                )
                .highlight_style(
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▌ ");
            frame.render_stateful_widget(files, file_panes[0], &mut app.files_state);
        }
        match &app.flow_state {
            FlowState::Failed(error) => {
                frame.render_widget(
                    Paragraph::new(error.as_str())
                        .block(
                            Block::default()
                                .title(" Peer review · unavailable ")
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(REMOVED)),
                        )
                        .wrap(Wrap { trim: false }),
                    file_panes[1],
                );
            }
            FlowState::Ready(_) | FlowState::Absent => {}
        }
        columns[0]
    } else {
        layout[1]
    };
    let visible_height = diff_area.height.saturating_sub(2) as usize;
    update_diff_scroll(app, visible_height);
    let start = app.diff_scroll;
    let file = selected_file(app);
    let search_query = active_search_query(app).to_string();
    let normalized_search_query = search_query.to_lowercase();
    let diff_lines: Vec<Line> =
        file.lines
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
                let cited =
                    app.report_highlight
                        .is_some_and(|(file_index, range_start, range_end)| {
                            file_index == app.file_index
                                && line.new_line.or(line.old_line).is_some_and(|number| {
                                    (range_start..=range_end).contains(&number)
                                })
                        });
                let marker = if matches!(app.diff_navigation, DiffNavigation::Line) {
                    " "
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
                } else if cited {
                    "▐"
                } else {
                    " "
                };
                let matched = !normalized_search_query.is_empty()
                    && line_matches(line, &normalized_search_query);
                let mut style = line_style(line.kind);
                if matched && !active_block {
                    style = style.bg(Color::DarkGray);
                }
                if cited {
                    style = style.bg(REPORT_EVIDENCE_BACKGROUND);
                }
                if selected {
                    style = style.bg(LINE_SELECTION_BACKGROUND);
                }
                let line_prefix = format!("{old} {new} {sign} ");
                let marker_style = if selected {
                    Style::default()
                        .fg(Color::LightCyan)
                        .bg(LINE_SELECTION_BACKGROUND)
                        .add_modifier(Modifier::BOLD)
                } else if cited {
                    Style::default()
                        .fg(Color::LightMagenta)
                        .bg(REPORT_EVIDENCE_BACKGROUND)
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
                    span.style = if selected {
                        span.style.bg(LINE_SELECTION_BACKGROUND)
                    } else if cited {
                        span.style.bg(REPORT_EVIDENCE_BACKGROUND)
                    } else if matched && !active_block {
                        span.style.bg(Color::DarkGray)
                    } else {
                        match line.kind {
                            LineKind::Add => span.style.bg(ADDED_BACKGROUND),
                            LineKind::Remove => span.style.bg(REMOVED_BACKGROUND),
                            _ => span.style,
                        }
                    };
                }
                if !selected {
                    syntax_spans = mark_search_matches(syntax_spans, &search_query);
                }
                spans.extend(syntax_spans);
                if selected {
                    let row_width = diff_area.width.saturating_sub(2) as usize;
                    let content_width = spans.iter().map(Span::width).sum::<usize>();
                    if content_width < row_width {
                        spans.push(Span::styled(
                            " ".repeat(row_width - content_width),
                            Style::default().bg(LINE_SELECTION_BACKGROUND),
                        ));
                    }
                }
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
        Mode::Browse if matches!(app.focus, Focus::Description) => {
            "DESCRIPTION  j files  Ctrl-d/u scroll  Enter collapse  Tab next pane  Esc files"
        }
        Mode::Browse
            if matches!(app.focus, Focus::Diff)
                && matches!(app.diff_navigation, DiffNavigation::Line) =>
        {
            "LINE  j/k line  zz center  Enter comment  Esc back  b intelligence  / search  : command  P submit"
        }
        Mode::Browse if app.sidebar_visible => {
            "BLOCK  j/k change  V choose line  zz center  Enter comment  b intelligence  / search  : command  P submit"
        }
        Mode::Browse => {
            "BLOCK  j/k change  V choose line  zz center  Enter comment  b intelligence  / search  : command  P submit"
        }
        Mode::Search { .. } => "Enter confirm  Esc normal  Esc again cancel",
        Mode::Command => "Enter run  Esc normal  Esc again cancel",
        Mode::Compose => "Enter save  Ctrl+Enter newline  Esc normal  Esc again cancel",
        Mode::Submit if app.local_review.is_some() => {
            "s submit to Codex  x copy handoff  Esc cancel"
        }
        Mode::Submit => "a approve  r request changes  c comment  x copy handoff  Esc cancel",
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

fn markdown_inline(mut source: &str, references: &[String], selected: usize) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    while !source.is_empty() {
        if let Some(rest) = source.strip_prefix('`')
            && let Some(end) = rest.find('`')
        {
            let value = &rest[..end];
            let reference = references.iter().position(|path| value.contains(path));
            let mut style = Style::default()
                .fg(Color::LightCyan)
                .bg(Color::Rgb(35, 35, 42));
            if let Some(index) = reference {
                style = style.add_modifier(Modifier::UNDERLINED);
                if index == selected {
                    style = style
                        .fg(Color::Black)
                        .bg(ACCENT)
                        .add_modifier(Modifier::BOLD);
                }
            }
            spans.push(Span::styled(value.to_string(), style));
            source = &rest[end + 1..];
            continue;
        }
        if let Some(rest) = source.strip_prefix("**")
            && let Some(end) = rest.find("**")
        {
            spans.push(Span::styled(
                rest[..end].to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            source = &rest[end + 2..];
            continue;
        }
        let next = [source.find('`'), source.find("**")]
            .into_iter()
            .flatten()
            .filter(|index| *index > 0)
            .min()
            .unwrap_or(source.len());
        spans.push(Span::raw(source[..next].to_string()));
        source = &source[next..];
    }
    spans
}

fn markdown_text(source: &str, references: &[String], selected: usize) -> Text<'static> {
    let mut code = false;
    let lines = source
        .lines()
        .filter_map(|raw| {
            if raw.trim_start().starts_with("```") {
                code = !code;
                return None;
            }
            let trimmed = raw.trim_start();
            if code {
                return Some(Line::styled(
                    raw.to_string(),
                    Style::default().fg(Color::LightGreen),
                ));
            }
            if let Some(callout) = trimmed.strip_prefix("> ") {
                let mut spans = vec![Span::styled(
                    "┃ ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )];
                spans.extend(markdown_inline(callout, references, selected));
                return Some(Line::from(spans).style(Style::default().bg(Color::Rgb(42, 40, 28))));
            }
            if let Some(heading) = trimmed
                .strip_prefix("### ")
                .or_else(|| trimmed.strip_prefix("## "))
                .or_else(|| trimmed.strip_prefix("# "))
            {
                return Some(Line::styled(
                    heading.to_string(),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ));
            }
            if let Some(item) = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
            {
                let mut spans = vec![Span::styled("• ", Style::default().fg(ACCENT))];
                spans.extend(markdown_inline(item, references, selected));
                return Some(Line::from(spans));
            }
            if let Some((number, item)) = trimmed.split_once(". ")
                && number.chars().all(|character| character.is_ascii_digit())
            {
                let mut spans = vec![Span::styled(
                    format!("{number}. "),
                    Style::default().fg(ACCENT),
                )];
                spans.extend(markdown_inline(item, references, selected));
                return Some(Line::from(spans));
            }
            Some(Line::from(markdown_inline(raw, references, selected)))
        })
        .collect::<Vec<_>>();
    Text::from(lines)
}

fn draw_briefing(app: &mut App, frame: &mut ratatui::Frame) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " REVIEW INTELLIGENCE ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("#{}  {}", app.pr_number, app.pull.title)),
            Span::styled("  deterministic · local", Style::default().fg(MUTED)),
        ]))
        .block(Block::default().borders(Borders::ALL)),
        layout[0],
    );
    if !app.report_sections.is_empty() {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(28), Constraint::Percentage(72)])
            .split(layout[1]);
        let items = app
            .report_sections
            .iter()
            .map(|section| ListItem::new(format!("●  {}", section.title)));
        let mut state = ListState::default().with_selected(Some(app.report_section));
        frame.render_stateful_widget(
            List::new(items)
                .block(
                    Block::default()
                        .title(" Briefing sections ")
                        .borders(Borders::ALL)
                        .border_style(if app.report_content_focus {
                            Style::default()
                        } else {
                            Style::default().fg(ACCENT)
                        }),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("› "),
            columns[0],
            &mut state,
        );
        let references = report_references(app);
        let reference_labels = references
            .iter()
            .map(|reference| reference.raw.clone())
            .collect::<Vec<_>>();
        app.report_reference = app.report_reference.min(references.len().saturating_sub(1));
        let section = &app.report_sections[app.report_section];
        let SectionState::Ready(source) = &section.state;
        frame.render_widget(
            Paragraph::new(markdown_text(
                source,
                &reference_labels,
                app.report_reference,
            ))
            .scroll((app.briefing_scroll, 0))
            .block(
                Block::default()
                    .title(format!(" {} · local evidence ", section.title))
                    .borders(Borders::ALL)
                    .border_style(if app.report_content_focus {
                        Style::default().fg(ACCENT)
                    } else {
                        Style::default()
                    }),
            )
            .wrap(Wrap { trim: false }),
            columns[1],
        );
        frame.render_widget(
            Paragraph::new(if app.report_content_focus {
                " CONTENT  j/k scroll  n/N evidence  Enter jump  Esc sections  : commands"
            } else {
                " SECTIONS  j/k select  Enter inspect  Esc review  : commands"
            })
            .style(Style::default().fg(MUTED)),
            layout[2],
        );
        return;
    }
    match &app.briefing_state {
        BriefingState::Idle => {
            let targets = review_targets(app);
            app.briefing_target = app.briefing_target.min(targets.len().saturating_sub(1));
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
                .split(layout[1]);
            let items = targets.iter().map(|target| {
                ListItem::new(format!(
                    "{}\n  {}",
                    target.label, app.files[target.file_index].path
                ))
            });
            let mut state = ListState::default()
                .with_selected((!targets.is_empty()).then_some(app.briefing_target));
            frame.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .title(format!(" Review targets ({}) ", targets.len()))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    )
                    .highlight_style(
                        Style::default()
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("› "),
                columns[0],
                &mut state,
            );
            let (title, preview) = if let Some(target) = targets.get(app.briefing_target) {
                let file = &app.files[target.file_index];
                let (block_start, block_end) = change_block_at(file, target.line_index)
                    .unwrap_or((target.line_index, target.line_index));
                let start = block_start.saturating_sub(2);
                let end = (block_end + 2).min(file.lines.len().saturating_sub(1));
                let lines = file.lines[start..=end]
                    .iter()
                    .enumerate()
                    .map(|(offset, line)| {
                        let index = start + offset;
                        let sign = match line.kind {
                            LineKind::Add => "+",
                            LineKind::Remove => "-",
                            _ => " ",
                        };
                        let mut spans =
                            vec![Span::styled(format!("{sign} "), line_style(line.kind))];
                        let mut code: Vec<Span> = file.syntax_lines[index]
                            .iter()
                            .map(|span| {
                                Span::styled(span.text.clone(), Style::default().fg(span.color))
                            })
                            .collect();
                        if code.is_empty() {
                            code.push(Span::raw(line.text.clone()));
                        }
                        for span in &mut code {
                            span.style = match line.kind {
                                LineKind::Add => span.style.bg(ADDED_BACKGROUND),
                                LineKind::Remove => span.style.bg(REMOVED_BACKGROUND),
                                _ => span.style,
                            };
                        }
                        spans.extend(code);
                        Line::from(spans)
                    })
                    .collect::<Vec<_>>();
                (
                    format!(" {} · Enter opens diff ", file.path),
                    Text::from(lines),
                )
            } else {
                (
                    " No review targets ".to_string(),
                    Text::from("No changed blocks were found."),
                )
            };
            frame.render_widget(
                Paragraph::new(preview)
                    .block(Block::default().title(title).borders(Borders::ALL))
                    .wrap(Wrap { trim: false }),
                columns[1],
            );
        }
        BriefingState::Loading {
            symbols,
            transcript,
            ..
        } => {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(0), Constraint::Percentage(100)])
                .split(layout[1]);
            let deterministic = if symbols.is_empty() {
                "No top-level function or type signature changes detected.".to_string()
            } else {
                symbols
                    .iter()
                    .map(|symbol| {
                        format!(
                            "{:<8}  {}\n          {}",
                            symbol.kind, symbol.name, symbol.path
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n")
            };
            frame.render_widget(
                Paragraph::new(deterministic)
                    .block(
                        Block::default()
                            .title(" Available now · deterministic ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ADDED)),
                    )
                    .wrap(Wrap { trim: false }),
                columns[0],
            );
            let stream = if transcript.is_empty() {
                "YOU\nBuild me a peer-review briefing for this PR.\n\nPI\nConnecting…"
            } else {
                transcript.as_str()
            };
            let lines = stream.lines().count() as u16;
            let height = columns[1].height.saturating_sub(2);
            frame.render_widget(
                Paragraph::new(stream)
                    .scroll((lines.saturating_sub(height), 0))
                    .block(
                        Block::default()
                            .title(" PI · generating report ")
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    )
                    .wrap(Wrap { trim: false }),
                columns[1],
            );
        }
        BriefingState::Chat(transcript) => frame.render_widget(
            Paragraph::new(transcript.as_str())
                .scroll((app.briefing_scroll, 0))
                .block(
                    Block::default()
                        .title(" PI · review briefing ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(ACCENT)),
                )
                .wrap(Wrap { trim: false }),
            layout[1],
        ),
        BriefingState::Failed { error, transcript } => frame.render_widget(
            Paragraph::new(format!("{transcript}\n{error}\n\nPress r to retry."))
                .block(
                    Block::default()
                        .title(" PI · report failed ")
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(REMOVED)),
                )
                .wrap(Wrap { trim: false }),
            layout[1],
        ),
    }
    frame.render_widget(
        Paragraph::new(" j/k scroll  r rebuild report  b/Esc review")
            .style(Style::default().fg(MUTED)),
        layout[2],
    );
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
        Mode::Command => Rect {
            x: area.x + area.width / 10,
            y: area.y + area.height.saturating_sub(4),
            width: area.width * 8 / 10,
            height: 3,
        },
        _ => standard_overlay,
    };
    match &app.mode {
        Mode::Browse => {}
        Mode::Search { .. } => {
            frame.render_widget(Clear, overlay);
            frame.render_widget(
                Paragraph::new(editor_render_text(&app.editor))
                    .block(
                        Block::default()
                            .title(format!(" Search [{}] ", editor_status_label(&app.editor)))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    )
                    .wrap(Wrap { trim: false }),
                overlay,
            );
            place_editor_cursor(frame, overlay, &app.editor);
        }
        Mode::Command => {
            frame.render_widget(Clear, overlay);
            frame.render_widget(
                Paragraph::new(editor_render_text_with_prefix(&app.editor, ":"))
                    .block(
                        Block::default()
                            .title(format!(" Command [{}] ", editor_status_label(&app.editor)))
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(ACCENT)),
                    )
                    .wrap(Wrap { trim: false }),
                overlay,
            );
            let cursor_offset = app.editor.text[..app.editor.cursor].chars().count() as u16;
            frame.set_cursor_position((overlay.x + 2 + cursor_offset, overlay.y + 1));
        }
        Mode::Compose => {
            frame.render_widget(Clear, overlay);
            let target = selected_comment_target(app)
                .map(|(line, side)| format!("{}:{line} ({side:?})", selected_file(app).path))
                .unwrap_or_else(|| "not commentable".to_string());
            frame.render_widget(
                Paragraph::new(editor_render_text(&app.editor))
                    .block(
                        Block::default()
                            .title(format!(
                                " Comment: {target} [{}]  Enter submit · Shift+Enter newline ",
                                editor_status_label(&app.editor)
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
            let choices = if app.local_review.is_some() {
                "Submit local review\n\n[s] Submit feedback to Codex\n[x] Copy comments\n\nEsc cancels"
            } else {
                "Submit review\n\n[a] Approve\n[r] Request changes\n[c] Comment\n[x] Copy comments\n\nEsc cancels"
            };
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
                Paragraph::new(editor_render_text(&app.editor))
                    .block(
                        Block::default()
                            .title(format!(
                                " Review summary (optional) [{}] ",
                                editor_status_label(&app.editor)
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

fn checkout_cache_root() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    cache.join("reviewer")
}

fn repo_has_matching_remote(root: &Path, repo: &str) -> bool {
    let Ok(remotes) = git_at(root, &["remote"]) else {
        return false;
    };
    remotes.lines().any(|remote| {
        git_at(root, &["remote", "get-url", remote])
            .ok()
            .is_some_and(|url| remote_matches_repo(&url, repo))
    })
}

fn local_repo_root(repo: &str) -> Option<PathBuf> {
    let root = git_at(Path::new("."), &["rev-parse", "--show-toplevel"]).ok()?;
    let root = PathBuf::from(root.trim());
    repo_has_matching_remote(&root, repo).then_some(root)
}

fn ensure_repository(repo: &str) -> Result<PathBuf> {
    if let Some(root) = local_repo_root(repo) {
        return Ok(root);
    }
    let target = checkout_cache_root()
        .join("repositories")
        .join(repo.replace(['/', '\\'], "-"));
    if target.is_dir() && repo_has_matching_remote(&target, repo) {
        return Ok(target);
    }
    if target.exists() {
        bail!(
            "managed repository path exists but is not a checkout of {repo}: {}",
            target.display()
        );
    }
    let parent = target
        .parent()
        .context("managed repository path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let output = Command::new("gh")
        .args(["repo", "clone", repo])
        .arg(&target)
        .args(["--", "--filter=blob:none"])
        .output()
        .context("could not start gh repo clone")?;
    if !output.status.success() {
        bail!(
            "could not clone {repo} for editor integration: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(target)
}

fn ensure_pr_checkout(app: &App) -> Result<PathBuf> {
    let repository = ensure_repository(&app.repo)?;
    for worktree in git_worktrees(&repository) {
        if git_at(&worktree, &["rev-parse", "HEAD"])
            .ok()
            .is_some_and(|head| head.trim() == app.pull.head_ref_oid)
            && worktree.join(&selected_file(app).path).is_file()
        {
            return Ok(worktree);
        }
    }
    if git_at(
        &repository,
        &[
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", app.pull.head_ref_oid),
        ],
    )
    .is_err()
    {
        let remote = remote_for_repo_at(&repository, &app.repo)?;
        let source = format!("refs/pull/{}/head", app.pr_number);
        git_at(&repository, &["fetch", "--no-tags", &remote, &source])
            .context("could not fetch the pull request head for the editor")?;
    }
    let short_head: String = app.pull.head_ref_oid.chars().take(12).collect();
    let target = checkout_cache_root().join("worktrees").join(format!(
        "{}-pr-{}-{short_head}",
        app.repo.replace(['/', '\\'], "-"),
        app.pr_number
    ));
    if target.exists() {
        if git_at(&target, &["rev-parse", "HEAD"])
            .ok()
            .is_some_and(|head| head.trim() == app.pull.head_ref_oid)
        {
            return Ok(target);
        }
        bail!("managed worktree path is occupied: {}", target.display());
    }
    let parent = target
        .parent()
        .context("managed worktree path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let target_text = target.to_string_lossy().to_string();
    git_at(
        &repository,
        &[
            "worktree",
            "add",
            "--detach",
            &target_text,
            &app.pull.head_ref_oid,
        ],
    )
    .context("could not create a worktree for the pull request")?;
    Ok(target)
}

fn open_in_editor(app: &App, _workspace: &Path) -> Result<()> {
    let relative_path = &selected_file(app).path;
    if Path::new(relative_path).is_absolute()
        || Path::new(relative_path)
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("refusing to open an unsafe diff path: {relative_path}");
    }
    let root = match &app.local_review {
        Some(local) => local.workspace.clone(),
        None => ensure_pr_checkout(app)?,
    };
    let destination = root.join(relative_path);
    if !destination.is_file() {
        bail!("{} does not exist in the PR head checkout", relative_path);
    }
    let line = selected_comment_target(app)
        .map(|(number, _)| number)
        .unwrap_or(1);
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nvim".to_string());
    let status = Command::new(editor)
        .current_dir(&root)
        .arg(format!("+{line}"))
        .arg(relative_path)
        .status();
    execute!(io::stdout(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    let status = status.context("could not open editor")?;
    if !status.success() {
        bail!("editor exited with status {status}");
    }
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

fn cache_key(value: &str) -> String {
    let mut hash = 14_695_981_039_346_656_037u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    format!("{hash:016x}")
}

fn codex_session_path(workspace: &Path) -> PathBuf {
    checkout_cache_root()
        .join("codex-sessions")
        .join(format!("{}.json", cache_key(&workspace.to_string_lossy())))
}

fn record_codex_session() -> Result<()> {
    let input: serde_json::Value =
        serde_json::from_reader(io::stdin()).context("could not read Codex hook input")?;
    let session_id = input
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .context("Codex hook input has no session_id")?;
    let workspace = input
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .context("Codex hook input has no cwd")?;
    let workspace = PathBuf::from(workspace);
    let path = codex_session_path(&workspace);
    let parent = path.parent().context("Codex session path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    fs::write(
        &path,
        serde_json::to_vec_pretty(&ActiveCodexSession {
            session_id: session_id.to_string(),
            workspace,
            updated_at: unix_time(),
        })?,
    )
    .with_context(|| format!("could not save {}", path.display()))?;
    Ok(())
}

fn active_codex_session(workspace: &Path) -> Result<Option<String>> {
    let path = codex_session_path(workspace);
    match fs::read(&path) {
        Ok(contents) => {
            let session: ActiveCodexSession = serde_json::from_slice(&contents)
                .with_context(|| format!("could not parse {}", path.display()))?;
            Ok((session.workspace == workspace).then_some(session.session_id))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

fn local_review_path(review_id: &str) -> PathBuf {
    local_review_directory().join(format!("{}.json", safe_cache_component(review_id)))
}

fn local_review_directory() -> PathBuf {
    checkout_cache_root().join("local-reviews")
}

fn safe_cache_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn save_local_review(app: &App, summary: &str) -> Result<PathBuf> {
    let local = app
        .local_review
        .as_ref()
        .context("this is not a local review")?;
    let path = local_review_path(&local.review_id);
    let parent = path.parent().context("local review path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let submission = LocalReviewSubmission {
        version: 1,
        review_id: local.review_id.clone(),
        workspace: local.workspace.clone(),
        session_id: local.session_id.clone(),
        base_revision: local.base_revision.clone(),
        submitted_at: unix_time(),
        summary: summary.to_string(),
        comments: app.comments.clone(),
        diff: app.briefing_diff.clone(),
    };
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(&submission)?)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("could not replace {}", path.display()))?;
    Ok(path)
}

fn local_review_prompt(submission: &LocalReviewSubmission) -> String {
    let comments = if submission.comments.is_empty() {
        "No inline comments were left. Inspect the reviewed diff and identify any remaining issues before deciding whether further changes are needed.".to_string()
    } else {
        submission
            .comments
            .iter()
            .enumerate()
            .map(|(index, comment)| {
                format!(
                    "{}. `{}` line {} ({})\n{}",
                    index + 1,
                    comment.path,
                    comment.line,
                    match comment.side {
                        Side::Left => "old side",
                        Side::Right => "new side",
                    },
                    comment.body
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let summary = if submission.summary.trim().is_empty() {
        "No overall review summary was supplied."
    } else {
        submission.summary.trim()
    };
    let comment_count = submission.comments.len();
    format!(
        "LOCAL REVIEW SUBMITTED\n\nReview ID: `{}`\nInline feedback items: {}\n\nThis is the complete feedback returned by Reviewer. Address every inline item below before concluding the review. Preserve accepted work, inspect the current working tree before editing, keep scope minimal, and run relevant validation. Do not create or interact with a pull request.\n\nReview base: `{}`\n\nOverall feedback:\n{}\n\nInline feedback:\n{}",
        submission.review_id, comment_count, submission.base_revision, summary, comments
    )
}

fn load_local_review(review_id: &str) -> Result<LocalReviewSubmission> {
    let path = local_review_path(review_id);
    serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("could not read {}", path.display()))?,
    )
    .with_context(|| format!("could not parse {}", path.display()))
}

fn print_codex_prompt(review_id: &str) -> Result<()> {
    let submission = load_local_review(review_id)?;
    println!("{}", local_review_prompt(&submission));
    Ok(())
}

fn print_latest_codex_prompt() -> Result<()> {
    let workspace = PathBuf::from(
        git_at(Path::new("."), &["rev-parse", "--show-toplevel"])?
            .trim()
            .to_string(),
    );
    let mut latest: Option<LocalReviewSubmission> = None;
    for entry in fs::read_dir(local_review_directory()).context("could not list local reviews")? {
        let entry = entry.context("could not read a local review entry")?;
        if entry.path().extension().and_then(OsStr::to_str) != Some("json") {
            continue;
        }
        let submission: LocalReviewSubmission = serde_json::from_slice(
            &fs::read(entry.path()).context("could not read a local review")?,
        )
        .context("could not parse a local review")?;
        if submission.workspace == workspace
            && latest
                .as_ref()
                .is_none_or(|candidate| submission.submitted_at > candidate.submitted_at)
        {
            latest = Some(submission);
        }
    }
    let submission =
        latest.context("no submitted local review was found for the current repository")?;
    println!("{}", local_review_prompt(&submission));
    Ok(())
}

fn print_local_review_prompt_when_submitted(review_id: &str) -> Result<()> {
    let path = local_review_path(review_id);
    loop {
        match fs::read(&path) {
            Ok(contents) => {
                let submission: LocalReviewSubmission = serde_json::from_slice(&contents)
                    .with_context(|| format!("could not parse {}", path.display()))?;
                println!("{}", local_review_prompt(&submission));
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        }
    }
}

fn inject_codex_prompt(pane: &str, prompt: &str) -> Result<()> {
    let buffer = format!("reviewer-codex-{}", std::process::id());
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-b", &buffer, "-"])
        .stdin(Stdio::piped())
        .spawn()
        .context("could not create a tmux buffer for the Codex handoff")?;
    child
        .stdin
        .as_mut()
        .context("could not open the tmux buffer input")?
        .write_all(prompt.as_bytes())?;
    let status = child
        .wait()
        .context("could not finish writing the Codex handoff")?;
    if !status.success() {
        bail!("tmux load-buffer exited with status {status}");
    }

    let status = Command::new("tmux")
        .args(["paste-buffer", "-p", "-d", "-b", &buffer, "-t", pane])
        .status()
        .context("could not paste the review into the Codex pane")?;
    if !status.success() {
        bail!("tmux paste-buffer exited with status {status}");
    }

    // Codex handles bracketed paste asynchronously. Give it time to finish
    // committing the pasted block before sending the key that submits it.
    std::thread::sleep(Duration::from_millis(250));
    let status = Command::new("tmux")
        .args(["send-keys", "-t", pane, "Enter"])
        .status()
        .context("could not submit the review to Codex")?;
    if !status.success() {
        bail!("tmux send-keys exited with status {status}");
    }
    Ok(())
}

fn active_tmux_pane() -> Result<String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{pane_id}"])
        .output()
        .context("could not determine the active tmux pane")?;
    if !output.status.success() {
        bail!(
            "tmux display-message exited with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let pane = String::from_utf8(output.stdout)
        .context("tmux returned a non-UTF-8 pane ID")?
        .trim()
        .to_string();
    if pane.is_empty() {
        bail!("tmux returned an empty pane ID");
    }
    Ok(pane)
}

fn codex_tmux_args(args: &[String]) -> Result<(Option<String>, bool, Vec<String>)> {
    let mut target_pane = None;
    let mut unstaged_or_pr = false;
    let mut review_args = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--unstaged-or-pr" {
            if unstaged_or_pr {
                bail!("--unstaged-or-pr may only be supplied once");
            }
            unstaged_or_pr = true;
            index += 1;
            continue;
        }
        if args[index] != "--target-pane" {
            review_args.push(args[index].clone());
            index += 1;
            continue;
        }
        if target_pane.is_some() {
            bail!("--target-pane may only be supplied once");
        }
        index += 1;
        target_pane = Some(
            args.get(index)
                .filter(|value| !value.is_empty())
                .context("--target-pane requires a value")?
                .clone(),
        );
        index += 1;
    }
    Ok((target_pane, unstaged_or_pr, review_args))
}

fn run_codex_tmux_review(args: &[String]) -> Result<()> {
    if std::env::var_os("TMUX").is_none() {
        bail!("reviewer codex-tmux must run inside tmux");
    }
    let (target_pane, unstaged_or_pr, mut review_args) = codex_tmux_args(args)?;
    let pane = match target_pane {
        Some(pane) => pane,
        None => active_tmux_pane()?,
    };
    if unstaged_or_pr {
        if !review_args.is_empty() {
            bail!("--unstaged-or-pr cannot be combined with another review scope");
        }
        let diff = git_at(
            Path::new("."),
            &["diff", "--no-color", "--no-ext-diff", "--find-renames"],
        )?;
        if !parse_diff(&diff).is_empty() {
            review_args.push("--unstaged".to_string());
        }
    }
    let (pr_number, review_id, wait, local_scope) = pull_request_review_args(&review_args)?;
    if wait {
        bail!("--wait is not supported with reviewer codex-tmux");
    }
    if review_id.is_some() {
        bail!("--review-id is not supported with reviewer codex-tmux");
    }
    let review_id = new_local_review_id(&None);
    if let Some(scope) = local_scope {
        run_local_review("HEAD".to_string(), scope, None, Some(review_id.clone()))?;
    } else {
        let pr_number = match pr_number {
            Some(pr_number) => pr_number,
            None => current_pull_request_number()?,
        };
        run_local_pull_request_review(pr_number, review_id.clone())?;
    }

    if !local_review_path(&review_id).exists() {
        return Ok(());
    }
    let submission = load_local_review(&review_id)?;
    inject_codex_prompt(&pane, &local_review_prompt(&submission))
}

fn review_handoff(app: &App) -> String {
    let comments = if app.comments.is_empty() {
        "No pending inline comments.".to_string()
    } else {
        app.comments
            .iter()
            .enumerate()
            .map(|(index, comment)| {
                format!(
                    "{}. `{}` at `{}` on `{}`\n\n{}",
                    index + 1,
                    comment.path,
                    comment.line,
                    match comment.side {
                        Side::Left => "the old side",
                        Side::Right => "the new side",
                    },
                    comment.body
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    format!(
        "# Pull request review handoff\n\nRepository: `{}`\nPull request: #{}\nTitle: {}\n\n## Pending comments\n\n{}\n",
        app.repo, app.pr_number, app.pull.title, comments
    )
}

fn copy_to_clipboard(contents: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .context("could not start pbcopy")?;

    #[cfg(target_os = "windows")]
    let mut child = Command::new("clip")
        .stdin(Stdio::piped())
        .spawn()
        .context("could not start clip")?;

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = contents;
        bail!("clipboard export is supported on macOS and Windows")
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        child
            .stdin
            .as_mut()
            .context("could not write clipboard contents")?
            .write_all(contents.as_bytes())?;
        let status = child.wait()?;
        if status.success() {
            Ok(())
        } else {
            bail!("clipboard command exited with status {status}")
        }
    }
}

fn request_quit(app: &mut App) {
    if app.comments.is_empty() {
        app.should_quit = true;
    } else {
        app.mode = Mode::Message(format!(
            "{} review comment{} have not been submitted. Submit or remove them before quitting.",
            app.comments.len(),
            if app.comments.len() == 1 { "" } else { "s" }
        ));
    }
}

fn request_return_to_picker(app: &mut App) {
    if app.comments.is_empty() {
        app.return_to_picker = true;
    } else {
        app.mode = Mode::Message(format!(
            "{} review comment{} have not been submitted. Submit or remove them before returning to the PR list.",
            app.comments.len(),
            if app.comments.len() == 1 { "" } else { "s" }
        ));
    }
}

fn handle_browse(app: &mut App, key: KeyEvent, workspace: &Path) {
    if app.briefing_open {
        if !app.report_sections.is_empty() {
            match key.code {
                KeyCode::Char(':') => {
                    app.editor.reset();
                    app.mode = Mode::Command;
                }
                KeyCode::Esc if app.report_content_focus => {
                    app.report_content_focus = false;
                    app.briefing_scroll = 0;
                }
                KeyCode::Esc | KeyCode::Char('b') => app.briefing_open = false,
                KeyCode::Enter if !app.report_content_focus => {
                    app.report_content_focus = true;
                    app.briefing_scroll = 0;
                    app.report_reference = 0;
                }
                KeyCode::Enter if app.report_content_focus => jump_to_report_reference(app),
                KeyCode::Char('n') if app.report_content_focus => {
                    app.report_reference = (app.report_reference + 1)
                        .min(report_references(app).len().saturating_sub(1));
                }
                KeyCode::Char('N') if app.report_content_focus => {
                    app.report_reference = app.report_reference.saturating_sub(1);
                }
                KeyCode::Char('j') | KeyCode::Down if !app.report_content_focus => {
                    app.report_section =
                        (app.report_section + 1).min(app.report_sections.len() - 1);
                    app.briefing_scroll = 0;
                    app.report_reference = 0;
                }
                KeyCode::Char('k') | KeyCode::Up if !app.report_content_focus => {
                    app.report_section = app.report_section.saturating_sub(1);
                    app.briefing_scroll = 0;
                    app.report_reference = 0;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    app.briefing_scroll = app.briefing_scroll.saturating_add(1)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    app.briefing_scroll = app.briefing_scroll.saturating_sub(1)
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.briefing_scroll = app.briefing_scroll.saturating_add(8)
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.briefing_scroll = app.briefing_scroll.saturating_sub(8)
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('b') => app.briefing_open = false,
            KeyCode::Char('p') if !matches!(app.briefing_state, BriefingState::Loading { .. }) => {
                start_briefing(app)
            }
            KeyCode::Enter if matches!(app.briefing_state, BriefingState::Idle) => {
                open_briefing_target(app)
            }
            KeyCode::Char('r') if !matches!(app.briefing_state, BriefingState::Loading { .. }) => {
                start_briefing(app)
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if matches!(app.briefing_state, BriefingState::Idle) {
                    app.briefing_target =
                        (app.briefing_target + 1).min(review_targets(app).len().saturating_sub(1));
                } else {
                    app.briefing_scroll = app.briefing_scroll.saturating_add(1)
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if matches!(app.briefing_state, BriefingState::Idle) {
                    app.briefing_target = app.briefing_target.saturating_sub(1);
                } else {
                    app.briefing_scroll = app.briefing_scroll.saturating_sub(1)
                }
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.briefing_scroll = app.briefing_scroll.saturating_add(8)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.briefing_scroll = app.briefing_scroll.saturating_sub(8)
            }
            KeyCode::Char('g') => app.briefing_scroll = 0,
            KeyCode::Char('G') => app.briefing_scroll = u16::MAX,
            _ => {}
        }
        return;
    }
    if app.pending_z {
        app.pending_z = false;
        if matches!(app.focus, Focus::Diff) && matches!(key.code, KeyCode::Char('z')) {
            app.center_diff = true;
            return;
        }
    }
    if app.pending_space {
        app.pending_space = false;
        if matches!(key.code, KeyCode::Char('l')) {
            app.sidebar_visible = !app.sidebar_visible;
            if app.sidebar_visible {
                app.focus = Focus::Files;
                app.files_state.select(Some(app.file_index));
            } else {
                app.focus = Focus::Diff;
            }
            return;
        }
    }
    if matches!(key.code, KeyCode::Char(' ')) {
        app.pending_space = true;
        return;
    }
    if matches!(app.focus, Focus::Diff) && matches!(key.code, KeyCode::Char('z')) {
        app.pending_z = true;
        return;
    }
    match key.code {
        KeyCode::Char('q') => request_quit(app),
        KeyCode::Char('b') => {
            app.briefing_open = true;
        }
        KeyCode::Esc => {
            if matches!(app.focus, Focus::Description) && app.description_expanded {
                app.description_expanded = false;
                app.description_scroll = 0;
            } else if matches!(app.focus, Focus::Description) {
                app.focus = Focus::Files;
            } else if matches!(app.focus, Focus::Diff)
                && matches!(app.diff_navigation, DiffNavigation::Line)
                && !app.line_mode_locked
            {
                app.diff_navigation = DiffNavigation::Block;
            } else if matches!(app.focus, Focus::Diff) && app.return_to_briefing {
                app.return_to_briefing = false;
                app.briefing_open = true;
            } else if !app.sidebar_visible {
                app.sidebar_visible = true;
                app.focus = Focus::Files;
            } else if matches!(app.focus, Focus::Diff) {
                app.focus = Focus::Files;
            } else if matches!(app.focus, Focus::Files) {
                request_return_to_picker(app);
            }
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if matches!(app.focus, Focus::Description) {
                app.focus = Focus::Files;
                app.description_expanded = false;
                app.description_scroll = 0;
            } else if matches!(app.focus, Focus::Files) {
                if app.flow_view {
                    activate_flow_step(app, 1);
                } else {
                    change_file(app, 1)
                }
            } else if matches!(app.diff_navigation, DiffNavigation::Line) {
                move_line_in_block(app, 1)
            } else {
                move_change_block(app, 1)
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if matches!(app.focus, Focus::Description) {
                app.description_scroll = app.description_scroll.saturating_sub(1);
            } else if matches!(app.focus, Focus::Files) {
                if app.flow_view {
                    if app.flow_index == 0 {
                        app.focus = Focus::Description;
                        app.description_expanded = true;
                        app.description_scroll = 0;
                    } else {
                        activate_flow_step(app, -1);
                    }
                } else if app.file_index == 0 {
                    app.focus = Focus::Description;
                    app.description_expanded = true;
                    app.description_scroll = 0;
                } else {
                    change_file(app, -1)
                }
            } else if matches!(app.diff_navigation, DiffNavigation::Line) {
                move_line_in_block(app, -1)
            } else {
                move_change_block(app, -1)
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if matches!(app.focus, Focus::Description) {
                app.description_scroll = app.description_scroll.saturating_add(5);
            } else if matches!(app.focus, Focus::Files) {
                if app.flow_view {
                    app.flow_detail_scroll = app
                        .flow_detail_scroll
                        .saturating_add((app.diff_view_height / 2).max(1));
                } else {
                    move_file(app, page_size(app));
                }
            } else if matches!(app.diff_navigation, DiffNavigation::Line) {
                move_line_in_block(app, page_size(app));
            } else if !scroll_oversized_block(app, 1) {
                move_change_block(app, page_size(app));
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if matches!(app.focus, Focus::Description) {
                app.description_scroll = app.description_scroll.saturating_sub(5);
            } else if matches!(app.focus, Focus::Files) {
                if app.flow_view {
                    app.flow_detail_scroll = app
                        .flow_detail_scroll
                        .saturating_sub((app.diff_view_height / 2).max(1));
                } else {
                    move_file(app, -page_size(app));
                }
            } else if matches!(app.diff_navigation, DiffNavigation::Line) {
                move_line_in_block(app, -page_size(app));
            } else if !scroll_oversized_block(app, -1) {
                move_change_block(app, -page_size(app));
            }
        }
        KeyCode::Char('h') | KeyCode::Left => change_file(app, -1),
        KeyCode::Char('l') | KeyCode::Right => change_file(app, 1),
        KeyCode::Char(']') => {
            if activate_flow_step(app, 1) {
                app.focus = Focus::Diff;
                app.sidebar_visible = false;
            }
        }
        KeyCode::Char('[') => {
            if activate_flow_step(app, -1) {
                app.focus = Focus::Diff;
                app.sidebar_visible = false;
            }
        }
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
                    Focus::Description => Focus::Diff,
                    Focus::Files => Focus::Description,
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
        KeyCode::Char(':') => {
            app.editor.reset();
            app.mode = Mode::Command;
        }
        KeyCode::Char('n') if !app.search_query.is_empty() => {
            search(app, true);
        }
        KeyCode::Char('N') if !app.search_query.is_empty() => {
            search(app, false);
        }
        KeyCode::Char('d') => {
            app.sidebar_visible = true;
            app.focus = Focus::Description;
            app.description_expanded = true;
        }
        KeyCode::Char('v') => {
            let path = selected_file(app).path.clone();
            if let Err(error) = app.progress.toggle(&path) {
                app.mode = Mode::Message(error.to_string());
            }
        }
        KeyCode::Char('f') => {
            if matches!(app.flow_state, FlowState::Ready(_)) {
                app.flow_view = !app.flow_view;
                app.sidebar_visible = true;
                app.focus = Focus::Files;
                app.files_state.select(Some(app.file_index));
            }
        }
        KeyCode::Char('c') => {
            app.comment_index = app.comments.len().saturating_sub(1);
            app.mode = Mode::Comments;
        }
        KeyCode::Char('P') => app.mode = Mode::Submit,
        KeyCode::Char('V')
            if matches!(app.focus, Focus::Diff)
                && matches!(app.diff_navigation, DiffNavigation::Block) =>
        {
            app.diff_navigation = DiffNavigation::Line;
            app.line_mode_locked = false;
        }
        KeyCode::Enter => {
            if matches!(app.focus, Focus::Description) {
                app.description_expanded = !app.description_expanded;
                if !app.description_expanded {
                    app.description_scroll = 0;
                }
            } else if matches!(app.focus, Focus::Files) {
                if app.flow_view {
                    activate_flow_step(app, 0);
                }
                app.sidebar_visible = false;
                app.focus = Focus::Diff;
                if !app.flow_view {
                    reset_diff_navigation(app);
                }
            } else if selected_comment_target(app).is_some() {
                app.editor.reset();
                app.mode = Mode::Compose;
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
        Mode::Command => match app.editor.handle(key) {
            EditorAction::Cancel => app.mode = Mode::Browse,
            EditorAction::Submit => match app.editor.text.trim() {
                "q" => request_quit(app),
                "q!" => app.should_quit = true,
                command => {
                    app.mode = Mode::Message(format!("Unknown command: :{command}"));
                }
            },
            EditorAction::Continue => {}
        },
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
            KeyCode::Char('x') => {
                if app.comments.is_empty() {
                    app.mode = Mode::Message("No pending comments to copy.".to_string());
                } else {
                    match copy_to_clipboard(&review_handoff(app)) {
                        Ok(()) => {
                            app.mode =
                                Mode::Message("Comments copied to the clipboard.".to_string());
                        }
                        Err(error) => app.mode = Mode::Message(error.to_string()),
                    }
                }
            }
            KeyCode::Char(choice @ ('a' | 'r' | 'c')) => {
                if app.local_review.is_some() {
                    return;
                }
                let event = match choice {
                    'a' => "APPROVE",
                    'r' => "REQUEST_CHANGES",
                    _ => "COMMENT",
                };
                app.editor.reset();
                app.mode = Mode::ReviewSummary(event);
            }
            KeyCode::Char('s') if app.local_review.is_some() => {
                app.editor.reset();
                app.mode = Mode::ReviewSummary("LOCAL");
            }
            _ => {}
        },
        Mode::ReviewSummary(event) => match app.editor.handle(key) {
            EditorAction::Cancel => app.mode = Mode::Browse,
            EditorAction::Submit => {
                let event = *event;
                let summary = app.editor.text.trim().to_string();
                let result: Result<String> = if event == "LOCAL" {
                    (|| {
                        let path = save_local_review(app, &summary)?;
                        app.should_quit = true;
                        Ok(format!("Local review submitted: {}", path.display()))
                    })()
                } else {
                    submit(app, event, &summary).map(|_| format!("Review submitted as {}.", event))
                };
                match result {
                    Ok(message) => {
                        app.mode = Mode::Message(message);
                        app.comments.clear();
                    }
                    Err(error) => app.mode = Mode::Message(error.to_string()),
                }
            }
            EditorAction::Continue => {}
        },
        Mode::Compose if matches!(key.code, KeyCode::Esc) && app.editor.text.trim().is_empty() => {
            app.mode = Mode::Browse;
        }
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
    local_review: Option<LocalReview>,
) -> Result<bool> {
    let pull_json = gh(&[
        "pr",
        "view",
        &pr_number,
        "--repo",
        &repo,
        "--json",
        "title,body,author,headRefName,baseRefName,baseRefOid,headRefOid",
    ])?;
    let pull: PullRequest =
        serde_json::from_str(&pull_json).context("could not parse pull request")?;
    let diff = match local_review.as_ref() {
        Some(local_review) => {
            local_pull_request_worktree_diff(&local_review.workspace, &repo, &pull)?
        }
        None => match gh(&["pr", "diff", &pr_number, "--repo", &repo]) {
            Ok(diff) => diff,
            Err(error) if error.to_string().contains("PullRequest.diff too_large") => {
                local_pr_diff(&pr_number, &repo, &pull).context(
                    "could not generate a local diff after GitHub rejected the large pull request diff",
                )?
            }
            Err(error) => return Err(error).context("could not find pull request diff"),
        },
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
    let report_sections = report_sections(&files, &diff);
    let flow_state = load_flow_analysis(&repo, &pr_number, &pull.head_ref_oid);
    let flow_view = matches!(&flow_state, FlowState::Ready(_));
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
        description_expanded: false,
        description_scroll: 0,
        diff_scroll: 0,
        diff_view_height: 0,
        center_diff: true,
        pending_z: false,
        pending_space: false,
        auto_line_mode_pending: true,
        line_mode_locked: false,
        files_state: ListState::default(),
        search_query: String::new(),
        progress,
        briefing_open: false,
        briefing_state: BriefingState::Idle,
        briefing_scroll: 0,
        briefing_diff: diff,
        briefing_target: 0,
        return_to_briefing: false,
        report_sections,
        report_section: 0,
        report_content_focus: false,
        report_reference: 0,
        report_highlight: None,
        flow_view,
        flow_index: 0,
        flow_detail_scroll: 0,
        flow_state,
        local_review,
    };
    app.files_state.select(Some(0));
    reset_diff_navigation(&mut app);
    let workspace = tempfile::tempdir().context("could not create editor workspace")?;
    loop {
        poll_briefing(&mut app);
        terminal.draw(|frame| draw(&mut app, frame))?;
        if app.should_quit {
            return Ok(false);
        }
        if app.return_to_picker {
            return Ok(true);
        }
        if event::poll(Duration::from_millis(50))?
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

fn review_local_changes(
    terminal: &mut AppTerminal,
    workspace: PathBuf,
    base_revision: String,
    scope: LocalReviewScope,
    session_id: Option<String>,
    review_id: Option<String>,
    syntax_set: &SyntaxSet,
    syntax_theme: &Theme,
) -> Result<()> {
    let effective_base = match scope {
        LocalReviewScope::WorkingTree => base_revision.as_str(),
        LocalReviewScope::Unstaged => "HEAD",
        LocalReviewScope::LastCommit => "HEAD^",
    };
    let base_oid = git_at(&workspace, &["rev-parse", effective_base])?
        .trim()
        .to_string();
    let head_oid = git_at(&workspace, &["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let branch = git_at(&workspace, &["branch", "--show-current"])?
        .trim()
        .to_string();
    let diff = match scope {
        LocalReviewScope::WorkingTree => local_working_tree_diff(&workspace, &base_oid)?,
        LocalReviewScope::Unstaged => git_at(
            &workspace,
            &["diff", "--no-color", "--no-ext-diff", "--find-renames"],
        )?,
        LocalReviewScope::LastCommit => git_at(
            &workspace,
            &[
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--find-renames",
                &base_oid,
                "HEAD",
            ],
        )?,
    };
    let mut files = parse_diff(&diff);
    if files.is_empty() {
        bail!("No changes found for the selected local review scope.");
    }
    highlight_files(&mut files, syntax_set, syntax_theme);
    let review_id = review_id.unwrap_or_else(|| new_local_review_id(&session_id));
    let repo = workspace.to_string_lossy().to_string();
    let progress = ReviewProgress::load(&repo, &review_id, &head_oid)?;
    let title = match scope {
        LocalReviewScope::WorkingTree => "Local working-tree review",
        LocalReviewScope::Unstaged => "Unstaged changes review",
        LocalReviewScope::LastCommit => "Last commit review",
    }
    .to_string();
    let review_base = match scope {
        LocalReviewScope::WorkingTree => base_revision.clone(),
        LocalReviewScope::Unstaged => "unstaged changes".to_string(),
        LocalReviewScope::LastCommit => "HEAD^..HEAD".to_string(),
    };
    let pull = PullRequest {
        title: title.clone(),
        body: format!("Local diff for `{review_base}`."),
        author: Author {
            login: "local".to_string(),
        },
        head_ref_name: if branch.is_empty() {
            "detached HEAD".to_string()
        } else {
            branch
        },
        base_ref_name: review_base.clone(),
        base_ref_oid: base_oid,
        head_ref_oid: head_oid.clone(),
    };
    let report_sections = report_sections(&files, &diff);
    let flow_state = load_flow_analysis(&repo, &review_id, &head_oid);
    let flow_view = matches!(&flow_state, FlowState::Ready(_));
    let mut app = App {
        pr_number: review_id.clone(),
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
        description_expanded: false,
        description_scroll: 0,
        diff_scroll: 0,
        diff_view_height: 0,
        center_diff: true,
        pending_z: false,
        pending_space: false,
        auto_line_mode_pending: true,
        line_mode_locked: false,
        files_state: ListState::default(),
        search_query: String::new(),
        progress,
        briefing_open: false,
        briefing_state: BriefingState::Idle,
        briefing_scroll: 0,
        briefing_diff: diff,
        briefing_target: 0,
        return_to_briefing: false,
        report_sections,
        report_section: 0,
        report_content_focus: false,
        report_reference: 0,
        report_highlight: None,
        flow_view,
        flow_index: 0,
        flow_detail_scroll: 0,
        flow_state,
        local_review: Some(LocalReview {
            workspace: workspace.clone(),
            session_id,
            base_revision: review_base,
            review_id,
        }),
    };
    app.files_state.select(Some(0));
    reset_diff_navigation(&mut app);
    let editor_workspace = tempfile::tempdir().context("could not create editor workspace")?;
    loop {
        poll_briefing(&mut app);
        terminal.draw(|frame| draw(&mut app, frame))?;
        if app.should_quit || app.return_to_picker {
            return Ok(());
        }
        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
            && key.kind == event::KeyEventKind::Press
        {
            handle_event(&mut app, key, editor_workspace.path());
            if app.should_redraw {
                terminal.clear()?;
                app.should_redraw = false;
            }
        }
    }
}

fn new_local_review_id(session_id: &Option<String>) -> String {
    format!(
        "{}-{}",
        session_id.as_deref().unwrap_or("manual"),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    )
}

fn local_working_tree_diff(workspace: &Path, base_oid: &str) -> Result<String> {
    let mut diff = git_at(
        workspace,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            base_oid,
        ],
    )?;
    let untracked = git_at(
        workspace,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    for path in untracked.split('\0').filter(|path| !path.is_empty()) {
        let output = Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(["diff", "--no-index", "--no-color", "--", "/dev/null", path])
            .output()
            .context("could not generate an untracked-file diff")?;
        if !output.status.success() && output.status.code() != Some(1) {
            bail!(
                "could not generate a diff for untracked file {path}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        diff.push_str(
            &String::from_utf8(output.stdout)
                .context("git returned invalid UTF-8 for an untracked-file diff")?,
        );
    }
    Ok(diff)
}

fn run_in_terminal(
    repo: String,
    mut pulls: Vec<PullRequestChoice>,
    initial_pr: Option<String>,
    initial_picker_query: String,
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
        let mut picker_query = initial_picker_query;
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
                None,
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

fn run_local_review(
    base_revision: String,
    scope: LocalReviewScope,
    session_id: Option<String>,
    review_id: Option<String>,
) -> Result<()> {
    let workspace = PathBuf::from(
        git_at(Path::new("."), &["rev-parse", "--show-toplevel"])?
            .trim()
            .to_string(),
    );
    let syntax_set = SyntaxSet::load_defaults_newlines();
    let syntax_theme = ThemeSet::load_defaults()
        .themes
        .remove("base16-ocean.dark")
        .context("could not load default syntax theme")?;
    let session_id = match session_id {
        Some(session_id) => Some(session_id),
        None => active_codex_session(&workspace)?,
    };
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let outcome = review_local_changes(
        &mut terminal,
        workspace,
        base_revision,
        scope,
        session_id,
        review_id,
        &syntax_set,
        &syntax_theme,
    );
    let leave_result = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let raw_mode_result = disable_raw_mode();
    outcome?;
    leave_result?;
    raw_mode_result?;
    Ok(())
}

fn open_local_review_in_tmux(
    base_revision: String,
    scope: LocalReviewScope,
    review_id: String,
    wait: bool,
) -> Result<()> {
    if std::env::var_os("TMUX").is_none() {
        bail!("reviewer local-tmux requires Codex to run inside tmux");
    }
    let executable = std::env::current_exe().context("could not locate the Reviewer executable")?;
    let command = local_review_tmux_command(&executable, &base_revision, scope, &review_id);
    let status = Command::new("tmux")
        .args(["new-window", "-n", "reviewer", &command])
        .status()
        .context("could not create a Reviewer tmux window")?;
    if status.success() {
        if wait {
            print_local_review_prompt_when_submitted(&review_id)
        } else {
            println!("Opened a Reviewer tmux window.");
            Ok(())
        }
    } else {
        bail!("tmux new-window exited with status {status}")
    }
}

fn local_review_tmux_command(
    executable: &Path,
    base_revision: &str,
    scope: LocalReviewScope,
    review_id: &str,
) -> String {
    let scope_args = match scope {
        LocalReviewScope::WorkingTree => format!(" --base {}", shell_single_quote(base_revision)),
        LocalReviewScope::Unstaged => " --unstaged".to_string(),
        LocalReviewScope::LastCommit => " --last-commit".to_string(),
    };
    format!(
        "{} local --review-id {}{}",
        shell_single_quote(&executable.to_string_lossy()),
        shell_single_quote(review_id),
        scope_args,
    )
}

#[derive(Deserialize)]
struct CurrentPullRequest {
    number: u64,
}

fn current_pull_request_number() -> Result<String> {
    let response = gh(&["pr", "view", "--json", "number"])
        .context("could not determine the pull request for the current branch")?;
    let pull: CurrentPullRequest =
        serde_json::from_str(&response).context("could not parse the current pull request")?;
    Ok(pull.number.to_string())
}

fn run_local_pull_request_review(pr_number: String, review_id: String) -> Result<()> {
    let workspace = PathBuf::from(
        git_at(Path::new("."), &["rev-parse", "--show-toplevel"])?
            .trim()
            .to_string(),
    );
    let repo = current_repo()?;
    let session_id = active_codex_session(&workspace)?;
    let syntax_set = SyntaxSet::load_defaults_newlines();
    let syntax_theme = ThemeSet::load_defaults()
        .themes
        .remove("base16-ocean.dark")
        .context("could not load default syntax theme")?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let outcome = review_pull_request(
        &mut terminal,
        pr_number.clone(),
        repo,
        &syntax_set,
        &syntax_theme,
        Some(LocalReview {
            workspace,
            session_id,
            base_revision: format!("pull request #{pr_number}"),
            review_id,
        }),
    );
    let leave_result = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let raw_mode_result = disable_raw_mode();
    outcome?;
    leave_result?;
    raw_mode_result?;
    Ok(())
}

fn open_pull_request_review_in_tmux(
    pr_number: String,
    review_id: String,
    wait: bool,
) -> Result<()> {
    if std::env::var_os("TMUX").is_none() {
        bail!("reviewer pr-tmux requires Codex to run inside tmux");
    }
    let executable = std::env::current_exe().context("could not locate the Reviewer executable")?;
    let command = format!(
        "{} pr-local --review-id {} {}",
        shell_single_quote(&executable.to_string_lossy()),
        shell_single_quote(&review_id),
        shell_single_quote(&pr_number)
    );
    let status = Command::new("tmux")
        .args(["new-window", "-n", "reviewer", &command])
        .status()
        .context("could not create a Reviewer tmux window")?;
    if !status.success() {
        bail!("tmux new-window exited with status {status}");
    }
    if wait {
        print_local_review_prompt_when_submitted(&review_id)
    } else {
        println!("Opened pull request #{pr_number} in a Reviewer tmux window.");
        Ok(())
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Debug, PartialEq, Eq)]
struct LocalReviewOptions {
    base_revision: String,
    scope: LocalReviewScope,
    session_id: Option<String>,
    review_id: Option<String>,
    wait: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalReviewScope {
    WorkingTree,
    Unstaged,
    LastCommit,
}

fn local_review_args(args: &[String]) -> Result<LocalReviewOptions> {
    let mut base_revision = "HEAD".to_string();
    let mut base_was_set = false;
    let mut scope = LocalReviewScope::WorkingTree;
    let mut session_id = None;
    let mut review_id = None;
    let mut wait = false;
    let mut index = 0;
    while index < args.len() {
        let option = &args[index];
        if option == "--wait" {
            wait = true;
            index += 1;
            continue;
        }
        if matches!(option.as_str(), "--unstaged" | "--last-commit") {
            let next_scope = if option == "--unstaged" {
                LocalReviewScope::Unstaged
            } else {
                LocalReviewScope::LastCommit
            };
            if scope != LocalReviewScope::WorkingTree {
                bail!("--unstaged and --last-commit are mutually exclusive");
            }
            if base_was_set {
                bail!("--base cannot be combined with {option}");
            }
            scope = next_scope;
            index += 1;
            continue;
        }
        index += 1;
        let value = match option.as_str() {
            "--base" | "--session" | "--review-id" => args
                .get(index)
                .filter(|value| !value.is_empty())
                .context(format!("{option} requires a value"))?
                .clone(),
            _ => bail!("unknown local-review option: {option}"),
        };
        match option.as_str() {
            "--base" => {
                if scope != LocalReviewScope::WorkingTree {
                    bail!("--base cannot be combined with a local review scope flag");
                }
                base_revision = value;
                base_was_set = true;
            }
            "--session" => session_id = Some(value),
            "--review-id" => review_id = Some(value),
            _ => unreachable!(),
        }
        index += 1;
    }
    Ok(LocalReviewOptions {
        base_revision,
        scope,
        session_id,
        review_id,
        wait,
    })
}

fn pull_request_review_args(
    args: &[String],
) -> Result<(
    Option<String>,
    Option<String>,
    bool,
    Option<LocalReviewScope>,
)> {
    let mut pr_number = None;
    let mut review_id = None;
    let mut wait = false;
    let mut local_scope = None;
    let mut index = 0;
    while index < args.len() {
        let argument = &args[index];
        if argument == "--wait" {
            wait = true;
            index += 1;
            continue;
        }
        if matches!(argument.as_str(), "--unstaged" | "--last-commit") {
            let scope = if argument == "--unstaged" {
                LocalReviewScope::Unstaged
            } else {
                LocalReviewScope::LastCommit
            };
            if local_scope.replace(scope).is_some() {
                bail!("--unstaged and --last-commit are mutually exclusive");
            }
            index += 1;
            continue;
        }
        if argument == "--review-id" {
            index += 1;
            let value = args
                .get(index)
                .filter(|value| !value.is_empty())
                .context("--review-id requires a value")?;
            review_id = Some(value.clone());
        } else if argument.starts_with('-') {
            bail!("unknown pull-request review option: {argument}");
        } else if pr_number.replace(argument.clone()).is_some() {
            bail!("only one pull-request number may be supplied");
        }
        index += 1;
    }
    if local_scope.is_some() && pr_number.is_some() {
        bail!("a pull-request number cannot be combined with a local review scope");
    }
    Ok((pr_number, review_id, wait, local_scope))
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

#[derive(Deserialize, Serialize)]
struct PeerReviewJob {
    repo: String,
    pr_number: String,
    state: String,
    phase: String,
    model: String,
    head_oid: Option<String>,
    started_at: u64,
    updated_at: u64,
    pid: Option<u32>,
    error: Option<String>,
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn peer_job_path(repo: &str, pr_number: &str) -> PathBuf {
    checkout_cache_root().join("peer-reviews").join(format!(
        "{}-{pr_number}.json",
        repo.replace(['/', '\\'], "-")
    ))
}

fn peer_log_path(repo: &str, pr_number: &str) -> PathBuf {
    checkout_cache_root().join("peer-reviews").join(format!(
        "{}-{pr_number}.log",
        repo.replace(['/', '\\'], "-")
    ))
}

fn save_peer_job(job: &PeerReviewJob) -> Result<()> {
    let path = peer_job_path(&job.repo, &job.pr_number);
    let parent = path
        .parent()
        .context("peer-review status path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(job)?)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    fs::rename(&temporary, &path).with_context(|| format!("could not replace {}", path.display()))
}

fn peer_review_args(args: &[String]) -> Result<(String, String)> {
    let (repo, pr_number) = parse_cli_args(args)?;
    Ok((
        repo.context("peer-review commands require --repo owner/name")?,
        pr_number.context("peer-review commands require a pull-request number")?,
    ))
}

fn start_peer_review(repo: String, pr_number: String) -> Result<()> {
    let model = std::env::var("REVIEWER_PEER_MODEL")
        .unwrap_or_else(|_| "openai/gpt-5.6-sol:nitro".to_string());
    let now = unix_time();
    let existing_path = peer_job_path(&repo, &pr_number);
    if let Ok(contents) = fs::read(&existing_path)
        && let Ok(existing) = serde_json::from_slice::<PeerReviewJob>(&contents)
        && matches!(existing.state.as_str(), "queued" | "running")
        && now.saturating_sub(existing.updated_at) < 600
    {
        bail!(
            "a peer review is already {} for {repo}#{pr_number}; run `reviewer peer-review-status --repo {repo} {pr_number}`",
            existing.state
        );
    }
    let job = PeerReviewJob {
        repo: repo.clone(),
        pr_number: pr_number.clone(),
        state: "queued".to_string(),
        phase: "Starting detached worker".to_string(),
        model,
        head_oid: None,
        started_at: now,
        updated_at: now,
        pid: None,
        error: None,
    };
    save_peer_job(&job)?;
    let log_path = peer_log_path(&repo, &pr_number);
    let log = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&log_path)
        .with_context(|| format!("could not open {}", log_path.display()))?;
    let error_log = log.try_clone()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["__peer-review-worker", "--repo", &repo, &pr_number])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .context("could not start peer-review worker")?;
    println!(
        "Started peer review for {repo}#{pr_number} (pid {}).",
        child.id()
    );
    println!("Status: reviewer peer-review-status --repo {repo} {pr_number}");
    println!("Log: {}", log_path.display());
    Ok(())
}

fn run_peer_review_worker(repo: String, pr_number: String) -> Result<()> {
    let path = peer_job_path(&repo, &pr_number);
    let mut job: PeerReviewJob = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("could not read {}", path.display()))?,
    )?;
    let outcome = (|| -> Result<()> {
        let log_phase = |phase: &str| {
            println!("[{}] {phase}", unix_time());
            let _ = io::stdout().flush();
        };
        job.pid = Some(std::process::id());
        job.state = "running".to_string();
        job.phase = "Fetching PR description and diff".to_string();
        job.updated_at = unix_time();
        save_peer_job(&job)?;
        log_phase(&job.phase);
        let pull_json = gh(&[
            "pr",
            "view",
            &pr_number,
            "--repo",
            &repo,
            "--json",
            "title,body,author,headRefName,baseRefName,baseRefOid,headRefOid",
        ])?;
        let pull: PullRequest = serde_json::from_str(&pull_json)?;
        let diff = match gh(&["pr", "diff", &pr_number, "--repo", &repo]) {
            Ok(diff) => diff,
            Err(error) if error.to_string().contains("PullRequest.diff too_large") => {
                local_pr_diff(&pr_number, &repo, &pull)?
            }
            Err(error) => return Err(error),
        };
        job.head_oid = Some(pull.head_ref_oid.clone());
        job.phase = "Starting deep model review".to_string();
        job.updated_at = unix_time();
        save_peer_job(&job)?;
        log_phase(&job.phase);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || flow::generate_peer_review(pull.title, pull.body, diff, sender));
        let mut last_heartbeat = unix_time();
        loop {
            match receiver.recv_timeout(Duration::from_secs(10)) {
                Ok(FlowEvent::Status(phase)) => {
                    job.phase = phase;
                    job.updated_at = unix_time();
                    save_peer_job(&job)?;
                    log_phase(&job.phase);
                }
                Ok(FlowEvent::Complete(result)) => {
                    let plan = result.map_err(anyhow::Error::msg)?;
                    let head = job
                        .head_oid
                        .as_deref()
                        .context("peer review has no head oid")?;
                    flow::save_plan(&repo, &pr_number, head, &plan)?;
                    job.state = "completed".to_string();
                    job.phase = format!("Ready · {} review stages", plan.steps.len());
                    job.updated_at = unix_time();
                    save_peer_job(&job)?;
                    log_phase(&job.phase);
                    return Ok(());
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if unix_time().saturating_sub(last_heartbeat) >= 30 {
                        println!(
                            "[{}] worker alive · waiting for model · {}s elapsed",
                            unix_time(),
                            unix_time().saturating_sub(job.started_at)
                        );
                        let _ = io::stdout().flush();
                        last_heartbeat = unix_time();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("peer-review generator stopped unexpectedly")
                }
            }
        }
    })();
    if let Err(error) = &outcome {
        job.state = "failed".to_string();
        job.phase = "Peer review failed".to_string();
        job.error = Some(format!("{error:#}"));
        job.updated_at = unix_time();
        let _ = save_peer_job(&job);
        eprintln!("[{}] failed · {error:#}", unix_time());
    }
    outcome
}

fn show_peer_review_status(repo: String, pr_number: String) -> Result<()> {
    let path = peer_job_path(&repo, &pr_number);
    let job: PeerReviewJob = serde_json::from_slice(&fs::read(&path).with_context(|| {
        format!(
            "no peer-review job found for {repo}#{pr_number}; expected {}",
            path.display()
        )
    })?)?;
    println!("{}#{} · {}", job.repo, job.pr_number, job.state);
    println!("Phase: {}", job.phase);
    println!("Model: {}", job.model);
    let elapsed_until = if matches!(job.state.as_str(), "queued" | "running") {
        unix_time()
    } else {
        job.updated_at
    };
    println!("Elapsed: {}s", elapsed_until.saturating_sub(job.started_at));
    println!(
        "Last progress: {}s ago",
        unix_time().saturating_sub(job.updated_at)
    );
    if let Some(pid) = job.pid {
        println!("PID: {pid}");
    }
    if let Some(head) = &job.head_oid {
        println!("Head: {head}");
        if job.state == "completed" {
            println!(
                "Cache: {}",
                flow::cache_path(&repo, &pr_number, head).display()
            );
        }
    }
    if let Some(error) = &job.error {
        println!("Error: {error}");
    }
    println!("Log: {}", peer_log_path(&repo, &pr_number).display());
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [flag] if matches!(flag.as_str(), "-h" | "--help")) {
        println!("{}", cli_usage());
        return Ok(());
    }
    if matches!(args.as_slice(), [flag] if matches!(flag.as_str(), "-V" | "--version")) {
        println!("reviewer {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if let Some(command) = args.first().map(String::as_str) {
        match command {
            "local" => {
                let options = local_review_args(&args[1..])?;
                if options.wait {
                    bail!("--wait is only supported with reviewer local-tmux");
                }
                return run_local_review(
                    options.base_revision,
                    options.scope,
                    options.session_id,
                    options.review_id,
                );
            }
            "local-tmux" => {
                let options = local_review_args(&args[1..])?;
                if options.session_id.is_some() {
                    bail!("reviewer local-tmux records the active Codex session automatically");
                }
                let review_id = options
                    .review_id
                    .unwrap_or_else(|| new_local_review_id(&None));
                return open_local_review_in_tmux(
                    options.base_revision,
                    options.scope,
                    review_id,
                    options.wait,
                );
            }
            "pr-tmux" => {
                let (pr_number, review_id, wait, local_scope) =
                    pull_request_review_args(&args[1..])?;
                if let Some(scope) = local_scope {
                    let review_id = review_id.unwrap_or_else(|| new_local_review_id(&None));
                    return open_local_review_in_tmux("HEAD".to_string(), scope, review_id, wait);
                }
                let pr_number = match pr_number {
                    Some(pr_number) => pr_number,
                    None => current_pull_request_number()?,
                };
                let review_id = review_id.unwrap_or_else(|| new_local_review_id(&None));
                return open_pull_request_review_in_tmux(pr_number, review_id, wait);
            }
            "codex-tmux" => return run_codex_tmux_review(&args[1..]),
            "pr-local" => {
                let (pr_number, review_id, wait, local_scope) =
                    pull_request_review_args(&args[1..])?;
                if local_scope.is_some() {
                    bail!("local review scope flags are only supported with reviewer pr-tmux");
                }
                if wait {
                    bail!("--wait is only supported with reviewer pr-tmux");
                }
                let pr_number =
                    pr_number.context("reviewer pr-local requires a pull-request number")?;
                let review_id = review_id.context("reviewer pr-local requires --review-id")?;
                return run_local_pull_request_review(pr_number, review_id);
            }
            "codex-prompt" => {
                let review_id = args
                    .get(1)
                    .filter(|value| !value.is_empty())
                    .context("usage: reviewer codex-prompt <review-id>")?;
                return print_codex_prompt(review_id);
            }
            "latest-codex-prompt" => return print_latest_codex_prompt(),
            "codex-session-start" => return record_codex_session(),
            "peer-review" => {
                let (repo, pr) = peer_review_args(&args[1..])?;
                return start_peer_review(repo, pr);
            }
            "peer-review-status" => {
                let (repo, pr) = peer_review_args(&args[1..])?;
                return show_peer_review_status(repo, pr);
            }
            "__peer-review-worker" => {
                let (repo, pr) = peer_review_args(&args[1..])?;
                return run_peer_review_worker(repo, pr);
            }
            _ => {}
        }
    }
    let (repo_override, pr_number) =
        parse_cli_args(&args).map_err(|error| anyhow::anyhow!("{error}\n\n{}", cli_usage()))?;
    let repo = match repo_override {
        Some(repo) => repo,
        None => current_repo()?,
    };
    let picker_query = load_picker_query(&repo)
        .unwrap_or(None)
        .unwrap_or_else(|| "is:open".to_string());
    let pulls = search_pull_requests(&repo, &picker_query)?;
    run_in_terminal(repo, pulls, pr_number, picker_query)
}

#[cfg(test)]
mod tests {
    use super::{
        App, Author, BriefingState, CLI_COMMANDS, Color, DiffNavigation, FlowPlan, FlowState,
        Focus, LineKind, ListState, Mode, PendingComment, PullRequest, ReviewProgress, Side,
        TextEditor, active_author_prefix, author_suggestions, change_block_at, cli_usage,
        codex_tmux_args, comment_target, complete_author, editor_overlay, editor_render_text,
        editor_status_label, handle_browse, handle_event, highlight_files,
        is_entirely_added_or_removed, line_matches, local_review_args, local_review_prompt,
        local_review_tmux_command, markdown_inline, parse_cli_args, parse_diff,
        replace_picker_results, report_sections, review_handoff, search, shell_single_quote,
        update_diff_scroll,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use std::path::Path;
    use syntect::highlighting::ThemeSet;
    use syntect::parsing::SyntaxSet;

    #[test]
    fn cli_help_documents_public_entry_points() {
        let usage = cli_usage();
        for command in CLI_COMMANDS {
            assert!(usage.contains(command), "missing help for {command}");
        }
        assert!(usage.contains("-R, --repo OWNER/NAME"));
        assert!(usage.contains("-V, --version"));
    }

    #[test]
    fn parses_local_review_options() {
        let args = vec![
            "--base".to_string(),
            "main".to_string(),
            "--session".to_string(),
            "session-123".to_string(),
        ];
        assert_eq!(
            local_review_args(&args).unwrap(),
            super::LocalReviewOptions {
                base_revision: "main".to_string(),
                scope: super::LocalReviewScope::WorkingTree,
                session_id: Some("session-123".to_string()),
                review_id: None,
                wait: false,
            }
        );
    }

    #[test]
    fn parses_scoped_local_review_options() {
        let unstaged =
            local_review_args(&["--unstaged".to_string(), "--wait".to_string()]).unwrap();
        assert_eq!(unstaged.scope, super::LocalReviewScope::Unstaged);
        assert!(unstaged.wait);

        let last_commit = local_review_args(&["--last-commit".to_string()]).unwrap();
        assert_eq!(last_commit.scope, super::LocalReviewScope::LastCommit);
    }

    #[test]
    fn rejects_conflicting_local_review_scopes() {
        assert!(
            local_review_args(&["--unstaged".to_string(), "--last-commit".to_string()]).is_err()
        );
        assert!(
            local_review_args(&[
                "--base".to_string(),
                "main".to_string(),
                "--last-commit".to_string(),
            ])
            .is_err()
        );
    }

    #[test]
    fn quotes_tmux_command_arguments() {
        assert_eq!(shell_single_quote("main"), "'main'");
        assert_eq!(
            shell_single_quote("feature's branch"),
            "'feature'\\''s branch'"
        );
    }

    #[test]
    fn scoped_tmux_commands_do_not_include_a_conflicting_base() {
        let executable = Path::new("/tmp/reviewer");
        assert_eq!(
            local_review_tmux_command(
                executable,
                "HEAD",
                super::LocalReviewScope::Unstaged,
                "review-1",
            ),
            "'/tmp/reviewer' local --review-id 'review-1' --unstaged"
        );
        assert_eq!(
            local_review_tmux_command(
                executable,
                "main",
                super::LocalReviewScope::WorkingTree,
                "review-1",
            ),
            "'/tmp/reviewer' local --review-id 'review-1' --base 'main'"
        );
    }

    #[test]
    fn local_review_prompt_preserves_line_specific_feedback() {
        let submission = super::LocalReviewSubmission {
            version: 1,
            review_id: "review-1".to_string(),
            workspace: Path::new("/tmp/project").to_path_buf(),
            session_id: Some("session-123".to_string()),
            base_revision: "HEAD".to_string(),
            submitted_at: 1,
            summary: "Check error handling.".to_string(),
            comments: vec![PendingComment {
                path: "src/lib.rs".to_string(),
                line: 42,
                side: Side::Right,
                body: "Return the underlying error here.".to_string(),
            }],
            diff: String::new(),
        };
        let prompt = local_review_prompt(&submission);
        assert!(prompt.contains("LOCAL REVIEW SUBMITTED"));
        assert!(prompt.contains("`src/lib.rs` line 42 (new side)"));
        assert!(prompt.contains("Return the underlying error here."));
        assert!(prompt.contains("Check error handling."));
    }

    #[test]
    fn styles_inline_markdown_code_and_bold_text() {
        let spans = markdown_inline(
            "Call `load_user` with **validated** input",
            &["load_user".to_string()],
            usize::MAX,
        );
        assert_eq!(spans[1].content, "load_user");
        assert_eq!(spans[1].style.fg, Some(Color::LightCyan));
        assert!(
            spans[3]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

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
    fn block_comments_prefer_the_new_side_while_line_comments_use_the_selected_side() {
        let files = parse_diff(
            "diff --git a/src/main.rs b/src/main.rs\n@@ -4,2 +4,2 @@\n context\n-removed\n+added\n",
        );
        let file = &files[0];

        assert_eq!(
            comment_target(file, 2, DiffNavigation::Block),
            Some((5, Side::Right))
        );
        assert_eq!(
            comment_target(file, 2, DiffNavigation::Line),
            Some((5, Side::Left))
        );
        assert_eq!(
            comment_target(file, 3, DiffNavigation::Line),
            Some((5, Side::Right))
        );
    }

    #[test]
    fn recognizes_diffs_whose_content_is_all_added_or_all_removed() {
        let added = parse_diff(
            "diff --git a/new.rs b/new.rs\nnew file mode 100644\n@@ -0,0 +1,2 @@\n+one\n+two\n",
        );
        let removed = parse_diff(
            "diff --git a/old.rs b/old.rs\ndeleted file mode 100644\n@@ -1,2 +0,0 @@\n-one\n-two\n",
        );
        let partial = parse_diff(
            "diff --git a/edit.rs b/edit.rs\n@@ -1,2 +1,3 @@\n context\n+extra\n context\n",
        );

        assert!(is_entirely_added_or_removed(&added[0]));
        assert!(is_entirely_added_or_removed(&removed[0]));
        assert!(!is_entirely_added_or_removed(&partial[0]));
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
            "diff --git a/one.rs b/one.rs\n@@ -1 +1,5 @@\n+needle first\n+companion one\n+companion two\n+companion three\n+companion four\ndiff --git a/two.rs b/two.rs\n@@ -1 +1 @@\n+needle second\n",
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
            description_expanded: false,
            description_scroll: 0,
            diff_scroll: 0,
            diff_view_height: 0,
            center_diff: true,
            pending_z: false,
            pending_space: false,
            auto_line_mode_pending: true,
            line_mode_locked: false,
            files_state: ListState::default(),
            search_query: "needle".to_string(),
            progress: ReviewProgress::load("owner/repo", "1", "head").unwrap(),
            briefing_open: false,
            briefing_state: BriefingState::Idle,
            briefing_scroll: 0,
            briefing_diff: String::new(),
            briefing_target: 0,
            return_to_briefing: false,
            report_sections: report_sections(&[], ""),
            report_section: 0,
            report_content_focus: false,
            report_reference: 0,
            report_highlight: None,
            flow_view: false,
            flow_index: 0,
            flow_detail_scroll: 0,
            flow_state: FlowState::Failed("not configured in test".to_string()),
            local_review: None,
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
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(matches!(app.focus, Focus::Description));
        assert!(app.description_expanded);
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Path::new("."),
        );
        assert_eq!(app.description_scroll, 5);
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(!app.description_expanded);
        assert!(matches!(app.focus, Focus::Files));
        assert_eq!(app.file_index, 0);

        app.flow_view = true;
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(matches!(app.focus, Focus::Description));
        assert!(app.description_expanded);
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(matches!(app.focus, Focus::Files));
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            Path::new("."),
        );
        assert!(app.flow_detail_scroll > 0);
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Path::new("."),
        );
        assert_eq!(app.flow_detail_scroll, 0);
        app.flow_view = false;

        app.flow_state = FlowState::Ready(FlowPlan { steps: Vec::new() });
        app.sidebar_visible = false;
        app.focus = Focus::Diff;
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(app.flow_view);
        assert!(app.sidebar_visible);
        assert!(matches!(app.focus, Focus::Files));
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(!app.flow_view);

        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(!app.sidebar_visible);
        assert!(matches!(app.focus, Focus::Diff));
        assert!(matches!(app.diff_navigation, DiffNavigation::Line));
        assert_eq!(app.line_index, 1);
        assert_eq!(app.diff_scroll, 0);
        assert!(!app.center_diff);

        for key in [' ', 'l'] {
            handle_browse(
                &mut app,
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
                Path::new("."),
            );
        }
        assert!(app.sidebar_visible);
        assert!(matches!(app.focus, Focus::Files));
        for key in [' ', 'l'] {
            handle_browse(
                &mut app,
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
                Path::new("."),
            );
        }
        assert!(!app.sidebar_visible);
        assert!(matches!(app.focus, Focus::Diff));

        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(matches!(app.mode, Mode::Compose));

        app.mode = Mode::Browse;
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('V'), KeyModifiers::SHIFT),
            Path::new("."),
        );
        assert!(matches!(app.diff_navigation, DiffNavigation::Line));

        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(app.sidebar_visible);
        assert!(matches!(app.focus, Focus::Files));
        assert!(matches!(app.diff_navigation, DiffNavigation::Line));
        assert!(app.line_mode_locked);

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

        app.mode = Mode::Browse;
        app.focus = Focus::Diff;
        app.diff_navigation = DiffNavigation::Block;
        app.center_diff = false;
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(app.center_diff);
        app.center_diff = false;
        for key in ['z', 'z'] {
            handle_browse(
                &mut app,
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE),
                Path::new("."),
            );
        }
        assert!(app.center_diff);

        app.file_index = 0;
        app.line_index = 1;
        app.diff_scroll = 0;
        app.diff_navigation = DiffNavigation::Block;
        app.auto_line_mode_pending = true;
        update_diff_scroll(&mut app, 2);
        assert!(matches!(app.diff_navigation, DiffNavigation::Line));
        assert_eq!(app.diff_scroll, 0);
        assert_eq!(app.line_index, 1);
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Path::new("."),
        );
        update_diff_scroll(&mut app, 2);
        assert!(matches!(app.diff_navigation, DiffNavigation::Line));
        assert!(app.sidebar_visible);
        assert!(matches!(app.focus, Focus::Files));
        app.sidebar_visible = false;
        app.focus = Focus::Diff;
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            Path::new("."),
        );
        assert_eq!(app.line_index, 2);
        update_diff_scroll(&mut app, 2);
        assert_eq!(app.diff_scroll, 1);

        app.focus = Focus::Files;
        app.sidebar_visible = true;
        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(app.return_to_picker);

        app.return_to_picker = false;
        app.mode = Mode::Compose;
        app.editor.reset();
        handle_event(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(matches!(app.mode, Mode::Browse));

        handle_browse(
            &mut app,
            KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT),
            Path::new("."),
        );
        assert!(matches!(app.mode, Mode::Submit));

        app.comments.push(PendingComment {
            path: "two.rs".to_string(),
            line: 1,
            side: Side::Right,
            body: "pending".to_string(),
        });
        app.pull.title = "Improve review handoffs".to_string();
        app.briefing_diff = "diff --git a/two.rs b/two.rs\n+pending\n".to_string();
        let handoff = review_handoff(&app);
        assert!(handoff.contains("# Pull request review handoff"));
        assert!(handoff.contains("`two.rs` at `1` on `the new side`"));
        assert!(handoff.contains("pending"));
        assert!(!handoff.contains("diff --git a/two.rs b/two.rs"));

        app.mode = Mode::Command;
        app.editor.reset();
        app.editor.text = "q".to_string();
        app.editor.cursor = 1;
        handle_event(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(!app.should_quit);
        assert!(matches!(app.mode, Mode::Message(_)));

        app.mode = Mode::Command;
        app.editor.reset();
        app.editor.text = "q!".to_string();
        app.editor.cursor = 2;
        handle_event(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            Path::new("."),
        );
        assert!(app.should_quit);
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
    fn wraps_flow_descriptions_without_dropping_words() {
        let source =
            "Review the public contract before tracing how downstream behavior consumes it";
        let lines = super::wrap_words(source, 24);
        assert!(lines.iter().all(|line| line.chars().count() <= 24));
        assert_eq!(lines.join(" "), source);
    }

    #[test]
    fn wraps_flow_titles_with_aligned_continuation_lines() {
        let lines = super::flow_step_title_lines(
            1,
            "Persistence boundaries and crash-safe recovery behavior",
            24,
        );
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|line| line.chars().count() <= 24));
        assert!(lines[0].starts_with("2. "));
        assert!(lines[1].starts_with("   "));
        assert_eq!(
            lines
                .iter()
                .map(|line| line.trim())
                .collect::<Vec<_>>()
                .join(" "),
            "2. Persistence boundaries and crash-safe recovery behavior"
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

        assert_eq!(editor.text, "azc");
    }

    #[test]
    fn input_editor_uses_shift_enter_for_a_newline() {
        let mut editor = TextEditor::new();
        editor.handle(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(matches!(
            editor.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            super::EditorAction::Continue
        ));
        editor.handle(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));

        assert_eq!(editor.text, "a\nb");
        assert!(matches!(
            editor.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            super::EditorAction::Submit
        ));
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
        assert_eq!(editor.text, "one three");

        editor.text = "keep\ndelete\nkeep too".to_string();
        editor.cursor = "keep\ndelete".len();
        for key in ['d', 'd'] {
            editor.handle(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        assert_eq!(editor.text, "keep\nkeep too");
    }

    #[test]
    fn input_editor_composes_change_with_line_end_motion() {
        let mut editor = TextEditor::new();
        editor.text = "alpha target omega".to_string();
        editor.cursor = "alpha ".len();
        editor.mode = super::EditorMode::Normal;

        for key in ['c', '$', 'r', 'e', 'p', 'l', 'a', 'c', 'e', 'd'] {
            editor.handle(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }

        assert_eq!(editor.text, "alpha replaced");
        assert!(matches!(editor.mode, super::EditorMode::Insert));
    }

    #[test]
    fn input_editor_supports_counts_and_quoted_text_objects() {
        let mut editor = TextEditor::new();
        editor.text = "one two three four".to_string();
        editor.mode = super::EditorMode::Normal;
        for key in ['d', '2', 'w'] {
            editor.handle(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        assert_eq!(editor.text, "three four");

        editor.text = "say \"hello world\" now".to_string();
        editor.cursor = "say \"hel".len();
        editor.mode = super::EditorMode::Normal;
        for key in ['c', 'i', '"', 'x'] {
            editor.handle(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        assert_eq!(editor.text, "say \"x\" now");
        assert!(matches!(editor.mode, super::EditorMode::Insert));
    }

    #[test]
    fn input_editor_displays_counts_and_visual_selections() {
        let mut editor = TextEditor::new();
        editor.text = "one two three four".to_string();
        editor.mode = super::EditorMode::Normal;
        editor.handle(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        assert!(editor_status_label(&editor).contains('3'));
        editor.handle(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert_eq!(editor.cursor, "one two three ".len());

        editor.text = "alpha target omega".to_string();
        editor.cursor = "alpha tar".len();
        editor.mode = super::EditorMode::Normal;
        for key in ['v', 'i', 'w'] {
            editor.handle(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        let rendered = editor_render_text(&editor);
        let selection = rendered.lines[0]
            .spans
            .iter()
            .find(|span| span.style.bg.is_some())
            .expect("visual selection is highlighted");
        assert_eq!(selection.content.as_ref(), "target");

        for key in ['c', 'x'] {
            editor.handle(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        }
        assert_eq!(editor.text, "alpha x omega");
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
    fn refreshed_picker_results_preserve_the_selected_pull_request() {
        let mut pulls = serde_json::from_str(
            r#"[{"number":1,"title":"One","author":{"login":"alice"},"headRefName":"one"},{"number":2,"title":"Two","author":{"login":"bob"},"headRefName":"two"}]"#,
        )
        .unwrap();
        let refreshed = serde_json::from_str(
            r#"[{"number":3,"title":"Three","author":{"login":"carol"},"headRefName":"three"},{"number":2,"title":"Updated","author":{"login":"bob"},"headRefName":"two"}]"#,
        )
        .unwrap();
        let mut contributors = vec!["alice".to_string(), "bob".to_string()];

        let selected = replace_picker_results(&mut pulls, &mut contributors, refreshed, Some(2));

        assert_eq!(selected, 1);
        assert_eq!(pulls[selected].title, "Updated");
        assert_eq!(contributors, ["alice", "bob", "carol"]);
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

    #[test]
    fn parses_repository_and_pr_for_peer_review_commands() {
        let args = ["--repo", "acme/widgets", "42"].map(str::to_string);
        assert_eq!(
            super::peer_review_args(&args).unwrap(),
            ("acme/widgets".to_string(), "42".to_string())
        );
    }

    #[test]
    fn parses_pull_request_local_review_options() {
        let args = ["--review-id", "review-123", "--wait", "42"].map(str::to_string);
        assert_eq!(
            super::pull_request_review_args(&args).unwrap(),
            (
                Some("42".to_string()),
                Some("review-123".to_string()),
                true,
                None
            )
        );
    }

    #[test]
    fn parses_local_scopes_through_the_codex_review_entry_point() {
        let args = ["--wait", "--unstaged"].map(str::to_string);
        assert_eq!(
            super::pull_request_review_args(&args).unwrap(),
            (None, None, true, Some(super::LocalReviewScope::Unstaged))
        );

        let conflicting = ["--unstaged", "--last-commit"].map(str::to_string);
        assert!(super::pull_request_review_args(&conflicting).is_err());
    }

    #[test]
    fn separates_codex_target_pane_from_review_options() {
        let args = ["--target-pane", "%42", "--unstaged"].map(str::to_string);
        assert_eq!(
            codex_tmux_args(&args).unwrap(),
            (
                Some("%42".to_string()),
                false,
                vec!["--unstaged".to_string()]
            )
        );

        let missing = ["--target-pane"].map(str::to_string);
        assert!(codex_tmux_args(&missing).is_err());

        let fallback = ["--unstaged-or-pr"].map(str::to_string);
        assert_eq!(
            codex_tmux_args(&fallback).unwrap(),
            (None, true, Vec::new())
        );

        let duplicate = ["--unstaged-or-pr", "--unstaged-or-pr"].map(str::to_string);
        assert!(codex_tmux_args(&duplicate).is_err());
    }
}
