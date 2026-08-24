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
    pub summary: String,
    pub why_now: String,
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

pub fn generate_peer_review(title: String, body: String, diff: String, sender: Sender<FlowEvent>) {
    let result = generate_peer_review_inner(&title, &body, &diff, &sender)
        .map_err(|error| format!("{error:#}"));
    let _ = sender.send(FlowEvent::Complete(result));
}

fn generate_peer_review_inner(
    title: &str,
    body: &str,
    diff: &str,
    sender: &Sender<FlowEvent>,
) -> Result<FlowPlan> {
    let _ = sender.send(FlowEvent::Status(
        "Reading the PR description and complete diff".to_string(),
    ));
    let clipped_diff: String = diff.chars().take(180_000).collect();
    let prompt = format!(
        r#"Perform a deep peer review of this pull request for an experienced engineering manager. Read the PR description and the complete diff together. Build a logical review flow that explains intent, architectural context, contracts, behavior, failure modes, rollout/compatibility concerns, and test adequacy. Do not merely summarize files.

Return ONLY valid JSON with this exact shape:
{{"steps":[{{"title":"plain-English review stage","summary":"what the PR changes here, why it matters, and relevant wider-PR context in 45-90 words","why_now":"why this stage belongs here in the dependency/review sequence, 15-35 words","locations":[{{"path":"exact changed path","line":123,"reason":"a specific reviewer question or failure condition, 10-25 words"}}]}}]}}

Rules:
- Produce 3 to 8 stages ordered by logical dependency, not diff order.
- Cover every materially changed file, grouping related locations into coherent stages.
- Use exact changed paths and line numbers; new-side lines where available, old-side lines for deletions.
- Explain interactions across files and relate claims to the PR's stated intent.
- Use direct, understandable language. Make every location reason actionable.
- Distinguish evidence from a concern that needs checking. Do not claim a bug without evidence.
- No Markdown fences or text outside the JSON.

PR TITLE:
{title}

PR DESCRIPTION:
{body}

FULL DIFF:
{clipped_diff}"#
    );
    let model = std::env::var("REVIEWER_PEER_MODEL")
        .unwrap_or_else(|_| "openai/gpt-5.6-sol:nitro".to_string());
    let thinking = std::env::var("REVIEWER_PEER_THINKING").unwrap_or_else(|_| "low".to_string());
    run_pi(&prompt, &model, &thinking, sender)
}

fn run_pi(
    prompt: &str,
    model: &str,
    thinking: &str,
    sender: &Sender<FlowEvent>,
) -> Result<FlowPlan> {
    let provider = std::env::var("REVIEWER_PI_PROVIDER").unwrap_or_else(|_| "openrouter".into());
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
            model,
            "--no-session",
            "--no-tools",
            "--no-skills",
            "--no-extensions",
            "--thinking",
            thinking,
            prompt,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start pi; install pi or ensure it is on PATH")?;
    let stdout = child.stdout.take().context("could not read pi output")?;
    let mut response = String::new();
    let mut announced = false;
    let mut next_progress_report = 2_000usize;
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
            if response.len() >= next_progress_report {
                let _ = sender.send(FlowEvent::Status(format!(
                    "Drafting review · {} characters received",
                    response.len()
                )));
                next_progress_report = response.len().saturating_add(2_000);
            }
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
    let plan = parse_flow_plan(&response)?;
    if plan.steps.is_empty() {
        bail!("pi returned an empty flow plan");
    }
    Ok(plan)
}

fn parse_flow_plan(response: &str) -> Result<FlowPlan> {
    let trimmed = response.trim();
    let unfenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .strip_suffix("```")
        .unwrap_or_else(|| {
            trimmed
                .strip_prefix("```json")
                .or_else(|| trimmed.strip_prefix("```"))
                .unwrap_or(trimmed)
        })
        .trim();
    let object = response
        .find('{')
        .zip(response.rfind('}'))
        .filter(|(start, end)| start < end)
        .map(|(start, end)| &response[start..=end]);
    let mut last_error = None;
    for candidate in [trimmed, unfenced].into_iter().chain(object) {
        match serde_json::from_str::<FlowPlan>(candidate) {
            Ok(plan) => return Ok(plan),
            Err(error) => last_error = Some(error),
        }
    }
    let excerpt: String = response.chars().take(4_000).collect();
    bail!(
        "pi returned an invalid flow plan: {}; response excerpt:\n{}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "empty response".to_string()),
        if excerpt.is_empty() {
            "<empty>"
        } else {
            &excerpt
        }
    )
}

pub fn save_plan(repo: &str, pr: &str, head: &str, plan: &FlowPlan) -> Result<()> {
    let path = cache_path(repo, pr, head);
    let parent = path.parent().context("flow cache path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    fs::write(&path, serde_json::to_vec_pretty(plan)?)
        .with_context(|| format!("could not save {}", path.display()))
}

pub fn cache_path(repo: &str, pr: &str, head: &str) -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let repo = repo.replace(['/', '\\'], "-");
    cache
        .join("reviewer")
        .join("flow")
        .join(format!("{repo}-{pr}-{head}-v4.json"))
}

#[cfg(test)]
mod tests {
    use super::{cache_path, parse_flow_plan};

    #[test]
    fn flow_cache_is_scoped_to_the_pr_head() {
        let path = cache_path("acme/widgets", "42", "abc123");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("acme-widgets-42-abc123-v4.json")
        );
    }

    #[test]
    fn parses_fenced_or_prefixed_flow_json() {
        let json = r#"{"steps":[{"title":"Start","summary":"What changed","why_now":"Read first","locations":[{"path":"src/main.rs","line":4,"reason":"Check this"}]}]}"#;
        assert_eq!(
            parse_flow_plan(&format!("```json\n{json}\n```"))
                .unwrap()
                .steps
                .len(),
            1
        );
        assert_eq!(
            parse_flow_plan(&format!("Here is the plan:\n{json}\nDone."))
                .unwrap()
                .steps[0]
                .title,
            "Start"
        );
    }
}
