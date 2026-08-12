use std::path::Path;

use anyhow::{bail, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::RemuxCompressedBlock;
use tiff_core::{Compression, PlanarConfiguration, SampleFormat};
use tiff_reader::Ifd;

use crate::cog::tile_payload::{
    collect_remux_layers, ifd_planar, ifd_sample_format, input_compression, read_layer_blocks,
};
use crate::cog::{configure_cog, configure_cog_with_layer_sizes, auto_overview_levels, overview_levels, CogOutputOptions};
use crate::crop::WriteWindow;
use crate::input::RasterProfile;
use crate::open::open_geotiff;

/// Attempt a fast COG rewrite without decoding pixels.
///
/// Strategies (in order):
/// 1. **Identity remux** — copy every compressed tile block unchanged (COG→COG copy, identity bands)
/// 2. **Planar band permute** — reorder separate band planes by copying their tile blocks (no decode)
/// 3. **Hybrid crop remux** — copy full source tiles when possible; decode/recompress only
///    edge tiles. Overview layers are cropped from source overviews (not resampled from base).
pub fn try_remux_cog(
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    window: Option<&WriteWindow>,
    bands: Option<&[usize]>,
) -> Result<bool> {
    if !layout_compatible(input, profile, opts)? {
        return Ok(false);
    }

    if let Some(window) = window {
        return try_remux_crop(input, output, profile, opts, window);
    }

    if let Some(bands) = bands {
        if is_identity_bands(bands, input.band_count() as usize) {
            if compression_matches(opts, input_compression(input.tiff().ifd(input.base_ifd_index())?)) {
                if overview_tiles_compatible(input, opts.blocksize)? {
                    return remux_identity(input, output, profile, opts);
                }
                if input.overview_count() > 0 {
                    return try_transcode_remux(input, output, profile, opts);
                }
                return remux_identity(input, output, profile, opts);
            }
            return try_transcode_remux(input, output, profile, opts);
        }
        if ifd_planar(input.tiff().ifd(input.base_ifd_index())?) == PlanarConfiguration::Planar {
            return remux_planar_band_permute(input, output, profile, opts, bands);
        }
        return remux_chunky_band_permute(input, output, profile, opts, bands);
    }

    if compression_matches(opts, input_compression(input.tiff().ifd(input.base_ifd_index())?)) {
        if overview_tiles_compatible(input, opts.blocksize)? {
            return remux_identity(input, output, profile, opts);
        }
        if input.overview_count() > 0 {
            return try_transcode_remux(input, output, profile, opts);
        }
        return remux_identity(input, output, profile, opts);
    }

    try_transcode_remux(input, output, profile, opts)
}

fn layout_compatible(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
) -> Result<bool> {
    let base_ifd = input.tiff().ifd(input.base_ifd_index())?;
    if !base_ifd.is_tiled() {
        return Ok(false);
    }

    let (tile_w, tile_h) = match (base_ifd.tile_width(), base_ifd.tile_height()) {
        (Some(w), Some(h)) => (w, h),
        _ => return Ok(false),
    };
    if tile_w != opts.blocksize || tile_h != opts.blocksize {
        return Ok(false);
    }

    if ifd_planar(base_ifd) != profile.planar_configuration {
        return Ok(false);
    }

    if ifd_sample_format(base_ifd)? != profile.sample.sample_format {
        return Ok(false);
    }

    if opts.no_overviews {
        if input.overview_count() > 0 {
            return Ok(false);
        }
    }

    Ok(true)
}

fn try_transcode_remux(
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
) -> Result<bool> {
    if input.overview_count() == 0 {
        return Ok(false);
    }

    let levels = input_overview_levels(input)?;
    let output_layers = match (profile.sample.bits_per_sample, profile.sample.sample_format) {
        (8, SampleFormat::Uint) => {
            crate::transcode::build_transcode_layers::<u8>(input, profile, opts)?
        }
        (8, SampleFormat::Int) => {
            crate::transcode::build_transcode_layers::<i8>(input, profile, opts)?
        }
        (16, SampleFormat::Uint) => {
            crate::transcode::build_transcode_layers::<u16>(input, profile, opts)?
        }
        (16, SampleFormat::Int) => {
            crate::transcode::build_transcode_layers::<i16>(input, profile, opts)?
        }
        (32, SampleFormat::Uint) => {
            crate::transcode::build_transcode_layers::<u32>(input, profile, opts)?
        }
        (32, SampleFormat::Int) => {
            crate::transcode::build_transcode_layers::<i32>(input, profile, opts)?
        }
        (32, SampleFormat::Float) => {
            crate::transcode::build_transcode_layers::<f32>(input, profile, opts)?
        }
        (64, SampleFormat::Uint) => {
            crate::transcode::build_transcode_layers::<u64>(input, profile, opts)?
        }
        (64, SampleFormat::Int) => {
            crate::transcode::build_transcode_layers::<i64>(input, profile, opts)?
        }
        (64, SampleFormat::Float) => {
            crate::transcode::build_transcode_layers::<f64>(input, profile, opts)?
        }
        _ => return Ok(false),
    };

    remux_encoded_layers(
        profile,
        opts,
        output_layers,
        output,
        Some(levels),
        Some(input_overview_layer_sizes(input)?),
    )?;
    Ok(true)
}

pub(crate) fn best_source_overview(source_factors: &[u32], target_level: u32) -> Option<(usize, u32)> {
    source_factors
        .iter()
        .enumerate()
        .filter(|(_, factor)| **factor >= target_level)
        .min_by_key(|(_, factor)| *factor)
        .map(|(index, &factor)| (index, factor))
}

/// Resolve which source pyramid layer to read when building a target overview level.
/// Returns `(layer_index, source_factor, downsample)` where `layer_index` is 0 for base
/// and 1+ for overview IFDs, and `downsample` is the extra reduction after the read.
pub(crate) fn resolve_overview_read_source(
    input: &GeoTiffFile,
    level: u32,
) -> Result<(usize, u32, usize)> {
    let factors = input_overview_levels(input)?;
    if let Some(index) = factors.iter().position(|&factor| factor == level) {
        return Ok((index + 1, level, 1));
    }
    if let Some((index, factor)) = best_source_overview(&factors, level) {
        let downsample = (factor / level).max(1) as usize;
        return Ok((index + 1, factor, downsample));
    }
    Ok((0, 1, level as usize))
}

pub(crate) fn input_overview_levels(input: &GeoTiffFile) -> Result<Vec<u32>> {
    let base_w = input.width();
    let mut levels = Vec::with_capacity(input.overview_count());
    for index in 0..input.overview_count() {
        let ov = input.overview_ifd(index)?;
        let ov_w = ov.width().max(1);
        let factor = (base_w / ov_w).max(1);
        levels.push(factor);
    }
    Ok(levels)
}

pub(crate) fn input_overview_layer_sizes(input: &GeoTiffFile) -> Result<Vec<(u32, u32)>> {
    let mut sizes = Vec::with_capacity(input.overview_count());
    for index in 0..input.overview_count() {
        let ov = input.overview_ifd(index)?;
        sizes.push((ov.width(), ov.height()));
    }
    Ok(sizes)
}

fn collect_all_layers(input: &GeoTiffFile) -> Result<Vec<Vec<RemuxCompressedBlock>>> {
    let base_ifd = input.tiff().ifd(input.base_ifd_index())?;
    if input.overview_count() > 0 {
        collect_remux_layers(input)
    } else {
        Ok(vec![read_layer_blocks(input.tiff(), base_ifd)?])
    }
}

fn remux_identity(
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
) -> Result<bool> {
    let layers = collect_all_layers(input)?;
    let levels = input_overview_levels(input)?;
    let sizes = input_overview_layer_sizes(input)?;
    remux_with_layers(profile, opts, layers, output, Some(levels), Some(sizes))?;
    Ok(true)
}

fn remux_planar_band_permute(
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    bands: &[usize],
) -> Result<bool> {
    let source_layers = collect_all_layers(input)?;

    let mut output_layers = Vec::with_capacity(source_layers.len());
    for (layer_index, layer) in source_layers.iter().enumerate() {
        let ifd = layer_ifd(input, layer_index)?;
        output_layers.push(permute_planar_layer_blocks(layer, ifd, bands)?);
    }

    remux_with_layers(
        profile,
        opts,
        output_layers,
        output,
        Some(input_overview_levels(input)?),
        Some(input_overview_layer_sizes(input)?),
    )?;
    Ok(true)
}

pub(crate) fn layer_ifd(input: &GeoTiffFile, layer_index: usize) -> Result<&Ifd> {
    if layer_index == 0 {
        input
            .tiff()
            .ifd(input.base_ifd_index())
            .map_err(|err| anyhow::anyhow!(err))
    } else {
        input
            .overview_ifd(layer_index - 1)
            .map_err(|err| anyhow::anyhow!(err))
    }
}

fn permute_planar_layer_blocks(
    layer: &[RemuxCompressedBlock],
    ifd: &Ifd,
    bands: &[usize],
) -> Result<Vec<RemuxCompressedBlock>> {
    let tile_size = ifd.tile_width().unwrap_or(256) as usize;
    let tiles_across = (ifd.width() as usize).div_ceil(tile_size);
    let tiles_down = (ifd.height() as usize).div_ceil(tile_size);
    let tiles_per_plane = tiles_across * tiles_down;
    let plane_count = ifd.samples_per_pixel() as usize;
    if layer.len() != tiles_per_plane * plane_count {
        bail!("unexpected planar block count for remux");
    }

    let mut out = Vec::with_capacity(tiles_per_plane * bands.len());
    for out_band in bands {
        let src_plane = out_band - 1;
        if src_plane >= plane_count {
            bail!("band {out_band} is out of range for planar remux");
        }
        let start = src_plane * tiles_per_plane;
        out.extend_from_slice(&layer[start..start + tiles_per_plane]);
    }
    Ok(out)
}

fn try_remux_crop(
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    window: &WriteWindow,
) -> Result<bool> {
    let output_levels = overview_levels(opts, profile.width, profile.height);
    let output_layers = match (profile.sample.bits_per_sample, profile.sample.sample_format) {
        (8, SampleFormat::Uint) => crate::hybrid_crop::build_hybrid_crop_layers::<u8>(
            input, window, profile, opts, &output_levels,
        )?,
        (8, SampleFormat::Int) => crate::hybrid_crop::build_hybrid_crop_layers::<i8>(
            input, window, profile, opts, &output_levels,
        )?,
        (16, SampleFormat::Uint) => crate::hybrid_crop::build_hybrid_crop_layers::<u16>(
            input, window, profile, opts, &output_levels,
        )?,
        (16, SampleFormat::Int) => crate::hybrid_crop::build_hybrid_crop_layers::<i16>(
            input, window, profile, opts, &output_levels,
        )?,
        (32, SampleFormat::Uint) => crate::hybrid_crop::build_hybrid_crop_layers::<u32>(
            input, window, profile, opts, &output_levels,
        )?,
        (32, SampleFormat::Int) => crate::hybrid_crop::build_hybrid_crop_layers::<i32>(
            input, window, profile, opts, &output_levels,
        )?,
        (32, SampleFormat::Float) => crate::hybrid_crop::build_hybrid_crop_layers::<f32>(
            input, window, profile, opts, &output_levels,
        )?,
        (64, SampleFormat::Uint) => crate::hybrid_crop::build_hybrid_crop_layers::<u64>(
            input, window, profile, opts, &output_levels,
        )?,
        (64, SampleFormat::Int) => crate::hybrid_crop::build_hybrid_crop_layers::<i64>(
            input, window, profile, opts, &output_levels,
        )?,
        (64, SampleFormat::Float) => crate::hybrid_crop::build_hybrid_crop_layers::<f64>(
            input, window, profile, opts, &output_levels,
        )?,
        _ => return Ok(false),
    };

    remux_with_layers(profile, opts, output_layers, output, Some(output_levels), None)?;
    Ok(true)
}

fn remux_chunky_band_permute(
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    bands: &[usize],
) -> Result<bool> {
    let output_layers = match (profile.sample.bits_per_sample, profile.sample.sample_format) {
        (8, SampleFormat::Uint) => {
            crate::chunky_permute::build_chunky_band_permute_layers::<u8>(input, bands, profile, opts)?
        }
        (8, SampleFormat::Int) => {
            crate::chunky_permute::build_chunky_band_permute_layers::<i8>(input, bands, profile, opts)?
        }
        (16, SampleFormat::Uint) => {
            crate::chunky_permute::build_chunky_band_permute_layers::<u16>(input, bands, profile, opts)?
        }
        (16, SampleFormat::Int) => {
            crate::chunky_permute::build_chunky_band_permute_layers::<i16>(input, bands, profile, opts)?
        }
        (32, SampleFormat::Uint) => {
            crate::chunky_permute::build_chunky_band_permute_layers::<u32>(input, bands, profile, opts)?
        }
        (32, SampleFormat::Int) => {
            crate::chunky_permute::build_chunky_band_permute_layers::<i32>(input, bands, profile, opts)?
        }
        (32, SampleFormat::Float) => {
            crate::chunky_permute::build_chunky_band_permute_layers::<f32>(input, bands, profile, opts)?
        }
        (64, SampleFormat::Uint) => {
            crate::chunky_permute::build_chunky_band_permute_layers::<u64>(input, bands, profile, opts)?
        }
        (64, SampleFormat::Int) => {
            crate::chunky_permute::build_chunky_band_permute_layers::<i64>(input, bands, profile, opts)?
        }
        (64, SampleFormat::Float) => {
            crate::chunky_permute::build_chunky_band_permute_layers::<f64>(input, bands, profile, opts)?
        }
        _ => return Ok(false),
    };
    remux_with_layers(
        profile,
        opts,
        output_layers,
        output,
        Some(input_overview_levels(input)?),
        Some(input_overview_layer_sizes(input)?),
    )?;
    Ok(true)
}

fn is_identity_bands(bands: &[usize], band_count: usize) -> bool {
    bands.len() == band_count && bands.iter().enumerate().all(|(i, b)| *b == i + 1)
}

fn compression_matches(opts: &CogOutputOptions, input: Compression) -> bool {
    opts.compression.to_compression() == input
}

fn overview_tiles_compatible(input: &GeoTiffFile, blocksize: u32) -> Result<bool> {
    for index in 0..input.overview_count() {
        let ov = input.overview_ifd(index)?;
        let (tile_w, tile_h) = match (ov.tile_width(), ov.tile_height()) {
            (Some(w), Some(h)) => (w, h),
            _ => return Ok(false),
        };
        if tile_w != blocksize || tile_h != blocksize {
            return Ok(false);
        }
    }
    Ok(true)
}

fn remux_with_layers(
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    layers: Vec<Vec<RemuxCompressedBlock>>,
    output: &Path,
    overview_levels: Option<Vec<u32>>,
    overview_sizes: Option<Vec<(u32, u32)>>,
) -> Result<()> {
    remux_encoded_layers(
        profile,
        opts,
        layers,
        output,
        overview_levels,
        overview_sizes,
    )
}

pub(crate) fn remux_encoded_layers(
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    layers: Vec<Vec<RemuxCompressedBlock>>,
    output: &Path,
    mut overview_levels: Option<Vec<u32>>,
    mut overview_sizes: Option<Vec<(u32, u32)>>,
) -> Result<()> {
    let target_overviews = layers.len().saturating_sub(1);
    if let Some(levels) = overview_levels.as_mut() {
        levels.truncate(target_overviews);
    }
    if let Some(sizes) = overview_sizes.as_mut() {
        sizes.truncate(target_overviews);
    }
    if overview_levels.is_none() && target_overviews > 0 {
        overview_levels = Some(
            auto_overview_levels(profile.width, profile.height, opts.blocksize)
                .into_iter()
                .take(target_overviews)
                .collect(),
        );
    }

    let cog = match (overview_levels, overview_sizes) {
        (Some(levels), Some(sizes)) => configure_cog_with_layer_sizes(
            profile.base_builder(opts),
            opts,
            levels,
            sizes,
        ),
        (Some(levels), None) => {
            crate::cog::configure_cog_with_levels(profile.base_builder(opts), opts, levels)
        }
        _ => configure_cog(profile.base_builder(opts), opts, profile.width, profile.height),
    };
    match (profile.sample.bits_per_sample, profile.sample.sample_format) {
        (8, SampleFormat::Uint) => cog.remux_to_file::<u8, _>(output, layers),
        (8, SampleFormat::Int) => cog.remux_to_file::<i8, _>(output, layers),
        (16, SampleFormat::Uint) => cog.remux_to_file::<u16, _>(output, layers),
        (16, SampleFormat::Int) => cog.remux_to_file::<i16, _>(output, layers),
        (32, SampleFormat::Uint) => cog.remux_to_file::<u32, _>(output, layers),
        (32, SampleFormat::Int) => cog.remux_to_file::<i32, _>(output, layers),
        (32, SampleFormat::Float) => cog.remux_to_file::<f32, _>(output, layers),
        (64, SampleFormat::Uint) => cog.remux_to_file::<u64, _>(output, layers),
        (64, SampleFormat::Int) => cog.remux_to_file::<i64, _>(output, layers),
        (64, SampleFormat::Float) => cog.remux_to_file::<f64, _>(output, layers),
        _ => bail!("unsupported sample layout for remux"),
    }
    .map_err(|err| anyhow::anyhow!(err))
}

pub fn remux_if_possible(
    input_path: &Path,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    window: Option<&WriteWindow>,
    bands: Option<&[usize]>,
    mmap: bool,
) -> Result<bool> {
    let input = open_geotiff(input_path, mmap)?;
    try_remux_cog(&input, output, profile, opts, window, bands)
}
