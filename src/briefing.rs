use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result, bail};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolChange {
    pub name: String,
    pub path: String,
    pub kind: &'static str,
}

pub enum BriefingEvent {
    Delta(String),
    Complete(Result<String, String>),
}

pub fn analyze_symbols(diff: &str) -> Vec<SymbolChange> {
    let mut path = String::new();
    let mut symbols: BTreeMap<(String, String), (bool, bool)> = BTreeMap::new();
    for line in diff.lines() {
        if let Some(next) = line.strip_prefix("+++ b/") {
            path = next.to_string();
            continue;
        }
        let (added, source) = if let Some(source) = line.strip_prefix('+') {
            (true, source)
        } else if let Some(source) = line.strip_prefix('-') {
            (false, source)
        } else {
            continue;
        };
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(name) = symbol_name(source) {
            let flags = symbols.entry((path.clone(), name)).or_default();
            if added {
                flags.0 = true
            } else {
                flags.1 = true
            }
        }
    }
    symbols
        .into_iter()
        .map(|((path, name), (added, removed))| SymbolChange {
            name,
            path,
            kind: match (added, removed) {
                (true, true) => "modified",
                (true, false) => "added",
                (false, true) => "removed",
                _ => unreachable!(),
            },
        })
        .collect()
}

fn symbol_name(line: &str) -> Option<String> {
    let mut line = line.trim_start();
    if let Some(rest) = line.strip_prefix("pub(")
        && let Some((_, rest)) = rest.split_once(") ")
    {
        line = rest;
    } else if let Some(rest) = line.strip_prefix("pub ") {
        line = rest;
    } else if let Some(rest) = line.strip_prefix("export ") {
        line = rest;
    }
    if let Some(rest) = line.strip_prefix("async ") {
        line = rest;
    }
    for prefix in [
        "fn ",
        "function ",
        "def ",
        "class ",
        "struct ",
        "enum ",
        "trait ",
        "interface ",
        "type ",
    ] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            return (!name.is_empty()).then_some(name);
        }
    }
    for prefix in ["const ", "let ", "var "] {
        if let Some(rest) = line.strip_prefix(prefix)
            && rest.contains("=>")
        {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            return (!name.is_empty()).then_some(name);
        }
    }
    None
}

fn section_diff(section: &str, diff: &str) -> String {
    let section = section.to_ascii_lowercase();
    let mut result = String::new();
    let mut current_file = String::new();
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    for line in diff.lines() {
        if line.starts_with("diff --git") {
            current_file = line.to_ascii_lowercase();
        } else if line.starts_with("@@") {
            let mut pieces = line.split_whitespace();
            let _ = pieces.next();
            old_line = pieces
                .next()
                .and_then(|value| value.trim_start_matches('-').split(',').next())
                .and_then(|value| value.parse().ok())
                .unwrap_or(1);
            new_line = pieces
                .next()
                .and_then(|value| value.trim_start_matches('+').split(',').next())
                .and_then(|value| value.parse().ok())
                .unwrap_or(1);
        } else if let Some(text) = line.strip_prefix(' ') {
            let _ = text;
            old_line += 1;
            new_line += 1;
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let relevant = section == "overview"
            || section.contains("risk")
            || (section.contains("data")
                && [
                    "struct ",
                    "type ",
                    "interface ",
                    "schema",
                    "serde",
                    "json",
                    "api",
                    "config",
                    "request",
                    "response",
                ]
                .iter()
                .any(|term| lower.contains(term)))
            || (section == "functions"
                && ["fn ", "function ", "def ", "=>", "class ", "impl "]
                    .iter()
                    .any(|term| lower.contains(term)))
            || (section == "flow"
                && [
                    "if ", "else", "match ", "return", "await", "send", "handle", "call", "state",
                ]
                .iter()
                .any(|term| lower.contains(term)))
            || (section == "tests"
                && (current_file.contains("test")
                    || current_file.contains("spec")
                    || lower.contains("test")));
        if relevant || line.starts_with("diff --git") || line.starts_with("@@") {
            let rendered = if let Some(text) =
                line.strip_prefix('+').filter(|_| !line.starts_with("+++"))
            {
                format!("+{new_line}: {text}")
            } else if let Some(text) = line.strip_prefix('-').filter(|_| !line.starts_with("---")) {
                format!("-{old_line}: {text}")
            } else {
                line.to_string()
            };
            if result.len() + rendered.len() + 1 > 3_500 {
                result.push_str("\n[digest truncated]");
                break;
            }
            result.push_str(&rendered);
            result.push('\n');
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            new_line += 1;
        }
        if line.starts_with('-') && !line.starts_with("---") {
            old_line += 1;
        }
    }
    result
}

pub fn review_prompt(
    section: &str,
    instruction: &str,
    title: &str,
    body: &str,
    diff: &str,
) -> String {
    let clipped_diff = section_diff(section, diff);
    format!(
        r#"Write one high-signal section of a peer-review briefing in 90–140 words. Return readable Markdown using short bullets and `inline code`. Every factual bullet MUST end with a precise changed-code citation in backticks using `path:start-end` or `path:line` (for example `src/auth.rs:84-101`). Use `>` callouts for the most important reviewer warning. No introduction, conclusion, or repeated diff. Do not include the section title. Distinguish facts from inference, and do not modify or submit anything.

SECTION: {section}
FOCUS: {instruction}

PR TITLE:
{title}

PR DESCRIPTION:
{body}

CHANGED LINES AND HUNK CONTEXT:
{clipped_diff}"#
    )
}

pub fn generate_stream(
    section: &str,
    instruction: &str,
    title: &str,
    body: &str,
    diff: &str,
    sender: &Sender<BriefingEvent>,
) {
    let result = generate_stream_inner(section, instruction, title, body, diff, sender)
        .map_err(|error| error.to_string());
    let _ = sender.send(BriefingEvent::Complete(result));
}

fn generate_stream_inner(
    section: &str,
    instruction: &str,
    title: &str,
    body: &str,
    diff: &str,
    sender: &Sender<BriefingEvent>,
) -> Result<String> {
    let prompt = review_prompt(section, instruction, title, body, diff);
    let provider =
        std::env::var("REVIEWER_PI_PROVIDER").unwrap_or_else(|_| "openrouter".to_string());
    let model = std::env::var("REVIEWER_PI_MODEL")
        .unwrap_or_else(|_| "deepseek/deepseek-v4-flash".to_string());
    let mut child = Command::new("pi")
        .args([
            "--mode",
            "json",
            "--provider",
            &provider,
            "--model",
            &model,
            "--no-session",
            "--no-tools",
            "--no-skills",
            "--no-extensions",
            "--thinking",
            "off",
            &prompt,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start pi; install pi or ensure it is on PATH")?;
    let stdout = child.stdout.take().context("could not read pi output")?;
    let mut response = String::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.context("could not read pi event stream")?;
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            if !line.trim().is_empty() {
                let _ = sender.send(BriefingEvent::Delta(format!("{line}\n")));
            }
            continue;
        };
        let delta = event
            .get("assistantMessageEvent")
            .filter(|event| {
                event.get("type").and_then(|value| value.as_str()) == Some("text_delta")
            })
            .and_then(|event| event.get("delta"))
            .and_then(|delta| delta.as_str());
        if let Some(delta) = delta {
            response.push_str(delta);
            let _ = sender.send(BriefingEvent::Delta(delta.to_string()));
        }
    }
    let status = child.wait().context("could not wait for pi")?;
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        pipe.read_to_string(&mut stderr).ok();
    }
    if !status.success() {
        let message = if stderr.trim().is_empty() {
            "pi exited before completing the report; see the transcript above"
        } else {
            stderr.trim()
        };
        bail!("pi could not build the briefing: {message}");
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::{analyze_symbols, section_diff};

    #[test]
    fn detects_added_removed_and_modified_symbols() {
        let changes = analyze_symbols(
            "+++ b/src/api.rs\n-fn load() {}\n+fn load(id: u64) {}\n+pub struct ResultRow {}\n-def old_helper():\n",
        );
        assert!(
            changes
                .iter()
                .any(|s| s.name == "load" && s.kind == "modified")
        );
        assert!(
            changes
                .iter()
                .any(|s| s.name == "ResultRow" && s.kind == "added")
        );
        assert!(
            changes
                .iter()
                .any(|s| s.name == "old_helper" && s.kind == "removed")
        );
    }

    #[test]
    fn detects_rust_functions_with_visibility_and_async_modifiers() {
        let changes = analyze_symbols(
            "+++ b/src/api.rs\n+pub(crate) fn parse_request() {}\n+pub async fn fetch_user() {}\n",
        );

        assert!(
            changes
                .iter()
                .any(|symbol| symbol.name == "parse_request" && symbol.kind == "added")
        );
        assert!(
            changes
                .iter()
                .any(|symbol| symbol.name == "fetch_user" && symbol.kind == "added")
        );
    }

    #[test]
    fn compacts_diff_to_changed_lines_and_hunks() {
        let compact = section_diff(
            "Overview",
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n context\n-old\n+new\n",
        );
        assert!(!compact.contains(" context"));
        assert!(compact.contains("-2: old\n+2: new"));
    }
}
