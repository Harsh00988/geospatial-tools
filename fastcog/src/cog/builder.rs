use crate::config::{Args, CompressionChoice};
use geotiff_writer::cog::CogBuilder;
use geotiff_writer::GeoTiffBuilder;

use super::grid::auto_overview_levels;

pub fn overview_levels(args: &Args, width: u32, height: u32) -> Vec<u32> {
    if args.no_overviews {
        return Vec::new();
    }
    args.overviews
        .clone()
        .unwrap_or_else(|| auto_overview_levels(width, height, args.blocksize))
}

pub fn apply_compression(builder: GeoTiffBuilder, args: &Args) -> GeoTiffBuilder {
    let mut builder = builder.compression(args.compress.to_compression());
    if matches!(args.compress, CompressionChoice::Deflate) {
        builder = builder.deflate_level(args.deflate_level);
    }
    builder
}

pub fn configure_cog(base: GeoTiffBuilder, args: &Args, width: u32, height: u32) -> CogBuilder {
    let mut cog = CogBuilder::new(base)
        .resampling(args.resampling.to_resampling())
        .subifd_overviews();

    if args.no_overviews {
        cog = cog.no_overviews();
    } else {
        let levels = overview_levels(args, width, height);
        cog = cog.overview_levels(levels);
    }

    cog
}

pub fn configure_cog_with_levels(
    base: GeoTiffBuilder,
    args: &Args,
    levels: Vec<u32>,
) -> CogBuilder {
    CogBuilder::new(base)
        .resampling(args.resampling.to_resampling())
        .subifd_overviews()
        .overview_levels(levels)
}

pub use super::grid::overview_sizes as layer_sizes;
