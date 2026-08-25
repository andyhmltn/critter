use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("cache path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;

    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("could not create a temporary file in {}", parent.display()))?;
    temporary
        .write_all(contents)
        .with_context(|| format!("could not write temporary cache for {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("could not flush temporary cache for {}", path.display()))?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("could not replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_atomic;

    #[test]
    fn atomic_write_creates_parents_and_replaces_existing_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/cache.json");

        write_atomic(&path, br#"{"version":1}"#).unwrap();
        write_atomic(&path, br#"{"version":2,"complete":true}"#).unwrap();

        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            r#"{"version":2,"complete":true}"#
        );
    }
}
