use std::path::Path;

use anyhow::{bail, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::RemuxCompressedBlock;
use tiff_core::{Compression, PlanarConfiguration, SampleFormat};
use tiff_reader::Ifd;

use crate::cog::tile_payload::{
    collect_remux_layers, ifd_planar, ifd_sample_format, input_compression, read_layer_blocks,
};
use crate::cog::{
    configure_cog, configure_cog_with_layer_sizes, configure_cog_with_layer_sizes_masked,
    auto_overview_levels, overview_levels, CogOutputOptions,
};
use crate::cog::mask::prepare_remux_layers;
use crate::crop::WriteWindow;
use crate::input::RasterProfile;
use crate::open::open_geotiff;
use crate::path::ConvertPath;
use crate::progress::{ProgressTracker, StageBar};
use crate::util::ensure_parent_dir;

/// Attempt a fast COG rewrite without decoding pixels.
///
/// Strategies (in order):
/// 1. **Identity remux** — copy every compressed tile block unchanged (COG→COG copy, identity bands)
/// 2. **Planar band permute** — reorder separate band planes by copying their tile blocks (no decode)
/// 3. **Hybrid crop remux** — copy full source tiles when possible; decode/recompress only
///    edge tiles. Overview layers are cropped from source overviews (not resampled from base).
pub fn try_remux_cog(
    pool: &rayon::ThreadPool,
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    window: Option<&WriteWindow>,
    bands: Option<&[usize]>,
    show_progress: bool,
) -> Result<Option<ConvertPath>> {
    let base_ifd = input.tiff().ifd(input.base_ifd_index())?;
    if !base_ifd.is_tiled() {
        return try_strip_to_tiled_remux(
            pool, input, output, profile, opts, window, bands, show_progress,
        );
    }

    if !layout_compatible(input, profile, opts)? {
        return Ok(None);
    }

    if let Some(window) = window {
        if try_remux_crop(input, output, profile, opts, window, show_progress)? {
            return Ok(Some(ConvertPath::HybridCropRemux));
        }
        return Ok(None);
    }

    if let Some(bands) = bands {
        if is_identity_bands(bands, input.band_count() as usize) {
            if compression_matches(opts, input_compression(input.tiff().ifd(input.base_ifd_index())?)) {
                if overview_tiles_compatible(input, opts.blocksize)? {
                    remux_identity(input, output, profile, opts, show_progress)?;
                    return Ok(Some(ConvertPath::RemuxIdentity));
                }
                if input.overview_count() > 0 {
                    if try_transcode_remux(input, output, profile, opts, show_progress)? {
                        return Ok(Some(ConvertPath::TranscodeRemux));
                    }
                    remux_identity(input, output, profile, opts, show_progress)?;
                    return Ok(Some(ConvertPath::RemuxIdentity));
                }
                remux_identity(input, output, profile, opts, show_progress)?;
                return Ok(Some(ConvertPath::RemuxIdentity));
            }
            if try_transcode_remux(input, output, profile, opts, show_progress)? {
                return Ok(Some(ConvertPath::TranscodeRemux));
            }
            return Ok(None);
        }
        if ifd_planar(input.tiff().ifd(input.base_ifd_index())?) == PlanarConfiguration::Planar {
            remux_planar_band_permute(input, output, profile, opts, bands, show_progress)?;
            return Ok(Some(ConvertPath::PlanarBandPermute));
        }
        if remux_chunky_band_permute(input, output, profile, opts, bands, show_progress)? {
            return Ok(Some(ConvertPath::ChunkyBandPermute));
        }
        return Ok(None);
    }

    if compression_matches(opts, input_compression(input.tiff().ifd(input.base_ifd_index())?)) {
        if overview_tiles_compatible(input, opts.blocksize)? {
            remux_identity(input, output, profile, opts, show_progress)?;
            return Ok(Some(ConvertPath::RemuxIdentity));
        }
        if input.overview_count() > 0 {
            if try_transcode_remux(input, output, profile, opts, show_progress)? {
                return Ok(Some(ConvertPath::TranscodeRemux));
            }
            remux_identity(input, output, profile, opts, show_progress)?;
            return Ok(Some(ConvertPath::RemuxIdentity));
        }
        remux_identity(input, output, profile, opts, show_progress)?;
        return Ok(Some(ConvertPath::RemuxIdentity));
    }

    if try_transcode_remux(input, output, profile, opts, show_progress)? {
        return Ok(Some(ConvertPath::TranscodeRemux));
    }
    Ok(None)
}

fn layout_compatible(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
) -> Result<bool> {
    let base_ifd = input.tiff().ifd(input.base_ifd_index())?;
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

fn try_strip_to_tiled_remux(
    pool: &rayon::ThreadPool,
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    window: Option<&WriteWindow>,
    bands: Option<&[usize]>,
    show_progress: bool,
) -> Result<Option<ConvertPath>> {
    if bands.is_some_and(|bands| !is_identity_bands(bands, input.band_count() as usize)) {
        return Ok(None);
    }

    let window_owned = window.cloned();
    match (profile.sample.bits_per_sample, profile.sample.sample_format) {
        (8, SampleFormat::Uint) => {
            crate::strip_encode::convert_strip_to_remux_cog::<u8>(
                pool,
                input,
                output,
                profile,
                opts,
                window_owned,
                bands,
                show_progress,
            )?;
        }
        (8, SampleFormat::Int) => {
            crate::strip_encode::convert_strip_to_remux_cog::<i8>(
                pool,
                input,
                output,
                profile,
                opts,
                window_owned,
                bands,
                show_progress,
            )?;
        }
        (16, SampleFormat::Uint) => {
            crate::strip_encode::convert_strip_to_remux_cog::<u16>(
                pool,
                input,
                output,
                profile,
                opts,
                window_owned,
                bands,
                show_progress,
            )?;
        }
        (16, SampleFormat::Int) => {
            crate::strip_encode::convert_strip_to_remux_cog::<i16>(
                pool,
                input,
                output,
                profile,
                opts,
                window_owned,
                bands,
                show_progress,
            )?;
        }
        (32, SampleFormat::Uint) => {
            crate::strip_encode::convert_strip_to_remux_cog::<u32>(
                pool,
                input,
                output,
                profile,
                opts,
                window_owned,
                bands,
                show_progress,
            )?;
        }
        (32, SampleFormat::Int) => {
            crate::strip_encode::convert_strip_to_remux_cog::<i32>(
                pool,
                input,
                output,
                profile,
                opts,
                window_owned,
                bands,
                show_progress,
            )?;
        }
        (32, SampleFormat::Float) => {
            crate::strip_encode::convert_strip_to_remux_cog::<f32>(
                pool,
                input,
                output,
                profile,
                opts,
                window_owned,
                bands,
                show_progress,
            )?;
        }
        (64, SampleFormat::Uint) => {
            crate::strip_encode::convert_strip_to_remux_cog::<u64>(
                pool,
                input,
                output,
                profile,
                opts,
                window_owned,
                bands,
                show_progress,
            )?;
        }
        (64, SampleFormat::Int) => {
            crate::strip_encode::convert_strip_to_remux_cog::<i64>(
                pool,
                input,
                output,
                profile,
                opts,
                window_owned,
                bands,
                show_progress,
            )?;
        }
        (64, SampleFormat::Float) => {
            crate::strip_encode::convert_strip_to_remux_cog::<f64>(
                pool,
                input,
                output,
                profile,
                opts,
                window_owned,
                bands,
                show_progress,
            )?;
        }
        _ => return Ok(None),
    }
    Ok(Some(ConvertPath::StripEncode))
}

fn try_transcode_remux(
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    show_progress: bool,
) -> Result<bool> {
    if input.overview_count() == 0 {
        return Ok(false);
    }

    let progress = ProgressTracker::new(show_progress);
    let transcode_bar = progress.stage("Transcode", 1);

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
    transcode_bar.inc(1);
    transcode_bar.done("done");

    remux_with_layers(
        input,
        profile,
        opts,
        output_layers,
        output,
        None,
        Some(levels),
        Some(input_overview_layer_sizes(input)?),
        show_progress,
    )?;
    progress.finish();
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

fn collect_all_layers(
    input: &GeoTiffFile,
    progress: Option<&StageBar>,
) -> Result<Vec<Vec<RemuxCompressedBlock>>> {
    let base_ifd = input.tiff().ifd(input.base_ifd_index())?;
    if input.overview_count() > 0 {
        collect_remux_layers(input, progress)
    } else {
        let blocks = read_layer_blocks(input.tiff(), base_ifd)?;
        if let Some(bar) = progress {
            bar.inc(1);
        }
        Ok(vec![blocks])
    }
}

fn remux_identity(
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    show_progress: bool,
) -> Result<()> {
    let progress = ProgressTracker::new(show_progress);
    let read_bar = progress.stage("Remux read", (1 + input.overview_count()) as u64);
    let layers = collect_all_layers(input, Some(&read_bar))?;
    read_bar.done("done");
    let levels = input_overview_levels(input)?;
    let sizes = input_overview_layer_sizes(input)?;
    remux_with_layers(
        input,
        profile,
        opts,
        layers,
        output,
        None,
        Some(levels),
        Some(sizes),
        show_progress,
    )?;
    progress.finish();
    Ok(())
}

fn remux_planar_band_permute(
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    bands: &[usize],
    show_progress: bool,
) -> Result<()> {
    let progress = ProgressTracker::new(show_progress);
    let read_bar = progress.stage("Remux read", (1 + input.overview_count()) as u64);
    let source_layers = collect_all_layers(input, Some(&read_bar))?;
    read_bar.done("done");

    let mut output_layers = Vec::with_capacity(source_layers.len());
    for (layer_index, layer) in source_layers.iter().enumerate() {
        let ifd = layer_ifd(input, layer_index)?;
        output_layers.push(permute_planar_layer_blocks(layer, ifd, bands)?);
    }

    remux_with_layers(
        input,
        profile,
        opts,
        output_layers,
        output,
        None,
        Some(input_overview_levels(input)?),
        Some(input_overview_layer_sizes(input)?),
        show_progress,
    )?;
    progress.finish();
    Ok(())
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
    show_progress: bool,
) -> Result<bool> {
    let progress = ProgressTracker::new(show_progress);
    let crop_bar = progress.stage("Crop encode", 1);
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
    crop_bar.inc(1);
    crop_bar.done("done");

    remux_with_layers(
        input,
        profile,
        opts,
        output_layers,
        output,
        Some(window),
        Some(output_levels),
        None,
        show_progress,
    )?;
    progress.finish();
    Ok(true)
}

fn remux_chunky_band_permute(
    input: &GeoTiffFile,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    bands: &[usize],
    show_progress: bool,
) -> Result<bool> {
    let progress = ProgressTracker::new(show_progress);
    let encode_bar = progress.stage("Band permute", 1);
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
    encode_bar.inc(1);
    encode_bar.done("done");
    remux_with_layers(
        input,
        profile,
        opts,
        output_layers,
        output,
        None,
        Some(input_overview_levels(input)?),
        Some(input_overview_layer_sizes(input)?),
        show_progress,
    )?;
    progress.finish();
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
    input: &GeoTiffFile,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    layers: Vec<Vec<RemuxCompressedBlock>>,
    output: &Path,
    window: Option<&WriteWindow>,
    overview_levels: Option<Vec<u32>>,
    overview_sizes: Option<Vec<(u32, u32)>>,
    show_progress: bool,
) -> Result<()> {
    remux_encoded_layers(
        input,
        profile,
        opts,
        layers,
        output,
        window,
        overview_levels,
        overview_sizes,
        show_progress,
    )
}

pub(crate) fn remux_encoded_layers(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    layers: Vec<Vec<RemuxCompressedBlock>>,
    output: &Path,
    window: Option<&WriteWindow>,
    mut overview_levels: Option<Vec<u32>>,
    mut overview_sizes: Option<Vec<(u32, u32)>>,
    show_progress: bool,
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

    let levels_slice = overview_levels.as_deref().unwrap_or(&[]);
    let (remux_layers, has_masks) = prepare_remux_layers(
        input,
        profile,
        layers,
        window,
        profile.width,
        profile.height,
        levels_slice,
        opts,
    )?;

    let cog = match (overview_levels, overview_sizes, has_masks) {
        (Some(levels), Some(sizes), true) => configure_cog_with_layer_sizes_masked(
            profile.base_builder(opts),
            opts,
            levels,
            sizes,
        ),
        (Some(levels), Some(sizes), false) => configure_cog_with_layer_sizes(
            profile.base_builder(opts),
            opts,
            levels,
            sizes,
        ),
        (Some(levels), None, true) => {
            crate::cog::configure_cog_with_levels(profile.base_builder(opts), opts, levels)
                .overview_storage(geotiff_writer::OverviewStorage::TopLevelIfds)
        }
        (Some(levels), None, false) => {
            crate::cog::configure_cog_with_levels(profile.base_builder(opts), opts, levels)
        }
        _ => configure_cog(profile.base_builder(opts), opts, profile.width, profile.height),
    };
    let progress = ProgressTracker::new(show_progress);
    let write_bar = progress.stage("Write COG", 1);
    let result = match (profile.sample.bits_per_sample, profile.sample.sample_format) {
        (8, SampleFormat::Uint) => cog.remux_layers_to_file::<u8, _>(output, remux_layers),
        (8, SampleFormat::Int) => cog.remux_layers_to_file::<i8, _>(output, remux_layers),
        (16, SampleFormat::Uint) => cog.remux_layers_to_file::<u16, _>(output, remux_layers),
        (16, SampleFormat::Int) => cog.remux_layers_to_file::<i16, _>(output, remux_layers),
        (32, SampleFormat::Uint) => cog.remux_layers_to_file::<u32, _>(output, remux_layers),
        (32, SampleFormat::Int) => cog.remux_layers_to_file::<i32, _>(output, remux_layers),
        (32, SampleFormat::Float) => cog.remux_layers_to_file::<f32, _>(output, remux_layers),
        (64, SampleFormat::Uint) => cog.remux_layers_to_file::<u64, _>(output, remux_layers),
        (64, SampleFormat::Int) => cog.remux_layers_to_file::<i64, _>(output, remux_layers),
        (64, SampleFormat::Float) => cog.remux_layers_to_file::<f64, _>(output, remux_layers),
        _ => bail!("unsupported sample layout for remux"),
    }
    .map_err(|err| anyhow::anyhow!(err));
    write_bar.inc(1);
    write_bar.done("done");
    progress.finish();
    result
}

pub fn remux_if_possible(
    pool: &rayon::ThreadPool,
    input_path: &Path,
    output: &Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    window: Option<&WriteWindow>,
    bands: Option<&[usize]>,
    mmap: bool,
    show_progress: bool,
) -> Result<Option<ConvertPath>> {
    ensure_parent_dir(output)?;
    let input = open_geotiff(input_path, mmap)?;
    try_remux_cog(
        pool,
        &input,
        output,
        profile,
        opts,
        window,
        bands,
        show_progress,
    )
}
