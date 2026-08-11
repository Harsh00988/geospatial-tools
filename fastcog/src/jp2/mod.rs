mod decode;
mod profile;
mod stream;

use crate::config::Args;
use crate::geo;
use crate::input::projected_georef;
use crate::util;
use anyhow::{Context, Result};

pub fn convert(args: &Args, pool: &rayon::ThreadPool) -> Result<()> {
    let mmap = util::map_file(&args.input)?;
    let raster = profile::Jp2Raster::open(mmap.as_ref())?;
    let xml = geo::extract_jp2_xml(mmap.as_ref()).context("failed to read JP2 GML metadata")?;
    let (epsg, transform) =
        geo::parse_gmljp2(&xml).context("failed to parse JP2 georeferencing")?;
    let georef = projected_georef(epsg, transform);
    stream::convert(args, pool, mmap, raster, georef)
}
