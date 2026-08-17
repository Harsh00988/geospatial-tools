mod decode;
mod profile;
pub(crate) mod source;
mod stream;

pub(crate) use decode::{Jp2Sample, Region};

use std::path::Path;

use anyhow::{bail, Result};
use geotiff_core::GeoKeyDirectory;

use crate::crop::WriteWindow;
use crate::geo::{extract_jp2_xml, parse_gmljp2, read_prj_epsg, read_world_file};
use crate::input::{projected_georef, GeorefProfile};
use crate::path::ConvertPath;
use crate::write::ConvertRequest;

pub use profile::Jp2Raster;
pub use source::{open_jp2_source, Jp2Source};

pub fn convert(pool: &rayon::ThreadPool, request: &ConvertRequest<'_>) -> Result<ConvertPath> {
    let (source, path) = open_jp2_source(request.input, request.mmap)?;
    convert_source(
        pool,
        &source,
        path.as_deref(),
        request.output,
        request.opts,
        request.window,
        request.bands.as_deref(),
        request.show_progress,
    )?;
    Ok(ConvertPath::StripEncode)
}

pub(crate) fn convert_source(
    pool: &rayon::ThreadPool,
    source: &Jp2Source,
    input_path: Option<&Path>,
    output: &std::path::Path,
    opts: &crate::cog::CogOutputOptions,
    window: Option<WriteWindow>,
    bands: Option<&[usize]>,
    show_progress: bool,
) -> Result<()> {
    stream::convert(
        pool,
        source,
        input_path,
        output,
        opts,
        window,
        bands,
        show_progress,
    )
}

pub fn resolve_georef(data: &[u8], path: Option<&Path>) -> Result<GeorefProfile> {
    if let Ok(xml) = extract_jp2_xml(data) {
        if let Ok((epsg, transform)) = parse_gmljp2(&xml) {
            return Ok(projected_georef(epsg, transform));
        }
    }

    if let Some(path) = path {
        if let Some(transform) = read_world_file(path)? {
            if let Some(epsg) = read_prj_epsg(path) {
                return Ok(projected_georef(epsg, transform));
            }
            return Ok(affine_only_georef(transform));
        }
    }

    bail!("JP2 lacks georeferencing: expected GML metadata or a sidecar world file (.j2w/.wld)")
}

fn affine_only_georef(transform: geotiff_core::GeoTransform) -> GeorefProfile {
    let geokeys = GeoKeyDirectory::new();
    GeorefProfile {
        crs: geotiff_core::CrsInfo::from_geokeys(&geokeys),
        geokeys,
        affine: Some(transform),
        transformation_matrix: None,
        model_tiepoints: None,
    }
}

pub fn header_from_bytes(data: &[u8]) -> Result<profile::Jp2Raster> {
    profile::Jp2Raster::open(data)
}
