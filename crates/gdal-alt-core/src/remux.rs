use std::path::Path;

use anyhow::{bail, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::RemuxCompressedBlock;
use tiff_core::{Compression, PlanarConfiguration, SampleFormat};
use tiff_reader::Ifd;

use crate::cog::tile_payload::{
    collect_remux_layers, ifd_planar, ifd_predictor, ifd_sample_format, input_compression,
    read_layer_blocks,
};
use crate::cog::{configure_cog, overview_levels, CogOutputOptions};
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
    if !compatible_cog_source(input, profile, opts)? {
        return Ok(false);
    }

    if let Some(window) = window {
        return try_remux_crop(input, output, profile, opts, window);
    }

    if let Some(bands) = bands {
        if is_identity_bands(bands, input.band_count() as usize) {
            return remux_identity(input, output, profile, opts);
        }
        if ifd_planar(input.tiff().ifd(input.base_ifd_index())?) == PlanarConfiguration::Planar {
            return remux_planar_band_permute(input, output, profile, opts, bands);
        }
        return Ok(false);
    }

    remux_identity(input, output, profile, opts)
}

fn compatible_cog_source(
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

    if !compression_matches(opts, input_compression(base_ifd)) {
        return Ok(false);
    }

    if ifd_planar(base_ifd) != profile.planar_configuration {
        return Ok(false);
    }

    if ifd_sample_format(base_ifd)? != profile.sample.sample_format {
        return Ok(false);
    }

    let expected_levels = overview_levels(opts, input.width(), input.height());
    if opts.no_overviews {
        if input.overview_count() > 0 {
            return Ok(false);
        }
    } else if !overview_levels_match(input, &expected_levels, tile_w, tile_h, base_ifd)? {
        return Ok(false);
    }

    Ok(true)
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
    remux_with_layers(profile, opts, layers, output)?;
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

    remux_with_layers(profile, opts, output_layers, output)?;
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
    let base_ifd = input.tiff().ifd(input.base_ifd_index())?;
    let tile_size = base_ifd.tile_width().unwrap_or(opts.blocksize) as usize;

    if profile.sample.bits_per_sample == 8 && profile.sample.sample_format == SampleFormat::Uint {
        let source_layers = collect_all_layers(input)?;
        let output_layers = crate::hybrid_crop::build_hybrid_crop_layers_u8(
            input,
            &source_layers,
            window,
            profile,
            opts,
        )?;
        remux_with_layers(profile, opts, output_layers, output)?;
        return Ok(true);
    }

    if !is_tile_aligned(window, tile_size) {
        return Ok(false);
    }

    let source_layers = collect_all_layers(input)?;
    let output_levels = overview_levels(opts, profile.width, profile.height);

    let mut output_layers = Vec::with_capacity(1 + output_levels.len());
    output_layers.push(crop_layer_blocks(
        &source_layers[0],
        layer_ifd(input, 0)?,
        window,
        tile_size,
    )?);
    for (ov_idx, &level) in output_levels.iter().enumerate() {
        let source_idx = ov_idx + 1;
        let ifd = layer_ifd(input, source_idx)?;
        let crop = scale_window(window, level);
        output_layers.push(crop_layer_blocks(
            &source_layers[source_idx],
            ifd,
            &crop,
            tile_size,
        )?);
    }

    remux_with_layers(profile, opts, output_layers, output)?;
    Ok(true)
}

fn is_tile_aligned(window: &WriteWindow, tile_size: usize) -> bool {
    window.col_off.is_multiple_of(tile_size)
        && window.row_off.is_multiple_of(tile_size)
        && window.width.is_multiple_of(tile_size)
        && window.height.is_multiple_of(tile_size)
}

fn scale_window(window: &WriteWindow, scale: u32) -> WriteWindow {
    let scale = scale as usize;
    WriteWindow {
        col_off: window.col_off / scale,
        row_off: window.row_off / scale,
        width: window.width / scale,
        height: window.height / scale,
    }
}

fn crop_layer_blocks(
    layer: &[RemuxCompressedBlock],
    ifd: &Ifd,
    window: &WriteWindow,
    tile_size: usize,
) -> Result<Vec<RemuxCompressedBlock>> {
    let planar = ifd_planar(ifd) == PlanarConfiguration::Planar;
    let bands = ifd.samples_per_pixel() as usize;
    let width = ifd.width() as usize;
    let height = ifd.height() as usize;
    let tiles_across = width.div_ceil(tile_size);
    let tiles_down = height.div_ceil(tile_size);
    let tiles_per_plane = tiles_across * tiles_down;

    let col0 = window.col_off / tile_size;
    let row0 = window.row_off / tile_size;
    let col1 = (window.col_off + window.width).div_ceil(tile_size).min(tiles_across);
    let row1 = (window.row_off + window.height).div_ceil(tile_size).min(tiles_down);

    let plane_count = if planar { bands } else { 1 };
    let mut out = Vec::new();

    for plane in 0..plane_count {
        for row in row0..row1 {
            for col in col0..col1 {
                let tile_index = row * tiles_across + col;
                let block_index = if planar {
                    plane * tiles_per_plane + tile_index
                } else {
                    tile_index
                };
                out.push(
                    layer
                        .get(block_index)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("missing tile block {block_index}"))?,
                );
            }
        }
    }

    Ok(out)
}

fn is_identity_bands(bands: &[usize], band_count: usize) -> bool {
    bands.len() == band_count && bands.iter().enumerate().all(|(i, b)| *b == i + 1)
}

fn compression_matches(opts: &CogOutputOptions, input: Compression) -> bool {
    opts.compression.to_compression() == input
}

fn overview_levels_match(
    input: &GeoTiffFile,
    expected_levels: &[u32],
    tile_w: u32,
    tile_h: u32,
    base_ifd: &Ifd,
) -> Result<bool> {
    if expected_levels.len() != input.overview_count() {
        return Ok(false);
    }

    let base_w = base_ifd.width();
    let base_h = base_ifd.height();
    for (index, &level) in expected_levels.iter().enumerate() {
        let ov = input.overview_ifd(index)?;
        let expected_w = base_w.div_ceil(level);
        let expected_h = base_h.div_ceil(level);
        if ov.width() != expected_w || ov.height() != expected_h {
            return Ok(false);
        }
        if ov.tile_width() != Some(tile_w) || ov.tile_height() != Some(tile_h) {
            return Ok(false);
        }
        if input_compression(ov) != input_compression(base_ifd) {
            return Ok(false);
        }
        if ifd_predictor(ov) != ifd_predictor(base_ifd) {
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
) -> Result<()> {
    let cog = configure_cog(profile.base_builder(opts), opts, profile.width, profile.height);
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
