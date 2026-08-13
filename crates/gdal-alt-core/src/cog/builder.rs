use geotiff_writer::cog::{CogBuilder, OverviewStorage, RemuxTileEncoding};
use geotiff_writer::GeoTiffBuilder;
use tiff_core::Predictor;
use tiff_core::SampleFormat;

use super::grid::auto_overview_levels;
use super::options::{CogOutputOptions, CompressionChoice};

pub fn overview_levels(opts: &CogOutputOptions, width: u32, height: u32) -> Vec<u32> {
    opts.effective_overview_levels(width, height)
}

pub fn apply_compression(
    builder: GeoTiffBuilder,
    opts: &CogOutputOptions,
    sample_format: SampleFormat,
) -> GeoTiffBuilder {
    let mut builder = builder.compression(opts.compression.to_compression());
    let predictor = opts.encode_predictor_for(sample_format);
    match opts.compression {
        CompressionChoice::Deflate | CompressionChoice::Zstd => {
            builder = builder.deflate_level(opts.deflate_level);
            builder = builder.predictor(predictor);
        }
        CompressionChoice::Lerc => {
            builder = builder.lerc_options(opts.lerc_options());
        }
        CompressionChoice::Jpeg => {
            builder = builder.jpeg_options(opts.jpeg_options());
        }
        CompressionChoice::Lzw => {
            builder = builder.predictor(predictor);
        }
        CompressionChoice::None => {}
    }
    builder
}

/// Build tile compression parameters for remux/crop/encode hot paths.
pub fn tile_encoding_from_opts(
    opts: &CogOutputOptions,
    tile_size: usize,
    samples_per_pixel: u16,
    predictor: Option<Predictor>,
    sample_format: SampleFormat,
) -> RemuxTileEncoding {
    let (lerc_options, jpeg_options) = match opts.compression {
        CompressionChoice::Lerc => (Some(opts.lerc_options()), None),
        CompressionChoice::Jpeg => (None, Some(opts.jpeg_options())),
        _ => (None, None),
    };
    RemuxTileEncoding {
        compression: opts.compression.to_compression(),
        predictor: predictor
            .unwrap_or_else(|| opts.encode_predictor_for(sample_format)),
        samples_per_pixel,
        tile_width: tile_size,
        tile_height: tile_size as u32,
        deflate_level: opts.deflate_level,
        lerc_options,
        jpeg_options,
    }
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

pub fn configure_cog_with_layer_sizes(
    base: GeoTiffBuilder,
    opts: &CogOutputOptions,
    levels: Vec<u32>,
    overview_sizes: Vec<(u32, u32)>,
) -> CogBuilder {
    CogBuilder::new(base)
        .resampling(opts.resampling.to_resampling())
        .subifd_overviews()
        .overview_levels(levels)
        .overview_layer_sizes(overview_sizes)
}

/// Like [`configure_cog_with_layer_sizes`] but uses a classic top-level IFD chain
/// required for GDAL per-dataset transparency masks.
pub fn configure_cog_with_layer_sizes_masked(
    base: GeoTiffBuilder,
    opts: &CogOutputOptions,
    levels: Vec<u32>,
    overview_sizes: Vec<(u32, u32)>,
) -> CogBuilder {
    CogBuilder::new(base)
        .resampling(opts.resampling.to_resampling())
        .overview_storage(OverviewStorage::TopLevelIfds)
        .overview_levels(levels)
        .overview_layer_sizes(overview_sizes)
}

pub use super::grid::overview_sizes as layer_sizes;
