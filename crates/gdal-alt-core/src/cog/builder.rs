use geotiff_writer::cog::CogBuilder;
use geotiff_writer::GeoTiffBuilder;

use super::grid::auto_overview_levels;
use super::options::{CogOutputOptions, CompressionChoice};

pub fn overview_levels(opts: &CogOutputOptions, width: u32, height: u32) -> Vec<u32> {
    opts.effective_overview_levels(width, height)
}

pub fn apply_compression(builder: GeoTiffBuilder, opts: &CogOutputOptions) -> GeoTiffBuilder {
    let mut builder = builder.compression(opts.compression.to_compression());
    if matches!(opts.compression, CompressionChoice::Deflate) {
        builder = builder.deflate_level(opts.deflate_level);
    }
    builder
}

pub fn configure_cog(
    base: GeoTiffBuilder,
    opts: &CogOutputOptions,
    width: u32,
    height: u32,
) -> CogBuilder {
    let mut cog = CogBuilder::new(base)
        .resampling(opts.resampling.to_resampling())
        .subifd_overviews();

    if opts.no_overviews {
        cog = cog.no_overviews();
    } else {
        let levels = opts
            .overview_levels
            .clone()
            .unwrap_or_else(|| auto_overview_levels(width, height, opts.blocksize));
        cog = cog.overview_levels(levels);
    }

    cog
}

pub fn configure_cog_with_levels(
    base: GeoTiffBuilder,
    opts: &CogOutputOptions,
    levels: Vec<u32>,
) -> CogBuilder {
    CogBuilder::new(base)
        .resampling(opts.resampling.to_resampling())
        .subifd_overviews()
        .overview_levels(levels)
}

pub use super::grid::overview_sizes as layer_sizes;
