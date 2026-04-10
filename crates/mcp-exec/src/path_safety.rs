use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Normalize a user-supplied path and ensure it stays within an allowed root.
/// Reject absolute paths and any that escape via `..`.
pub fn normalize_under_root(root: &Path, candidate: &Path) -> Result<PathBuf> {
    if candidate.is_absolute() {
        anyhow::bail!("absolute paths are not allowed: {}", candidate.display());
    }

    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize root {}", root.display()))?;

    let joined = root.join(candidate);
    let canon = match joined.canonicalize() {
        Ok(path) => path,
        Err(_err) if !joined.exists() => {
            let parent = joined
                .parent()
                .context("path has no parent to normalize")?
                .canonicalize()
                .with_context(|| format!("failed to canonicalize {}", joined.display()))?;

            parent.join(joined.file_name().context("path missing final component")?)
        }
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to canonicalize {}", joined.display()));
        }
    };

    if !canon.starts_with(&root) {
        anyhow::bail!(
            "path escapes root ({}): {}",
            root.display(),
            canon.display()
        );
    }

    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn normalizes_existing_relative_path() {
        let root = tempdir().expect("tmp root");
        let nested = root.path().join("tools");
        fs::create_dir_all(&nested).expect("mkdir");
        let file = nested.join("example.wasm");
        fs::write(&file, b"wasm").expect("write");

        let normalized =
            normalize_under_root(root.path(), Path::new("tools/example.wasm")).expect("normalize");
        assert_eq!(
            normalized.file_name(),
            Some(std::ffi::OsStr::new("example.wasm"))
        );
        assert!(normalized.starts_with(root.path()));
    }

    #[test]
    fn rejects_absolute_paths() {
        let root = tempdir().expect("tmp root");
        let err = normalize_under_root(root.path(), Path::new("/etc/passwd"))
            .expect_err("absolute path should fail");
        let text = err.to_string();
        assert!(text.contains("absolute paths are not allowed"));
    }

    #[test]
    fn rejects_paths_that_escape_root() {
        let root = tempdir().expect("tmp root");
        let err = normalize_under_root(root.path(), Path::new("../outside"))
            .expect_err("escape should fail");
        assert!(err.to_string().contains("path escapes root"));
    }

    #[test]
    fn accepts_missing_leaf_under_root() {
        let root = tempdir().expect("tmp root");
        let nested = root.path().join("tools");
        fs::create_dir_all(&nested).expect("mkdir");

        let normalized = normalize_under_root(root.path(), Path::new("tools/missing.wasm"))
            .expect("normalize missing leaf");
        assert!(normalized.starts_with(root.path()));
        assert!(normalized.ends_with("tools/missing.wasm"));
    }
}
