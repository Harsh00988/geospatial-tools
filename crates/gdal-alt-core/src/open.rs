use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use geotiff_reader::cog::HttpGeoTiffFile;
use geotiff_reader::GeoTiffFile;

/// Keeps remote HTTP range sources alive while exposing decoded GeoTIFF metadata.
pub enum GeoTiffHandle {
    Local(GeoTiffFile),
    Remote(HttpGeoTiffFile),
}

impl GeoTiffHandle {
    pub fn as_file(&self) -> &GeoTiffFile {
        match self {
            Self::Local(file) => file,
            Self::Remote(file) => file.inner(),
        }
    }
}

pub fn is_http_source(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

pub fn open_input(source: &str, mmap: bool) -> Result<GeoTiffHandle> {
    if is_http_source(source) {
        let remote = HttpGeoTiffFile::open(source)
            .with_context(|| format!("failed to open remote GeoTIFF {source}"))?;
        return Ok(GeoTiffHandle::Remote(remote));
    }
    let path = PathBuf::from(source);
    open_geotiff(&path, mmap).map(GeoTiffHandle::Local)
}

pub fn open_geotiff(path: &Path, mmap: bool) -> Result<GeoTiffFile> {
    if mmap {
        unsafe { GeoTiffFile::open_mmap(path) }
    } else {
        GeoTiffFile::open(path)
    }
    .with_context(|| format!("failed to open {}", path.display()))
}
