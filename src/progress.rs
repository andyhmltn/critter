use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache::write_atomic;

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
    let contents = serde_json::to_vec_pretty(&PickerSearchFile {
        query: query.to_string(),
    })?;
    // Keep the previous valid query if Reviewer is interrupted while updating the cache.
    write_atomic(&path, &contents).with_context(|| format!("could not save {}", path.display()))
}

pub struct ReviewProgress {
    path: PathBuf,
    head_oid: String,
    viewed_files: BTreeSet<String>,
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
        let progress = ProgressFile {
            head_oid: self.head_oid.clone(),
            viewed_files: self.viewed_files.clone(),
        };
        let contents = serde_json::to_vec_pretty(&progress)?;
        write_atomic(&self.path, &contents)
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

#[cfg(test)]
mod tests {
    use super::{picker_search_path, progress_path};

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
}
