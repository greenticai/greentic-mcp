use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use crate::path_safety::normalize_under_root;

#[derive(Clone, Debug)]
pub enum ToolStore {
    /// Local directory populated with `.wasm` tool components.
    LocalDir(PathBuf),
    /// Single remote component downloaded and cached locally.
    HttpSingleFile {
        name: String,
        url: String,
        cache_dir: PathBuf,
    },
    // Additional registries (OCI/Warg) will be supported in future revisions.
}

#[derive(Clone, Debug)]
pub struct ToolInfo {
    pub name: String,
    pub path: PathBuf,
    pub sha256: Option<String>,
}

#[derive(Debug)]
pub struct ToolNotFound {
    name: String,
}

impl ToolNotFound {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl std::fmt::Display for ToolNotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tool `{}` not found", self.name)
    }
}

impl std::error::Error for ToolNotFound {}

pub fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ToolNotFound>().is_some()
}

impl ToolStore {
    pub fn list(&self) -> Result<Vec<ToolInfo>> {
        match self {
            ToolStore::LocalDir(root) => list_local(root),
            ToolStore::HttpSingleFile { name, .. } => {
                let info = self.fetch(name)?;
                Ok(vec![info])
            }
        }
    }

    pub fn fetch(&self, name: &str) -> Result<ToolInfo> {
        match self {
            ToolStore::LocalDir(root) => fetch_local(root, name),
            ToolStore::HttpSingleFile {
                name: expected,
                url,
                cache_dir,
            } => fetch_http(expected, url, cache_dir, name),
        }
    }
}

fn list_local(root: &Path) -> Result<Vec<ToolInfo>> {
    let mut items = Vec::new();
    if !root.exists() {
        return Ok(items);
    }

    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing tool store root {}", root.display()))?;
    let display_root = root.to_path_buf();

    for entry in fs::read_dir(&canonical_root)
        .with_context(|| format!("listing {}", canonical_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        if !matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some(ext) if ext.eq_ignore_ascii_case("wasm")
        ) {
            continue;
        }

        let Some(name) = path
            .file_stem()
            .and_then(|os| os.to_str())
            .map(|s| s.to_string())
        else {
            continue;
        };

        let relative = path.strip_prefix(&canonical_root).with_context(|| {
            format!(
                "entry {} not under {}",
                path.display(),
                canonical_root.display()
            )
        })?;
        let safe_path = normalize_under_root(&canonical_root, relative)?;
        let stable_path = display_root.join(relative);

        let sha = compute_sha256(&safe_path).ok();
        items.push(ToolInfo {
            name,
            path: stable_path,
            sha256: sha,
        });
    }

    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(items)
}

fn fetch_local(root: &Path, name: &str) -> Result<ToolInfo> {
    let tools = list_local(root)?;
    tools
        .into_iter()
        .find(|info| info.name == name)
        .ok_or_else(|| anyhow!(ToolNotFound::new(name)))
}

fn fetch_http(expected: &str, url: &str, cache_dir: &Path, name: &str) -> Result<ToolInfo> {
    if name != expected {
        return Err(anyhow!(ToolNotFound::new(name)));
    }

    fs::create_dir_all(cache_dir)
        .with_context(|| format!("creating cache dir {}", cache_dir.display()))?;

    let cache_dir = cache_dir
        .canonicalize()
        .with_context(|| format!("canonicalizing cache dir {}", cache_dir.display()))?;

    let filename = format!("{expected}.wasm");
    let dest_path = normalize_under_root(&cache_dir, Path::new(&filename))?;

    if !dest_path.exists() {
        download_with_retry(url, &dest_path)?;
    }

    let sha = compute_sha256(&dest_path).ok();
    Ok(ToolInfo {
        name: expected.to_string(),
        path: dest_path,
        sha256: sha,
    })
}

fn compute_sha256(path: &Path) -> Result<String> {
    use std::io::Read;

    let mut hasher = Sha256::new();
    let mut file = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = [0u8; 8192];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn download_with_retry(url: &str, dest: &Path) -> Result<()> {
    use std::thread::sleep;

    let client = reqwest::blocking::Client::builder()
        .use_rustls_tls()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    let mut last_err = None;
    for attempt in 1..=3 {
        match download_once(&client, url, dest) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                let backoff = Duration::from_secs(attempt * 2);
                sleep(backoff);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("download failed without specific error")))
}

fn download_once(client: &reqwest::blocking::Client, url: &str, dest: &Path) -> Result<()> {
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("requesting {}", url))?
        .error_for_status()
        .with_context(|| format!("non-success status from {}", url))?;

    let bytes = response
        .bytes()
        .with_context(|| format!("reading bytes from {}", url))?;

    let tmp = dest.with_extension("download");
    fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, dest).with_context(|| format!("moving into {}", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn list_dir_without_root_returns_empty() {
        let root = tempdir().expect("tmp root");
        let store = ToolStore::LocalDir(root.path().into());
        let tools = store.list().expect("list");
        assert!(tools.is_empty());
    }

    #[test]
    fn list_local_finds_wasm_files_sorted() {
        let root = tempdir().expect("tmp root");
        fs::write(root.path().join("z.wasm"), b"z").unwrap();
        fs::write(root.path().join("a.wasm"), b"a").unwrap();
        fs::write(root.path().join("ignore.txt"), b"x").unwrap();

        let store = ToolStore::LocalDir(root.path().into());
        let tools = store.list().expect("list");
        let names: Vec<_> = tools.iter().map(|t| &t.name).collect();
        assert_eq!(names, vec!["a", "z"]);
    }

    #[test]
    fn fetch_local_returns_matching_tool() {
        let root = tempdir().expect("tmp root");
        let wasm_path = root.path().join("tool.wasm");
        fs::write(&wasm_path, b"component").unwrap();

        let store = ToolStore::LocalDir(root.path().into());
        let info = store.fetch("tool").expect("fetch");
        assert_eq!(info.name, "tool");
        assert_eq!(info.path, wasm_path);
        assert!(info.sha256.is_some());
    }

    #[test]
    fn fetch_local_reports_missing_tool() {
        let root = tempdir().expect("tmp root");
        let store = ToolStore::LocalDir(root.path().into());
        let err = store.fetch("missing").expect_err("should miss");
        assert!(is_not_found(&err));
    }

    #[test]
    fn fetch_http_rejects_wrong_name() {
        let root = tempdir().expect("cache root");
        let err = fetch_http(
            "expected",
            "https://example.com/x.wasm",
            root.path(),
            "other",
        )
        .expect_err("wrong tool should fail");
        assert!(is_not_found(&err));
    }

    #[test]
    fn fetch_http_uses_cached_file_if_present() {
        let root = tempdir().expect("cache root");
        let store = ToolStore::HttpSingleFile {
            name: "expected".into(),
            url: "https://example.com/x.wasm".into(),
            cache_dir: root.path().into(),
        };

        let cached = root.path().join("expected.wasm");
        fs::write(&cached, b"cached fixture").unwrap();
        let info = store.fetch("expected").expect("fetch");
        assert_eq!(info.path, cached);
        assert!(info.sha256.is_some());
    }

    #[test]
    fn compute_sha256_stable_for_fixture() {
        let root = tempdir().expect("tmp root");
        let file = root.path().join("one.wasm");
        fs::write(&file, b"abc").unwrap();
        assert_eq!(
            compute_sha256(&file).expect("digest"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string()
        );
    }

    #[test]
    fn resolve_tool_is_consistent_with_local_store_listing() {
        let root = tempdir().expect("tmp root");
        let wasm = root.path().join("roundtrip.wasm");
        fs::write(&wasm, b"wasm").unwrap();

        let info = resolve::resolve("roundtrip", &ToolStore::LocalDir(root.path().into()))
            .expect("resolve");
        assert_eq!(info.info.name, "roundtrip");
        assert_eq!(info.info.path, wasm);
        assert_eq!(info.bytes, std::fs::read(&wasm).unwrap().into());
        assert!(!info.digest.is_empty());
    }
}
