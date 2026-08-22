use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::PendingComment;

#[derive(Default, Deserialize, Serialize)]
struct ProgressFile {
    head_oid: String,
    viewed_files: BTreeSet<String>,
}

#[derive(Deserialize, Serialize)]
struct PickerSearchFile {
    query: String,
}

pub fn load_picker_query(repo: &str) -> Result<Option<String>> {
    let path = picker_search_path(repo);
    match fs::read(&path) {
        Ok(contents) => {
            let saved = serde_json::from_slice::<PickerSearchFile>(&contents)
                .with_context(|| format!("could not parse {}", path.display()))?;
            Ok((!saved.query.trim().is_empty()).then_some(saved.query))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("could not read {}", path.display())),
    }
}

pub fn save_picker_query(repo: &str, query: &str) -> Result<()> {
    let path = picker_search_path(repo);
    let parent = path.parent().context("picker search path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let contents = serde_json::to_vec_pretty(&PickerSearchFile {
        query: query.to_string(),
    })?;
    fs::write(&path, contents).with_context(|| format!("could not save {}", path.display()))
}

pub struct ReviewProgress {
    path: PathBuf,
    head_oid: String,
    viewed_files: BTreeSet<String>,
}

/// A crash-safe, head-specific store for comments that have not reached GitHub yet.
pub struct ReviewDraft {
    path: PathBuf,
    comments: Vec<PendingComment>,
}

impl ReviewDraft {
    pub fn load(repo: &str, pr_number: &str, head_oid: &str) -> Result<Self> {
        Self::load_from(draft_path(repo, pr_number, head_oid))
    }

    fn load_from(path: PathBuf) -> Result<Self> {
        let comments = match fs::read(&path) {
            Ok(contents) => serde_json::from_slice(&contents)
                .with_context(|| format!("could not parse {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        };
        Ok(Self { path, comments })
    }

    pub fn comments(&self) -> &[PendingComment] {
        &self.comments
    }

    pub fn save(&mut self, comments: &[PendingComment]) -> Result<()> {
        if comments.is_empty() {
            self.clear()?;
            return Ok(());
        }
        let parent = self.path.parent().context("draft path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("could not create draft in {}", parent.display()))?;
        temporary.write_all(&serde_json::to_vec_pretty(comments)?)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&self.path)
            .map_err(|error| error.error)
            .with_context(|| format!("could not replace {}", self.path.display()))?;
        self.comments = comments.to_vec();
        Ok(())
    }

    pub fn clear(&mut self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("could not remove {}", self.path.display()));
            }
        }
        self.comments.clear();
        Ok(())
    }
}

impl ReviewProgress {
    pub fn load(repo: &str, pr_number: &str, head_oid: &str) -> Result<Self> {
        let path = progress_path(repo, pr_number);
        let saved = match fs::read(&path) {
            Ok(contents) => serde_json::from_slice::<ProgressFile>(&contents)
                .with_context(|| format!("could not parse {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ProgressFile::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        };
        let viewed_files = if saved.head_oid == head_oid {
            saved.viewed_files
        } else {
            BTreeSet::new()
        };
        Ok(Self {
            path,
            head_oid: head_oid.to_string(),
            viewed_files,
        })
    }

    pub fn is_viewed(&self, path: &str) -> bool {
        self.viewed_files.contains(path)
    }

    pub fn viewed_count(&self) -> usize {
        self.viewed_files.len()
    }

    pub fn toggle(&mut self, file: &str) -> Result<()> {
        if !self.viewed_files.remove(file) {
            self.viewed_files.insert(file.to_string());
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        let parent = self.path.parent().context("progress path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        let progress = ProgressFile {
            head_oid: self.head_oid.clone(),
            viewed_files: self.viewed_files.clone(),
        };
        let contents = serde_json::to_vec_pretty(&progress)?;
        fs::write(&self.path, contents)
            .with_context(|| format!("could not save {}", self.path.display()))
    }
}

fn progress_path(repo: &str, pr_number: &str) -> PathBuf {
    cache_root()
        .join("progress")
        .join(format!("{}-{pr_number}.json", safe_repo_name(repo)))
}

fn picker_search_path(repo: &str) -> PathBuf {
    cache_root()
        .join("search")
        .join(format!("{}.json", safe_repo_name(repo)))
}

fn draft_path(repo: &str, pr_number: &str, head_oid: &str) -> PathBuf {
    cache_root().join("drafts").join(format!(
        "{}-{pr_number}-{}.json",
        safe_repo_name(repo),
        safe_component(head_oid)
    ))
}

fn cache_root() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    cache.join("reviewer")
}

fn safe_repo_name(repo: &str) -> String {
    repo.replace(['/', '\\'], "-")
}

fn safe_component(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::{ReviewDraft, draft_path, picker_search_path, progress_path};
    use crate::{PendingComment, Side};

    #[test]
    fn progress_filename_is_safe_for_owner_repo_names() {
        assert_eq!(
            progress_path("acme/widgets", "42")
                .file_name()
                .and_then(|name| name.to_str()),
            Some("acme-widgets-42.json")
        );
    }

    #[test]
    fn picker_search_filename_is_scoped_to_a_safe_repo_name() {
        assert_eq!(
            picker_search_path("acme/widgets")
                .file_name()
                .and_then(|name| name.to_str()),
            Some("acme-widgets.json")
        );
    }

    #[test]
    fn draft_round_trips_atomically_and_clears_when_empty() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/draft.json");
        let comment = PendingComment {
            path: "src/lib.rs".to_string(),
            line: 17,
            side: Side::Right,
            body: "Handle the error here".to_string(),
        };
        let mut draft = ReviewDraft::load_from(path.clone()).unwrap();
        draft.save(std::slice::from_ref(&comment)).unwrap();

        let restored = ReviewDraft::load_from(path.clone()).unwrap();
        assert_eq!(restored.comments(), &[comment]);

        draft.save(&[]).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn draft_filename_is_scoped_to_the_exact_head() {
        let first = draft_path("acme/widgets", "42", "head/one");
        let second = draft_path("acme/widgets", "42", "head-two");
        assert_ne!(first, second);
        assert_eq!(
            first.file_name().and_then(|name| name.to_str()),
            Some("acme-widgets-42-head-one.json")
        );
    }
}
