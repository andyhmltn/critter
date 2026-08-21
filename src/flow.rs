use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FlowPlan {
    pub steps: Vec<FlowStep>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FlowStep {
    pub title: String,
    pub rationale: String,
    pub locations: Vec<FlowLocation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FlowLocation {
    pub path: String,
    pub line: u32,
    pub reason: String,
}

pub enum FlowEvent {
    Status(String),
    Complete(Result<FlowPlan, String>),
}

pub fn load(repo: &str, pr: &str, head: &str) -> Result<Option<FlowPlan>> {
    let path = cache_path(repo, pr, head);
    match fs::read(&path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .map(Some)
            .with_context(|| format!("could not parse {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

pub fn generate_and_cache(
    repo: String,
    pr: String,
    head: String,
    title: String,
    diff: String,
    sender: Sender<FlowEvent>,
) {
    let result = generate(&title, &diff, &sender)
        .and_then(|plan| {
            save(&repo, &pr, &head, &plan)?;
            Ok(plan)
        })
        .map_err(|error| error.to_string());
    let _ = sender.send(FlowEvent::Complete(result));
}

fn generate(title: &str, diff: &str, sender: &Sender<FlowEvent>) -> Result<FlowPlan> {
    let _ = sender.send(FlowEvent::Status("Preparing changed-code map".to_string()));
    let manifest = change_manifest(diff);
    let prompt = format!(
        r#"Build a logical code-review flow for this pull request. Group changed locations into the order an experienced engineering manager should review them: contracts and data first, then core behavior, integrations, user-facing behavior, and tests/operations as applicable.

Return ONLY valid JSON with this exact shape:
{{"steps":[{{"title":"short stage name","rationale":"two or three useful sentences explaining dependencies, reviewer intent, and concrete questions to verify","locations":[{{"path":"exact changed path","line":123,"reason":"what the reviewer should establish at this location"}}]}}]}}

Rules:
- 2 to 6 steps, ordered by dependency and logical flow rather than diff order.
- Every location must reference an exact changed file and line from the diff; use the new-side line where available and the old-side line for deletions.
- Include all materially changed files. Do not invent paths or line numbers.
- Make each rationale specific enough to guide a senior reviewer: explain why this stage comes now, what contract or behavior it establishes, and what failure mode to look for.
- Keep location reasons concise. No Markdown fences or commentary.

PR: {title}

CHANGED-CODE MANIFEST:
{manifest}"#
    );
    let provider = std::env::var("REVIEWER_PI_PROVIDER").unwrap_or_else(|_| "openrouter".into());
    let model =
        std::env::var("REVIEWER_PI_MODEL").unwrap_or_else(|_| "deepseek/deepseek-v4-flash".into());
    let _ = sender.send(FlowEvent::Status(format!(
        "Connecting to {provider} · {model}"
    )));
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
    let mut announced = false;
    for line in BufReader::new(stdout).lines() {
        let line = line.context("could not read pi event stream")?;
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(delta) = event
            .get("assistantMessageEvent")
            .filter(|event| {
                event.get("type").and_then(|value| value.as_str()) == Some("text_delta")
            })
            .and_then(|event| event.get("delta"))
            .and_then(|delta| delta.as_str())
        {
            if !announced {
                let _ = sender.send(FlowEvent::Status(
                    "Ordering dependencies and review stages".to_string(),
                ));
                announced = true;
            }
            response.push_str(delta);
        }
    }
    let output = child.wait_with_output().context("could not wait for pi")?;
    if !output.status.success() {
        bail!(
            "pi could not build the review flow: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let _ = sender.send(FlowEvent::Status(
        "Validating changed locations".to_string(),
    ));
    let json = response
        .trim()
        .strip_prefix("```json")
        .or_else(|| response.trim().strip_prefix("```"))
        .unwrap_or(response.trim())
        .strip_suffix("```")
        .unwrap_or(response.trim())
        .trim();
    let plan: FlowPlan = serde_json::from_str(json).context("pi returned an invalid flow plan")?;
    if plan.steps.is_empty() {
        bail!("pi returned an empty flow plan");
    }
    Ok(plan)
}

fn change_manifest(diff: &str) -> String {
    let mut files: Vec<(String, Vec<String>)> = Vec::new();
    let mut current = None;
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    for raw in diff.lines() {
        if let Some(path) = raw
            .strip_prefix("diff --git a/")
            .and_then(|value| value.split(" b/").nth(1))
        {
            files.push((path.to_string(), Vec::new()));
            current = Some(files.len() - 1);
            continue;
        }
        if raw.starts_with("@@") {
            let mut pieces = raw.split_whitespace();
            let _ = pieces.next();
            old_line = pieces.next().and_then(hunk_line).unwrap_or(1);
            new_line = pieces.next().and_then(hunk_line).unwrap_or(1);
            continue;
        }
        let Some(index) = current else { continue };
        if let Some(text) = raw.strip_prefix('+').filter(|_| !raw.starts_with("+++")) {
            if files[index].1.len() < 24 {
                files[index].1.push(format!(
                    "+{new_line}: {}",
                    text.trim().chars().take(120).collect::<String>()
                ));
            }
            new_line += 1;
        } else if let Some(text) = raw.strip_prefix('-').filter(|_| !raw.starts_with("---")) {
            if files[index].1.len() < 24 {
                files[index].1.push(format!(
                    "-{old_line}: {}",
                    text.trim().chars().take(120).collect::<String>()
                ));
            }
            old_line += 1;
        } else if raw.starts_with(' ') {
            old_line += 1;
            new_line += 1;
        }
    }
    files
        .into_iter()
        .map(|(path, lines)| format!("FILE {path}\n{}", lines.join("\n")))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn hunk_line(value: &str) -> Option<u32> {
    value
        .trim_start_matches(['-', '+'])
        .split(',')
        .next()?
        .parse()
        .ok()
}

fn save(repo: &str, pr: &str, head: &str, plan: &FlowPlan) -> Result<()> {
    let path = cache_path(repo, pr, head);
    let parent = path.parent().context("flow cache path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    fs::write(&path, serde_json::to_vec_pretty(plan)?)
        .with_context(|| format!("could not save {}", path.display()))
}

fn cache_path(repo: &str, pr: &str, head: &str) -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let repo = repo.replace(['/', '\\'], "-");
    cache
        .join("reviewer")
        .join("flow")
        .join(format!("{repo}-{pr}-{head}-v2.json"))
}

#[cfg(test)]
mod tests {
    use super::cache_path;

    #[test]
    fn flow_cache_is_scoped_to_the_pr_head() {
        let path = cache_path("acme/widgets", "42", "abc123");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("acme-widgets-42-abc123-v2.json")
        );
    }
}
