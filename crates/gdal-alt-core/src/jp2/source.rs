use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use memmap2::Mmap;

use crate::open::is_http_source;
use crate::util::map_file;

/// Read-only JP2 payload (memory-mapped local file or in-memory bytes).
#[derive(Clone)]
pub enum Jp2Source {
    Mmap(Arc<Mmap>),
    Bytes(Arc<Vec<u8>>),
}

impl AsRef<[u8]> for Jp2Source {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Mmap(mmap) => mmap.as_ref(),
            Self::Bytes(bytes) => bytes.as_ref(),
        }
    }
}

pub fn open_jp2_source(input: &str, mmap: bool) -> Result<(Jp2Source, Option<PathBuf>)> {
    if is_http_source(input) {
        let bytes = download_http(input)?;
        return Ok((Jp2Source::Bytes(Arc::new(bytes)), None));
    }
    let path = PathBuf::from(input);
    let source = if mmap {
        Jp2Source::Mmap(map_file(&path)?)
    } else {
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Jp2Source::Bytes(Arc::new(bytes))
    };
    Ok((source, Some(path)))
}

fn download_http(url: &str) -> Result<Vec<u8>> {
    let response = reqwest::blocking::get(url)
        .with_context(|| format!("failed to download {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error downloading {url}"))?;
    response
        .bytes()
        .with_context(|| format!("failed to read response body from {url}"))
        .map(|bytes| bytes.to_vec())
}
