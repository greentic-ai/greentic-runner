use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

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

    for entry in fs::read_dir(root).with_context(|| format!("listing {}", root.display()))? {
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

        let sha = compute_sha256(&path).ok();
        items.push(ToolInfo {
            name,
            path: path.clone(),
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

    let filename = format!("{expected}.wasm");
    let dest_path = cache_dir.join(filename);

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
