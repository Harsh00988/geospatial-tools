use std::path::Path;

use anyhow::Result;

use crate::input::{detect_source, InputFormat};
use crate::jp2::{Jp2Raster, open_jp2_source};
use crate::open::open_input;

/// Width, height, and band count for any supported raster input.
pub fn input_dimensions(source: &str, mmap: bool) -> Result<(u32, u32, u32)> {
    match detect_source(source) {
        InputFormat::GeoTiff => {
            let handle = open_input(source, mmap)?;
            let file = handle.as_file();
            Ok((file.width(), file.height(), file.band_count()))
        }
        InputFormat::Jp2 => {
            let (jp2_source, _) = open_jp2_source(source, mmap)?;
            let raster = Jp2Raster::open(jp2_source.as_ref())?;
            Ok((raster.width, raster.height, raster.bands))
        }
    }
}

pub fn is_jp2_path(path: &Path) -> bool {
    matches!(
        detect_source(&path.to_string_lossy()),
        InputFormat::Jp2
    )
}
