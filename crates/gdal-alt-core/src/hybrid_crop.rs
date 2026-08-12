use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{remux_compress_tile, RemuxCompressedBlock, RemuxTileEncoding};
use ndarray::{Array2, Array3};
use rayon::prelude::*;
use tiff_core::{PlanarConfiguration, Predictor};
use tiff_reader::{Ifd, TiffSample};

use crate::cog::tile_payload::{ifd_planar, read_layer_block_at};
use crate::cog::{tile_jobs, CogOutputOptions, TileJob};
use crate::crop::WriteWindow;
use crate::input::RasterProfile;
use crate::remux::{input_overview_levels, layer_ifd};

struct HybridTileContext<'a> {
    input: &'a GeoTiffFile,
    tiff: &'a tiff_reader::TiffFile,
    layer_index: usize,
    ifd: &'a Ifd,
    src_win: &'a WriteWindow,
    tile_size: usize,
    encoding: RemuxTileEncoding,
}

struct HybridLayerParams<'a> {
    input: &'a GeoTiffFile,
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

pub fn build_hybrid_crop_layers<T>(
    input: &GeoTiffFile,
    window: &WriteWindow,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    output_levels: &[u32],
) -> Result<Vec<Vec<RemuxCompressedBlock>>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let base_ifd = input.tiff().ifd(input.base_ifd_index())?;
    let tile_size = base_ifd.tile_width().unwrap_or(opts.blocksize) as usize;
    let bands = profile.bands as usize;
    let planar = ifd_planar(base_ifd) == PlanarConfiguration::Planar || bands == 1;
    let source_factors = input_overview_levels(input)?;
    let spec = HybridCropBuild {
        input,
        window,
        profile,
        opts,
        output_levels,
        tile_size,
        planar,
        bands,
    };

    let base = build_hybrid_crop_layer::<T>(&spec, 0, 0)?;

    let overviews: Result<Vec<_>> = output_levels
        .par_iter()
        .enumerate()
        .map(|(out_idx, &level)| {
            build_crop_overview_layer::<T>(
                &spec,
                &source_factors,
                out_idx,
                level,
            )
        })
        .collect();

    let mut layers = Vec::with_capacity(1 + output_levels.len());
    layers.push(base);
    layers.extend(overviews?);
    Ok(layers)
}

fn build_crop_overview_layer<T>(
    spec: &HybridCropBuild<'_>,
    source_factors: &[u32],
    out_idx: usize,
    level: u32,
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let output_layer_index = out_idx + 1;

    if let Some(src_ov) = source_factors.iter().position(|&factor| factor == level) {
        if crop_level_fits(spec.input, spec.window, level, src_ov)? {
            return build_hybrid_crop_layer::<T>(spec, src_ov + 1, output_layer_index);
        }
    }

    if let Some((src_ov, src_factor)) = best_source_overview(source_factors, level) {
        if crop_level_fits(spec.input, spec.window, src_factor, src_ov)? {
            let downsample = (src_factor / level).max(1) as usize;
            let base_ifd = spec.input.tiff().ifd(spec.input.base_ifd_index())?;
            let (out_w, out_h) = output_layer_size(
                spec.profile.width,
                spec.profile.height,
                output_layer_index,
                spec.output_levels,
            );
            let jobs = tile_jobs(out_w, out_h, spec.tile_size as u32);
            let encoding = tile_encoding(
                base_ifd,
                spec.opts,
                spec.tile_size,
                if spec.planar { 1 } else { spec.bands as u16 },
            );
            let src_win = scale_window(spec.window, src_factor);
            return if spec.planar {
                build_generated_planar_layer::<T>(
                    spec.input,
                    &jobs,
                    &src_win,
                    src_ov + 1,
                    level,
                    src_factor,
                    spec.tile_size,
                    spec.bands,
                    encoding,
                    spec.opts,
                )
            } else {
                build_generated_chunky_layer::<T>(
                    spec.input,
                    &jobs,
                    &src_win,
                    src_ov + 1,
                    level,
                    src_factor,
                    spec.tile_size,
                    spec.bands,
                    encoding,
                    spec.opts,
                )
            };
        }
    }

    let parent_level = if out_idx > 0 {
        spec.output_levels[out_idx - 1]
    } else {
        1
    };
    let base_ifd = spec.input.tiff().ifd(spec.input.base_ifd_index())?;
    let (out_w, out_h) = output_layer_size(
        spec.profile.width,
        spec.profile.height,
        output_layer_index,
        spec.output_levels,
    );
    let jobs = tile_jobs(out_w, out_h, spec.tile_size as u32);
    let encoding = tile_encoding(
        base_ifd,
        spec.opts,
        spec.tile_size,
        if spec.planar { 1 } else { spec.bands as u16 },
    );
    let (source_layer_index, src_win, source_factor) = if out_idx > 0 {
        if let Some(src_ov) = source_factors.iter().position(|&factor| factor == parent_level) {
            (src_ov + 1, scale_window(spec.window, parent_level), parent_level)
        } else {
            (0, *spec.window, 1)
        }
    } else {
        (0, *spec.window, 1)
    };

    if spec.planar {
        build_generated_planar_layer::<T>(
            spec.input,
            &jobs,
            &src_win,
            source_layer_index,
            level,
            source_factor,
            spec.tile_size,
            spec.bands,
            encoding,
            spec.opts,
        )
    } else {
        build_generated_chunky_layer::<T>(
            spec.input,
            &jobs,
            &src_win,
            source_layer_index,
            level,
            source_factor,
            spec.tile_size,
            spec.bands,
            encoding,
            spec.opts,
        )
    }
}

fn best_source_overview(source_factors: &[u32], target_level: u32) -> Option<(usize, u32)> {
    source_factors
        .iter()
        .enumerate()
        .filter(|(_, factor)| **factor >= target_level)
        .min_by_key(|(_, factor)| *factor)
        .map(|(index, &factor)| (index, factor))
}

fn crop_level_fits(
    input: &GeoTiffFile,
    window: &WriteWindow,
    level: u32,
    source_overview_index: usize,
) -> Result<bool> {
    let ifd = layer_ifd(input, source_overview_index + 1)?;
    let scaled = scale_window(window, level);
    let ifd_w = ifd.width() as usize;
    let ifd_h = ifd.height() as usize;
    Ok(scaled.col_off < ifd_w
        && scaled.row_off < ifd_h
        && scaled.col_off + scaled.width <= ifd_w
        && scaled.row_off + scaled.height <= ifd_h)
}

fn generated_read_region(
    src_win: &WriteWindow,
    job: &TileJob,
    target_level: u32,
    source_factor: u32,
) -> (usize, usize, usize, usize, usize) {
    let target = target_level as usize;
    let source = source_factor.max(1) as usize;
    let src_col = src_win.col_off + job.col_off * target / source;
    let src_row = src_win.row_off + job.row_off * target / source;
    let src_cols = job.cols * target / source;
    let src_rows = job.rows * target / source;
    let downsample = (source / target).max(1);
    (src_col, src_row, src_cols, src_rows, downsample)
}

fn build_generated_planar_layer<T>(
    input: &GeoTiffFile,
    jobs: &[TileJob],
    src_win: &WriteWindow,
    source_layer_index: usize,
    target_level: u32,
    source_factor: u32,
    tile_size: usize,
    bands: usize,
    encoding: RemuxTileEncoding,
    opts: &CogOutputOptions,
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let mut work: Vec<(usize, usize, TileJob)> = Vec::with_capacity(jobs.len() * bands);
    for (tile_idx, job) in jobs.iter().copied().enumerate() {
        for band in 0..bands {
            work.push((band * jobs.len() + tile_idx, band, job));
        }
    }

    let mut blocks = work
        .par_iter()
        .map(|(block_index, band, job)| {
            let (src_col, src_row, src_cols, src_rows, downsample) =
                generated_read_region(src_win, job, target_level, source_factor);
            let data = read_planar_window::<T>(
                input, source_layer_index, *band, src_row, src_col, src_rows, src_cols,
            )?;
            let downsampled = downsample_planar_tile(&data, job.rows, job.cols, downsample, opts)?;
            let padded = pad_tile_2d(&downsampled, job.rows, job.cols, tile_size);
            let block = remux_compress_tile(&padded, *block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            Ok((*block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

fn build_generated_chunky_layer<T>(
    input: &GeoTiffFile,
    jobs: &[TileJob],
    src_win: &WriteWindow,
    source_layer_index: usize,
    target_level: u32,
    source_factor: u32,
    tile_size: usize,
    bands: usize,
    encoding: RemuxTileEncoding,
    opts: &CogOutputOptions,
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let mut blocks = jobs
        .par_iter()
        .enumerate()
        .map(|(block_index, job)| {
            let (src_col, src_row, src_cols, src_rows, downsample) =
                generated_read_region(src_win, job, target_level, source_factor);
            let data = read_chunky_window::<T>(
                input, source_layer_index, src_row, src_col, src_rows, src_cols,
            )?;
            let downsampled = downsample_chunky_tile(&data, job.rows, job.cols, downsample, opts)?;
            let padded = pad_tile_chunky(&downsampled, job.rows, job.cols, bands, tile_size);
            let block = remux_compress_tile(&padded, block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            Ok((block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

fn downsample_planar_tile<T>(
    src: &Array2<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
    opts: &CogOutputOptions,
) -> Result<Array2<T>>
where
    T: geotiff_writer::NumericSample + Copy + Default,
{
    Ok(match opts.resampling {
        crate::cog::ResamplingChoice::Nearest => nearest_downsample_2d(src, out_rows, out_cols, scale),
        crate::cog::ResamplingChoice::Average => average_downsample_2d(src, out_rows, out_cols, scale),
    })
}

fn downsample_chunky_tile<T>(
    src: &Array3<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
    opts: &CogOutputOptions,
) -> Result<Array3<T>>
where
    T: geotiff_writer::NumericSample + Copy + Default,
{
    Ok(match opts.resampling {
        crate::cog::ResamplingChoice::Nearest => nearest_downsample_3d(src, out_rows, out_cols, scale),
        crate::cog::ResamplingChoice::Average => average_downsample_3d(src, out_rows, out_cols, scale),
    })
}

fn nearest_downsample_2d<T: Copy + Default>(
    src: &Array2<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
) -> Array2<T> {
    let mut out = Array2::default((out_rows, out_cols));
    let src_rows = src.shape()[0];
    let src_cols = src.shape()[1];
    for row in 0..out_rows {
        for col in 0..out_cols {
            let src_row = (row * scale).min(src_rows.saturating_sub(1));
            let src_col = (col * scale).min(src_cols.saturating_sub(1));
            out[[row, col]] = src[[src_row, src_col]];
        }
    }
    out
}

fn nearest_downsample_3d<T: Copy + Default>(
    src: &Array3<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
) -> Array3<T> {
    let bands = src.shape()[2];
    let mut out = Array3::default((out_rows, out_cols, bands));
    let src_rows = src.shape()[0];
    let src_cols = src.shape()[1];
    for row in 0..out_rows {
        for col in 0..out_cols {
            let src_row = (row * scale).min(src_rows.saturating_sub(1));
            let src_col = (col * scale).min(src_cols.saturating_sub(1));
            for band in 0..bands {
                out[[row, col, band]] = src[[src_row, src_col, band]];
            }
        }
    }
    out
}

fn average_downsample_2d<T>(
    src: &Array2<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
) -> Array2<T>
where
    T: geotiff_writer::NumericSample + Copy + Default,
{
    let mut out = Array2::default((out_rows, out_cols));
    for row in 0..out_rows {
        for col in 0..out_cols {
            out[[row, col]] = average_block_2d(src, row, col, scale);
        }
    }
    out
}

fn average_downsample_3d<T>(
    src: &Array3<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
) -> Array3<T>
where
    T: geotiff_writer::NumericSample + Copy + Default,
{
    let bands = src.shape()[2];
    let mut out = Array3::default((out_rows, out_cols, bands));
    for row in 0..out_rows {
        for col in 0..out_cols {
            for band in 0..bands {
                out[[row, col, band]] = average_block_3d(src, row, col, band, scale);
            }
        }
    }
    out
}

fn average_block_2d<T>(src: &Array2<T>, out_row: usize, out_col: usize, scale: usize) -> T
where
    T: geotiff_writer::NumericSample + Copy + Default,
{
    let mut sum = 0.0f64;
    let mut count = 0u32;
    let src_rows = src.shape()[0];
    let src_cols = src.shape()[1];
    for dr in 0..scale {
        for dc in 0..scale {
            let r = out_row * scale + dr;
            let c = out_col * scale + dc;
            if r < src_rows && c < src_cols {
                sum += src[[r, c]].to_f64();
                count += 1;
            }
        }
    }
    T::from_f64(sum / f64::from(count.max(1)))
}

fn average_block_3d<T>(
    src: &Array3<T>,
    out_row: usize,
    out_col: usize,
    band: usize,
    scale: usize,
) -> T
where
    T: geotiff_writer::NumericSample + Copy + Default,
{
    let mut sum = 0.0f64;
    let mut count = 0u32;
    let src_rows = src.shape()[0];
    let src_cols = src.shape()[1];
    for dr in 0..scale {
        for dc in 0..scale {
            let r = out_row * scale + dr;
            let c = out_col * scale + dc;
            if r < src_rows && c < src_cols {
                sum += src[[r, c, band]].to_f64();
                count += 1;
            }
        }
    }
    T::from_f64(sum / f64::from(count.max(1)))
}

fn build_hybrid_crop_layer<T>(
    spec: &HybridCropBuild<'_>,
    source_layer_index: usize,
    output_layer_index: usize,
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
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
        layer_index: source_layer_index,
        ifd,
        src_win,
        opts: spec.opts,
        tile_size: spec.tile_size,
        bands: spec.bands,
    };

    if spec.planar {
        build_planar_layer::<T>(&params, &jobs)
    } else {
        build_chunky_layer::<T>(&params, &jobs)
    }
}

fn build_planar_layer<T>(
    params: &HybridLayerParams<'_>,
    jobs: &[TileJob],
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let encoding = tile_encoding(params.ifd, params.opts, params.tile_size, 1);
    let ctx = HybridTileContext {
        input: params.input,
        tiff: params.input.tiff(),
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
            let block = encode_planar_tile::<T>(&ctx, *band, job, *block_index)?;
            Ok((*block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

fn build_chunky_layer<T>(
    params: &HybridLayerParams<'_>,
    jobs: &[TileJob],
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let encoding = tile_encoding(
        params.ifd,
        params.opts,
        params.tile_size,
        params.bands as u16,
    );
    let ctx = HybridTileContext {
        input: params.input,
        tiff: params.input.tiff(),
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
            let block = encode_chunky_tile::<T>(&ctx, job, params.bands, block_index)?;
            Ok((block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

fn encode_planar_tile<T>(
    ctx: &HybridTileContext<'_>,
    band: usize,
    job: &TileJob,
    block_index: usize,
) -> Result<RemuxCompressedBlock>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
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
        return read_layer_block_at(ctx.tiff, ctx.ifd, src_idx)
            .with_context(|| format!("failed to read source tile {src_idx}"));
    }

    let data = read_planar_window::<T>(
        ctx.input,
        ctx.layer_index,
        band,
        src_row,
        src_col,
        job.rows,
        job.cols,
    )
    .with_context(|| format!("failed to read band {band} window at ({src_col},{src_row})"))?;
    let padded = pad_tile_2d(&data, job.rows, job.cols, ctx.tile_size);
    remux_compress_tile(&padded, block_index, ctx.encoding).map_err(|err| anyhow::anyhow!(err))
}

fn encode_chunky_tile<T>(
    ctx: &HybridTileContext<'_>,
    job: &TileJob,
    bands: usize,
    block_index: usize,
) -> Result<RemuxCompressedBlock>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
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
        return read_layer_block_at(ctx.tiff, ctx.ifd, src_idx)
            .with_context(|| format!("failed to read source tile {src_idx}"));
    }

    let data = read_chunky_window::<T>(
        ctx.input,
        ctx.layer_index,
        src_row,
        src_col,
        job.rows,
        job.cols,
    )
    .with_context(|| format!("failed to read window at ({src_col},{src_row})"))?;
    let padded = pad_tile_chunky(&data, job.rows, job.cols, bands, ctx.tile_size);
    remux_compress_tile(&padded, block_index, ctx.encoding).map_err(|err| anyhow::anyhow!(err))
}

fn read_planar_window<T>(
    input: &GeoTiffFile,
    layer_index: usize,
    band: usize,
    src_row: usize,
    src_col: usize,
    rows: usize,
    cols: usize,
) -> Result<Array2<T>>
where
    T: TiffSample,
{
    let data = if layer_index == 0 {
        input.read_band_window::<T>(band, src_row, src_col, rows, cols)?
    } else {
        input.read_overview_band_window::<T>(
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

fn read_chunky_window<T>(
    input: &GeoTiffFile,
    layer_index: usize,
    src_row: usize,
    src_col: usize,
    rows: usize,
    cols: usize,
) -> Result<Array3<T>>
where
    T: TiffSample,
{
    let data = if layer_index == 0 {
        input.read_window::<T>(src_row, src_col, rows, cols)?
    } else {
        input.read_overview_window::<T>(layer_index - 1, src_row, src_col, rows, cols)?
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

fn pad_tile_2d<T: Copy + Default>(
    data: &Array2<T>,
    rows: usize,
    cols: usize,
    tile_size: usize,
) -> Vec<T> {
    let mut out = vec![T::default(); tile_size * tile_size];
    for row in 0..rows {
        for col in 0..cols {
            out[row * tile_size + col] = data[[row, col]];
        }
    }
    out
}

fn pad_tile_chunky<T: Copy + Default>(
    data: &Array3<T>,
    rows: usize,
    cols: usize,
    bands: usize,
    tile_size: usize,
) -> Vec<T> {
    let mut out = vec![T::default(); tile_size * tile_size * bands];
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
