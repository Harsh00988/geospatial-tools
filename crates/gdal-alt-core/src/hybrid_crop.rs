use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{remux_compress_tile, RemuxCompressedBlock, RemuxTileEncoding};
use ndarray::{Array2, Array3};
use rayon::prelude::*;
use tiff_core::{PlanarConfiguration, Predictor};
use tiff_reader::Ifd;

use crate::cog::tile_payload::ifd_planar;
use crate::cog::{overview_levels, tile_jobs, CogOutputOptions, TileJob};
use crate::crop::WriteWindow;
use crate::input::RasterProfile;
use crate::remux::layer_ifd;

struct HybridTileContext<'a> {
    input: &'a GeoTiffFile,
    source_layer: &'a [RemuxCompressedBlock],
    layer_index: usize,
    ifd: &'a Ifd,
    src_win: &'a WriteWindow,
    tile_size: usize,
    encoding: RemuxTileEncoding,
}

struct HybridLayerParams<'a> {
    input: &'a GeoTiffFile,
    source_layer: &'a [RemuxCompressedBlock],
    layer_index: usize,
    ifd: &'a Ifd,
    src_win: WriteWindow,
    opts: &'a CogOutputOptions,
    tile_size: usize,
    bands: usize,
}

struct HybridCropBuild<'a> {
    input: &'a GeoTiffFile,
    window: &'a WriteWindow,
    profile: &'a RasterProfile,
    opts: &'a CogOutputOptions,
    output_levels: &'a [u32],
    tile_size: usize,
    planar: bool,
    bands: usize,
}

pub fn build_hybrid_crop_layers_u8(
    input: &GeoTiffFile,
    source_layers: &[Vec<RemuxCompressedBlock>],
    window: &WriteWindow,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
) -> Result<Vec<Vec<RemuxCompressedBlock>>> {
    let base_ifd = input.tiff().ifd(input.base_ifd_index())?;
    let tile_size = base_ifd.tile_width().unwrap_or(opts.blocksize) as usize;
    let planar = ifd_planar(base_ifd) == PlanarConfiguration::Planar;
    let bands = profile.bands as usize;
    let output_levels = overview_levels(opts, profile.width, profile.height);
    let spec = HybridCropBuild {
        input,
        window,
        profile,
        opts,
        output_levels: &output_levels,
        tile_size,
        planar,
        bands,
    };

    let mut layers = Vec::with_capacity(1 + output_levels.len());
    layers.push(build_hybrid_crop_layer_u8(&spec, &source_layers[0], 0, 0)?);
    for (out_ov, source_layer) in source_layers.iter().skip(1).enumerate() {
        if out_ov >= output_levels.len() {
            break;
        }
        layers.push(build_hybrid_crop_layer_u8(
            &spec,
            source_layer,
            out_ov + 1,
            out_ov + 1,
        )?);
    }
    Ok(layers)
}

fn build_hybrid_crop_layer_u8(
    spec: &HybridCropBuild<'_>,
    source_layer: &[RemuxCompressedBlock],
    source_layer_index: usize,
    output_layer_index: usize,
) -> Result<Vec<RemuxCompressedBlock>> {
    let ifd = layer_ifd(spec.input, source_layer_index)?;
    let (out_w, out_h) =
        output_layer_size(spec.profile.width, spec.profile.height, output_layer_index, spec.output_levels);
    let src_win = if output_layer_index == 0 {
        *spec.window
    } else {
        scale_window(spec.window, spec.output_levels[output_layer_index - 1])
    };
    let jobs = tile_jobs(out_w, out_h, spec.tile_size as u32);
    let params = HybridLayerParams {
        input: spec.input,
        source_layer,
        layer_index: source_layer_index,
        ifd,
        src_win,
        opts: spec.opts,
        tile_size: spec.tile_size,
        bands: spec.bands,
    };

    if spec.planar {
        build_planar_layer_u8(&params, &jobs)
    } else {
        build_chunky_layer_u8(&params, &jobs)
    }
}

fn build_planar_layer_u8(
    params: &HybridLayerParams<'_>,
    jobs: &[TileJob],
) -> Result<Vec<RemuxCompressedBlock>> {
    let encoding = tile_encoding(params.ifd, params.opts, params.tile_size, 1);
    let ctx = HybridTileContext {
        input: params.input,
        source_layer: params.source_layer,
        layer_index: params.layer_index,
        ifd: params.ifd,
        src_win: &params.src_win,
        tile_size: params.tile_size,
        encoding,
    };

    let mut work: Vec<(usize, usize, TileJob)> = Vec::with_capacity(jobs.len() * params.bands);
    for (tile_idx, job) in jobs.iter().copied().enumerate() {
        for band in 0..params.bands {
            let block_index = band * jobs.len() + tile_idx;
            work.push((block_index, band, job));
        }
    }

    let mut blocks = work
        .par_iter()
        .map(|(block_index, band, job)| {
            let block = encode_planar_tile(&ctx, *band, job, *block_index)?;
            Ok((*block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

fn build_chunky_layer_u8(
    params: &HybridLayerParams<'_>,
    jobs: &[TileJob],
) -> Result<Vec<RemuxCompressedBlock>> {
    let encoding = tile_encoding(
        params.ifd,
        params.opts,
        params.tile_size,
        params.bands as u16,
    );
    let ctx = HybridTileContext {
        input: params.input,
        source_layer: params.source_layer,
        layer_index: params.layer_index,
        ifd: params.ifd,
        src_win: &params.src_win,
        tile_size: params.tile_size,
        encoding,
    };

    let mut blocks = jobs
        .par_iter()
        .enumerate()
        .map(|(block_index, job)| {
            let block = encode_chunky_tile(&ctx, job, params.bands, block_index)?;
            Ok((block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

fn encode_planar_tile(
    ctx: &HybridTileContext<'_>,
    band: usize,
    job: &TileJob,
    block_index: usize,
) -> Result<RemuxCompressedBlock> {
    let src_col = ctx.src_win.col_off + job.col_off;
    let src_row = ctx.src_win.row_off + job.row_off;

    if can_copy_whole_tile(
        src_col,
        src_row,
        job.cols,
        job.rows,
        ctx.tile_size,
        ctx.ifd,
    ) {
        let src_idx = source_planar_block_index(ctx.ifd, src_col, src_row, band, ctx.tile_size);
        return ctx
            .source_layer
            .get(src_idx)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing source tile {src_idx}"));
    }

    let data = read_planar_window_u8(
        ctx.input,
        ctx.layer_index,
        band,
        src_row,
        src_col,
        job.rows,
        job.cols,
    )
    .with_context(|| format!("failed to read band {band} window at ({src_col},{src_row})"))?;
    let padded = pad_tile_2d_u8(&data, job.rows, job.cols, ctx.tile_size);
    remux_compress_tile(&padded, block_index, ctx.encoding).map_err(|err| anyhow::anyhow!(err))
}

fn encode_chunky_tile(
    ctx: &HybridTileContext<'_>,
    job: &TileJob,
    bands: usize,
    block_index: usize,
) -> Result<RemuxCompressedBlock> {
    let src_col = ctx.src_win.col_off + job.col_off;
    let src_row = ctx.src_win.row_off + job.row_off;

    if can_copy_whole_tile(
        src_col,
        src_row,
        job.cols,
        job.rows,
        ctx.tile_size,
        ctx.ifd,
    ) {
        let src_idx = source_chunky_block_index(ctx.ifd, src_col, src_row, ctx.tile_size);
        return ctx
            .source_layer
            .get(src_idx)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing source tile {src_idx}"));
    }

    let data = read_chunky_window_u8(
        ctx.input,
        ctx.layer_index,
        src_row,
        src_col,
        job.rows,
        job.cols,
    )
    .with_context(|| format!("failed to read window at ({src_col},{src_row})"))?;
    let padded = pad_tile_chunky_u8(&data, job.rows, job.cols, bands, ctx.tile_size);
    remux_compress_tile(&padded, block_index, ctx.encoding).map_err(|err| anyhow::anyhow!(err))
}

fn read_planar_window_u8(
    input: &GeoTiffFile,
    layer_index: usize,
    band: usize,
    src_row: usize,
    src_col: usize,
    rows: usize,
    cols: usize,
) -> Result<Array2<u8>> {
    let data = if layer_index == 0 {
        input.read_band_window::<u8>(band, src_row, src_col, rows, cols)?
    } else {
        input.read_overview_band_window::<u8>(
            layer_index - 1,
            band,
            src_row,
            src_col,
            rows,
            cols,
        )?
    };
    data.into_dimensionality::<ndarray::Ix2>()
        .context("expected 2D band window")
}

fn read_chunky_window_u8(
    input: &GeoTiffFile,
    layer_index: usize,
    src_row: usize,
    src_col: usize,
    rows: usize,
    cols: usize,
) -> Result<Array3<u8>> {
    let data = if layer_index == 0 {
        input.read_window::<u8>(src_row, src_col, rows, cols)?
    } else {
        input.read_overview_window::<u8>(layer_index - 1, src_row, src_col, rows, cols)?
    };
    data.into_dimensionality::<ndarray::Ix3>()
        .context("expected [rows, cols, bands] window")
}

fn can_copy_whole_tile(
    src_col: usize,
    src_row: usize,
    cols: usize,
    rows: usize,
    tile_size: usize,
    ifd: &Ifd,
) -> bool {
    cols == tile_size
        && rows == tile_size
        && src_col.is_multiple_of(tile_size)
        && src_row.is_multiple_of(tile_size)
        && src_col + tile_size <= ifd.width() as usize
        && src_row + tile_size <= ifd.height() as usize
}

fn source_planar_block_index(
    ifd: &Ifd,
    src_col: usize,
    src_row: usize,
    band: usize,
    tile_size: usize,
) -> usize {
    let tiles_across = (ifd.width() as usize).div_ceil(tile_size);
    let tile_col = src_col / tile_size;
    let tile_row = src_row / tile_size;
    let tiles_per_plane = tiles_across * (ifd.height() as usize).div_ceil(tile_size);
    band * tiles_per_plane + tile_row * tiles_across + tile_col
}

fn source_chunky_block_index(ifd: &Ifd, src_col: usize, src_row: usize, tile_size: usize) -> usize {
    let tiles_across = (ifd.width() as usize).div_ceil(tile_size);
    let tile_col = src_col / tile_size;
    let tile_row = src_row / tile_size;
    tile_row * tiles_across + tile_col
}

fn pad_tile_2d_u8(data: &Array2<u8>, rows: usize, cols: usize, tile_size: usize) -> Vec<u8> {
    let mut out = vec![0u8; tile_size * tile_size];
    for row in 0..rows {
        for col in 0..cols {
            out[row * tile_size + col] = data[[row, col]];
        }
    }
    out
}

fn pad_tile_chunky_u8(
    data: &Array3<u8>,
    rows: usize,
    cols: usize,
    bands: usize,
    tile_size: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; tile_size * tile_size * bands];
    for row in 0..rows {
        for col in 0..cols {
            for band in 0..bands {
                out[(row * tile_size + col) * bands + band] = data[[row, col, band]];
            }
        }
    }
    out
}

fn tile_encoding(ifd: &Ifd, opts: &CogOutputOptions, tile_size: usize, spp: u16) -> RemuxTileEncoding {
    RemuxTileEncoding {
        compression: opts.compression.to_compression(),
        predictor: Predictor::from_code(ifd.predictor()).unwrap_or(Predictor::None),
        samples_per_pixel: spp,
        tile_width: tile_size,
        tile_height: tile_size as u32,
        deflate_level: opts.deflate_level,
    }
}

fn output_layer_size(
    crop_width: u32,
    crop_height: u32,
    layer_index: usize,
    levels: &[u32],
) -> (u32, u32) {
    if layer_index == 0 {
        return (crop_width, crop_height);
    }
    let scale = levels[layer_index - 1];
    (
        crop_width.div_ceil(scale),
        crop_height.div_ceil(scale),
    )
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
