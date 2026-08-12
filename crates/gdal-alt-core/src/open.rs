use std::path::Path;

use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;

pub fn open_geotiff(path: &Path, mmap: bool) -> Result<GeoTiffFile> {
    if mmap {
        unsafe { GeoTiffFile::open_mmap(path) }
    } else {
        GeoTiffFile::open(path)
    }
    .with_context(|| format!("failed to open {}", path.display()))
}
