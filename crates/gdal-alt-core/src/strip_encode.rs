use std::collections::BTreeMap;

use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{remux_compress_tile, RemuxCompressedBlock, RemuxTileEncoding};
use ndarray::{Array2, Array3, Axis, s};
use rayon::prelude::*;
use tiff_reader::TiffSample;

use crate::cog::{overview_levels, tile_jobs, CogOutputOptions, TileJob};
use crate::crop::WriteWindow;
use crate::encode_overview::encode_layers_with_spool;
use crate::input::RasterProfile;
use crate::progress::{ProgressTracker, StageBar};
use crate::remux::{remux_encoded_layers, resolve_overview_read_source};

pub fn convert_strip_to_remux_cog<T>(
    pool: &rayon::ThreadPool,
    input: &GeoTiffFile,
    output: &std::path::Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    show_progress: bool,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
{
    let nodata = crate::cog::semantics::parse_nodata::<T>(&profile.sample, &profile.nodata);
    let width = profile.width;
    let height = profile.height;
    let out_bands = profile.bands as usize;
    let tile_size = opts.blocksize as usize;
    let levels = overview_levels(opts, width, height);
    let encoding = output_tile_encoding(opts, tile_size, out_bands as u16);
    let progress = ProgressTracker::new(show_progress);
    let encode_total = encode_row_group_total(width, height, tile_size, &levels);
    let encode_bar = progress.stage("Encode tiles", encode_total);

    let layers = pool.install(|| {
        encode_layers_with_spool::<T>(
            input,
            width,
            height,
            tile_size,
            out_bands,
            window,
            band_map,
            encoding,
            opts,
            &levels,
            nodata,
            Some(&encode_bar),
        )
    })?;

    encode_bar.done("done");
    remux_encoded_layers(
        input,
        profile,
        opts,
        layers,
        output,
        window.as_ref(),
        Some(levels),
        None,
        show_progress,
    )?;
    progress.finish();
    Ok(())
}

pub(crate) fn encode_row_group_total(
    width: u32,
    height: u32,
    tile_size: usize,
    levels: &[u32],
) -> u64 {
    let mut total = row_group_count(width, height, tile_size);
    for &level in levels {
        let ov_w = (width / level).max(1);
        let ov_h = (height / level).max(1);
        total += row_group_count(ov_w, ov_h, tile_size);
    }
    total
}

fn row_group_count(width: u32, height: u32, tile_size: usize) -> u64 {
    (height as usize).div_ceil(tile_size) as u64
}

fn strip_rows_per_strip(input: &GeoTiffFile) -> Result<usize> {
    let ifd = input.tiff().ifd(input.base_ifd_index())?;
    Ok(ifd.rows_per_strip().max(1) as usize)
}

/// When each TIFF strip spans more than one tile row, decode each strip once and
/// slice tiles from the decoded buffer instead of re-decompressing per row group.
fn should_decode_strips_once(rows_per_strip: usize, tile_size: usize, window: Option<WriteWindow>) -> bool {
    window.is_none() && rows_per_strip > tile_size
}

fn build_base_layer_from_decoded_strips<T>(
    input: &GeoTiffFile,
    width: u32,
    height: u32,
    tile_size: usize,
    rows_per_strip: usize,
    out_bands: usize,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    progress: Option<&StageBar>,
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + Clone,
{
    let image_height = height as usize;
    let image_width = width as usize;
    let tiles = tile_jobs(width, height, tile_size as u32);
    let tile_index_by_pos: std::collections::HashMap<(usize, usize), usize> = tiles
        .iter()
        .enumerate()
        .map(|(idx, job)| ((job.col_off, job.row_off), idx))
        .collect();

    let strip_count = image_height.div_ceil(rows_per_strip);
    let mut jobs_by_strip: BTreeMap<usize, Vec<TileJob>> = BTreeMap::new();
    for job in tiles {
        let strip_idx = job.row_off / rows_per_strip;
        jobs_by_strip.entry(strip_idx).or_default().push(job);
    }

    let mut blocks = (0..strip_count)
        .into_par_iter()
        .map(|strip_idx| -> Result<Vec<(usize, RemuxCompressedBlock)>> {
            let row_start = strip_idx * rows_per_strip;
            let row_count = rows_per_strip.min(image_height.saturating_sub(row_start));
            let strip_tile = read_decoded_strip::<T>(
                input,
                row_start,
                0,
                row_count,
                image_width,
                out_bands,
                band_map,
            )?;

            let jobs = jobs_by_strip
                .get(&strip_idx)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let out = jobs
                .par_iter()
                .map(|job| {
                    let tile = slice_strip_tile(
                        &strip_tile,
                        job,
                        row_start,
                        out_bands,
                    )?;
                    let block_index = tile_index_by_pos[&(job.col_off, job.row_off)];
                    let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
                    let block = remux_compress_tile(&samples, block_index, encoding)
                        .map_err(|err| anyhow::anyhow!(err))?;
                    Ok((block_index, block))
                })
                .collect::<Result<Vec<_>>>()?;

            if let Some(bar) = progress {
                bar.inc(1);
            }
            Ok(out)
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

fn read_decoded_strip<T>(
    input: &GeoTiffFile,
    row_start: usize,
    col_start: usize,
    row_count: usize,
    col_count: usize,
    out_bands: usize,
    band_map: Option<&[usize]>,
) -> Result<StripTile<T>>
where
    T: TiffSample + Clone,
{
    if out_bands == 1 {
        let band_index = band_map.map(|bands| bands[0] - 1).unwrap_or(0);
        let data = input.read_band_window::<T>(
            band_index,
            row_start,
            col_start,
            row_count,
            col_count,
        )?;
        let data = data
            .into_dimensionality::<ndarray::Ix2>()
            .context("expected 2D decoded strip")?;
        Ok(StripTile::Single(data))
    } else {
        let data = input.read_window::<T>(row_start, col_start, row_count, col_count)?;
        let data = data
            .into_dimensionality::<ndarray::Ix3>()
            .context("expected [rows, cols, bands] decoded strip")?;
        let data = if let Some(bands) = band_map {
            select_bands(&data, bands)?
        } else {
            data
        };
        Ok(StripTile::Multi(data))
    }
}

fn slice_strip_tile<T: Clone>(
    strip: &StripTile<T>,
    job: &TileJob,
    strip_row_start: usize,
    out_bands: usize,
) -> Result<StripTile<T>> {
    let rel_row = job.row_off.saturating_sub(strip_row_start);
    match strip {
        StripTile::Single(data) => {
            let tile = data
                .slice(s![rel_row..rel_row + job.rows, job.col_off..job.col_off + job.cols])
                .to_owned();
            Ok(StripTile::Single(tile))
        }
        StripTile::Multi(data) => {
            let tile = data
                .slice(s![
                    rel_row..rel_row + job.rows,
                    job.col_off..job.col_off + job.cols,
                    ..
                ])
                .to_owned();
            if tile.len_of(Axis(2)) != out_bands {
                anyhow::bail!("unexpected band count in strip tile slice");
            }
            Ok(StripTile::Multi(tile))
        }
    }
}

fn build_strip_base_layer<T>(
    input: &GeoTiffFile,
    width: u32,
    height: u32,
    tile_size: usize,
    out_bands: usize,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    progress: Option<&StageBar>,
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    build_base_layer_from_rows::<T>(
        input,
        width,
        height,
        tile_size,
        out_bands,
        window,
        band_map,
        encoding,
        progress,
    )
}

pub(crate) fn build_base_layer_from_rows<T>(
    input: &GeoTiffFile,
    width: u32,
    height: u32,
    tile_size: usize,
    out_bands: usize,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    progress: Option<&StageBar>,
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let rows_per_strip = strip_rows_per_strip(input)?;
    if should_decode_strips_once(rows_per_strip, tile_size, window) {
        return build_base_layer_from_decoded_strips::<T>(
            input,
            width,
            height,
            tile_size,
            rows_per_strip,
            out_bands,
            band_map,
            encoding,
            progress,
        );
    }

    let tiles = tile_jobs(width, height, tile_size as u32);
    let tile_index_by_pos: std::collections::HashMap<(usize, usize), usize> = tiles
        .iter()
        .enumerate()
        .map(|(idx, job)| ((job.col_off, job.row_off), idx))
        .collect();
    let mut row_groups: BTreeMap<usize, Vec<TileJob>> = BTreeMap::new();
    for job in tiles {
        row_groups.entry(job.row_off).or_default().push(job);
    }

    let mut blocks = row_groups
        .par_iter()
        .map(|(&row_off, jobs)| -> Result<Vec<(usize, RemuxCompressedBlock)>> {
            let batch = read_strip_row_batch::<T>(input, out_bands, window, band_map, row_off, jobs)?;
            if let Some(bar) = progress {
                bar.inc(1);
            }
            batch
                .into_iter()
                .map(|(col_off, row_off, tile)| {
                    let block_index = tile_index_by_pos[&(col_off, row_off)];
                    let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
                    let block = remux_compress_tile(&samples, block_index, encoding)
                        .map_err(|err| anyhow::anyhow!(err))?;
                    Ok((block_index, block))
                })
                .collect()
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

pub(crate) fn build_strip_overview_layer_with_cache<T>(
    input: &GeoTiffFile,
    width: u32,
    height: u32,
    level: u32,
    tile_size: usize,
    out_bands: usize,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    opts: &CogOutputOptions,
    nodata: Option<T>,
    cache_decoded: bool,
    progress: Option<&StageBar>,
) -> Result<(Vec<RemuxCompressedBlock>, Vec<(usize, usize, StripTile<T>)>)>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
{
    let ov_width = (width / level).max(1);
    let ov_height = (height / level).max(1);
    let jobs = tile_jobs(ov_width, ov_height, tile_size as u32);
    let (source_layer, source_factor, downsample) = resolve_overview_read_source(input, level)?;
    let tile_index_by_pos: std::collections::HashMap<(usize, usize), usize> = jobs
        .iter()
        .enumerate()
        .map(|(idx, job)| ((job.col_off, job.row_off), idx))
        .collect();

    let mut row_groups: BTreeMap<usize, Vec<TileJob>> = BTreeMap::new();
    for job in jobs {
        row_groups.entry(job.row_off).or_default().push(job);
    }

    let mut blocks = row_groups
        .par_iter()
        .map(|(&row_off, row_jobs)| {
            if let Some(bar) = progress {
                bar.inc(1);
            }
            read_overview_row_batch::<T>(
                input,
                source_layer,
                source_factor,
                level,
                downsample,
                out_bands,
                window,
                band_map,
                row_off,
                row_jobs,
                width as usize,
                height as usize,
                tile_size,
                encoding,
                opts,
                nodata,
                &tile_index_by_pos,
            )
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    blocks.sort_by_key(|(index, _, _, _, _)| *index);
    let mut compressed = Vec::with_capacity(blocks.len());
    let mut decoded_tiles = if cache_decoded {
        Vec::with_capacity(blocks.len())
    } else {
        Vec::new()
    };
    for (_, col_off, row_off, tile, block) in blocks {
        if cache_decoded {
            decoded_tiles.push((col_off, row_off, tile));
        }
        compressed.push(block);
    }
    Ok((compressed, decoded_tiles))
}

pub(crate) fn build_strip_overview_from_decoded<T>(
    parent_tiles: Vec<(usize, usize, StripTile<T>)>,
    width: u32,
    height: u32,
    level: u32,
    parent_level: u32,
    tile_size: usize,
    out_bands: usize,
    encoding: RemuxTileEncoding,
    opts: &CogOutputOptions,
    nodata: Option<T>,
    cache_decoded: bool,
    progress: Option<&StageBar>,
) -> Result<(Vec<RemuxCompressedBlock>, Vec<(usize, usize, StripTile<T>)>)>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
{
    let downsample = (level / parent_level).max(1) as usize;
    let ov_width = (width / level).max(1);
    let ov_height = (height / level).max(1);
    let jobs = tile_jobs(ov_width, ov_height, tile_size as u32);
    let parent_map: std::collections::HashMap<(usize, usize), StripTile<T>> = parent_tiles
        .into_iter()
        .map(|(col, row, tile)| ((col, row), tile))
        .collect();

    let mut blocks = jobs
        .par_iter()
        .enumerate()
        .map(|(block_index, job)| {
            if let Some(bar) = progress {
                bar.inc(1);
            }
            let tile = downsample_parent_tile::<T>(
                &parent_map,
                job,
                parent_level,
                downsample,
                out_bands,
                tile_size,
                opts,
                nodata,
            )?;
            let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
            let block = remux_compress_tile(&samples, block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            Ok((block_index, job.col_off, job.row_off, tile, block))
        })
        .collect::<Result<Vec<_>>>()?;

    blocks.sort_by_key(|(index, _, _, _, _)| *index);
    let mut compressed = Vec::with_capacity(blocks.len());
    let mut decoded_tiles = if cache_decoded {
        Vec::with_capacity(blocks.len())
    } else {
        Vec::new()
    };
    for (_, col_off, row_off, tile, block) in blocks {
        if cache_decoded {
            decoded_tiles.push((col_off, row_off, tile));
        }
        compressed.push(block);
    }
    Ok((compressed, decoded_tiles))
}

fn downsample_parent_tile<T>(
    parent_map: &std::collections::HashMap<(usize, usize), StripTile<T>>,
    job: &TileJob,
    _parent_level: u32,
    downsample: usize,
    out_bands: usize,
    tile_size: usize,
    opts: &CogOutputOptions,
    nodata: Option<T>,
) -> Result<StripTile<T>>
where
    T: geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
{
    let parent_col = job.col_off * downsample;
    let parent_row = job.row_off * downsample;
    let parent_cols = job.cols * downsample;
    let parent_rows = job.rows * downsample;

    let stitched = match out_bands {
        1 => {
            let mut canvas = Array2::default((parent_rows, parent_cols));
            for tile_col in (parent_col / tile_size * tile_size..parent_col + parent_cols)
                .step_by(tile_size)
            {
                for tile_row in (parent_row / tile_size * tile_size..parent_row + parent_rows)
                    .step_by(tile_size)
                {
                    let Some(tile) = parent_map.get(&(tile_col, tile_row)) else {
                        continue;
                    };
                    let StripTile::Single(data) = tile else {
                        anyhow::bail!("expected single-band parent tile");
                    };
                    copy_tile_region_2d(
                        data,
                        &mut canvas,
                        tile_col,
                        tile_row,
                        parent_col,
                        parent_row,
                        parent_cols,
                        parent_rows,
                    );
                }
            }
            StripTile::Single(downsample_2d(&canvas, job.rows, job.cols, downsample, opts, nodata)?)
        }
        _ => {
            let mut canvas = Array3::default((parent_rows, parent_cols, out_bands));
            for tile_col in (parent_col / tile_size * tile_size..parent_col + parent_cols)
                .step_by(tile_size)
            {
                for tile_row in (parent_row / tile_size * tile_size..parent_row + parent_rows)
                    .step_by(tile_size)
                {
                    let Some(tile) = parent_map.get(&(tile_col, tile_row)) else {
                        continue;
                    };
                    let StripTile::Multi(data) = tile else {
                        anyhow::bail!("expected multi-band parent tile");
                    };
                    copy_tile_region_3d(
                        data,
                        &mut canvas,
                        tile_col,
                        tile_row,
                        parent_col,
                        parent_row,
                        parent_cols,
                        parent_rows,
                    );
                }
            }
            StripTile::Multi(downsample_3d(&canvas, job.rows, job.cols, downsample, opts, nodata)?)
        }
    };
    Ok(stitched)
}

fn tile_width<T>(tile: &StripTile<T>) -> usize {
    match tile {
        StripTile::Single(data) => data.ncols(),
        StripTile::Multi(data) => data.shape()[1],
    }
}

fn tile_height<T>(tile: &StripTile<T>) -> usize {
    match tile {
        StripTile::Single(data) => data.nrows(),
        StripTile::Multi(data) => data.shape()[0],
    }
}

fn tile_size_for<T>(tile: &StripTile<T>) -> usize {
    tile_width(tile).max(tile_height(tile))
}

fn copy_tile_region_2d<T: Copy>(
    tile: &Array2<T>,
    canvas: &mut Array2<T>,
    tile_col: usize,
    tile_row: usize,
    region_col: usize,
    region_row: usize,
    region_cols: usize,
    region_rows: usize,
) {
    let tile_rows = tile.nrows();
    let tile_cols = tile.ncols();
    for row in 0..tile_rows {
        let dst_row = tile_row + row;
        if dst_row < region_row || dst_row >= region_row + region_rows {
            continue;
        }
        for col in 0..tile_cols {
            let dst_col = tile_col + col;
            if dst_col < region_col || dst_col >= region_col + region_cols {
                continue;
            }
            canvas[[dst_row - region_row, dst_col - region_col]] = tile[[row, col]];
        }
    }
}

fn copy_tile_region_3d<T: Copy>(
    tile: &Array3<T>,
    canvas: &mut Array3<T>,
    tile_col: usize,
    tile_row: usize,
    region_col: usize,
    region_row: usize,
    region_cols: usize,
    region_rows: usize,
) {
    let tile_rows = tile.shape()[0];
    let tile_cols = tile.shape()[1];
    let bands = tile.len_of(Axis(2));
    for row in 0..tile_rows {
        let dst_row = tile_row + row;
        if dst_row < region_row || dst_row >= region_row + region_rows {
            continue;
        }
        for col in 0..tile_cols {
            let dst_col = tile_col + col;
            if dst_col < region_col || dst_col >= region_col + region_cols {
                continue;
            }
            for band in 0..bands {
                canvas[[dst_row - region_row, dst_col - region_col, band]] =
                    tile[[row, col, band]];
            }
        }
    }
}

fn overview_source_window(
    job: &TileJob,
    scale: usize,
    width: usize,
    height: usize,
    window: Option<WriteWindow>,
) -> (usize, usize, usize, usize) {
    let (base_col, base_row) = match window {
        Some(w) => (w.col_off, w.row_off),
        None => (0, 0),
    };
    let src_col = base_col + job.col_off * scale;
    let src_row = base_row + job.row_off * scale;
    let src_cols = (job.cols * scale).min(width.saturating_sub(src_col));
    let src_rows = (job.rows * scale).min(height.saturating_sub(src_row));
    (src_col, src_row, src_cols, src_rows)
}

fn read_overview_source<T>(
    input: &GeoTiffFile,
    out_bands: usize,
    band_map: Option<&[usize]>,
    src_row: usize,
    src_col: usize,
    src_rows: usize,
    src_cols: usize,
    scale: usize,
    out_rows: usize,
    out_cols: usize,
    opts: &CogOutputOptions,
    nodata: Option<T>,
) -> Result<StripTile<T>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
{
    if out_bands == 1 {
        let band_index = band_map.map(|bands| bands[0] - 1).unwrap_or(0);
        let data = input.read_band_window::<T>(band_index, src_row, src_col, src_rows, src_cols)?;
        let data = data
            .into_dimensionality::<ndarray::Ix2>()
            .context("expected 2D overview source")?;
        let downsampled = downsample_2d(&data, out_rows, out_cols, scale, opts, nodata)?;
        Ok(StripTile::Single(downsampled))
    } else {
        let data = input.read_window::<T>(src_row, src_col, src_rows, src_cols)?;
        let data = data
            .into_dimensionality::<ndarray::Ix3>()
            .context("expected [rows, cols, bands] overview source")?;
        let data = if let Some(bands) = band_map {
            select_bands(&data, bands)?
        } else {
            data
        };
        let downsampled = downsample_3d(&data, out_rows, out_cols, scale, opts, nodata)?;
        Ok(StripTile::Multi(downsampled))
    }
}

fn downsample_2d<T>(
    src: &Array2<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
    opts: &CogOutputOptions,
    nodata: Option<T>,
) -> Result<Array2<T>>
where
    T: geotiff_writer::NumericSample + Copy + Default + PartialEq,
{
    Ok(crate::resample::downsample_2d(
        src,
        out_rows,
        out_cols,
        scale,
        opts.resampling,
        nodata,
    ))
}

fn downsample_3d<T>(
    src: &Array3<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
    opts: &CogOutputOptions,
    nodata: Option<T>,
) -> Result<Array3<T>>
where
    T: geotiff_writer::NumericSample + Copy + Default + PartialEq,
{
    Ok(crate::resample::downsample_3d(
        src,
        out_rows,
        out_cols,
        scale,
        opts.resampling,
        nodata,
    ))
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
    let bands = src.len_of(Axis(2));
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

fn average_downsample_2d<T>(src: &Array2<T>, out_rows: usize, out_cols: usize, scale: usize) -> Array2<T>
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
    let bands = src.len_of(Axis(2));
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

pub(crate) enum StripTile<T> {
    Single(Array2<T>),
    Multi(Array3<T>),
}

fn encode_tile_samples<T>(
    tile: &StripTile<T>,
    tile_size: usize,
    out_bands: usize,
) -> Result<Vec<T>>
where
    T: Copy + Default,
{
    match tile {
        StripTile::Single(data) => {
            let rows = data.nrows();
            let cols = data.ncols();
            Ok(pad_tile_2d(data, rows, cols, tile_size))
        }
        StripTile::Multi(data) => {
            let rows = data.shape()[0];
            let cols = data.shape()[1];
            Ok(pad_tile_chunky(data, rows, cols, out_bands, tile_size))
        }
    }
}

fn overview_read_region(
    job: &TileJob,
    target_level: u32,
    source_factor: u32,
    window: Option<WriteWindow>,
) -> (usize, usize, usize, usize, usize) {
    let (base_col, base_row) = match window {
        Some(w) => (w.col_off, w.row_off),
        None => (0, 0),
    };
    let target = target_level as usize;
    let source = source_factor.max(1) as usize;
    let src_col = base_col + job.col_off * target / source;
    let src_row = base_row + job.row_off * target / source;
    let src_cols = job.cols * target / source;
    let src_rows = job.rows * target / source;
    let downsample = (source / target).max(1);
    (src_col, src_row, src_cols, src_rows, downsample)
}

fn read_overview_row_batch<T>(
    input: &GeoTiffFile,
    source_layer: usize,
    source_factor: u32,
    target_level: u32,
    layer_downsample: usize,
    out_bands: usize,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    row_off: usize,
    jobs: &[TileJob],
    width: usize,
    height: usize,
    tile_size: usize,
    encoding: RemuxTileEncoding,
    opts: &CogOutputOptions,
    nodata: Option<T>,
    tile_index_by_pos: &std::collections::HashMap<(usize, usize), usize>,
) -> Result<Vec<(usize, usize, usize, StripTile<T>, RemuxCompressedBlock)>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
{
    let mut out = Vec::with_capacity(jobs.len());
    if source_layer > 0 && layer_downsample == 1 {
        let rows = jobs[0].rows;
        let read_cols = jobs
            .iter()
            .map(|job| job.col_off + job.cols)
            .max()
            .unwrap_or(0);
        if out_bands == 1 {
            let band_index = band_map.map(|bands| bands[0] - 1).unwrap_or(0);
            let data = input.read_overview_band_window::<T>(
                source_layer - 1,
                band_index,
                row_off,
                0,
                rows,
                read_cols,
            )?;
            let data = data
                .into_dimensionality::<ndarray::Ix2>()
                .context("expected 2D overview row batch")?;
            for job in jobs {
                let tile = data
                    .slice(s![.., job.col_off..job.col_off + job.cols])
                    .to_owned();
                let strip_tile = StripTile::Single(tile);
                let block_index = tile_index_by_pos[&(job.col_off, job.row_off)];
                let samples = encode_tile_samples(&strip_tile, tile_size, out_bands)?;
                let block = remux_compress_tile(&samples, block_index, encoding)
                    .map_err(|err| anyhow::anyhow!(err))?;
                out.push((block_index, job.col_off, job.row_off, strip_tile, block));
            }
        } else {
            let data = input.read_overview_window::<T>(source_layer - 1, row_off, 0, rows, read_cols)?;
            let data = data
                .into_dimensionality::<ndarray::Ix3>()
                .context("expected [rows, cols, bands] overview row batch")?;
            for job in jobs {
                let slice = data.slice(s![.., job.col_off..job.col_off + job.cols, ..]);
                let tile_data = if let Some(bands) = band_map {
                    select_bands(&slice.to_owned(), bands)?
                } else {
                    slice.to_owned()
                };
                let strip_tile = StripTile::Multi(tile_data);
                let block_index = tile_index_by_pos[&(job.col_off, job.row_off)];
                let samples = encode_tile_samples(&strip_tile, tile_size, out_bands)?;
                let block = remux_compress_tile(&samples, block_index, encoding)
                    .map_err(|err| anyhow::anyhow!(err))?;
                out.push((block_index, job.col_off, job.row_off, strip_tile, block));
            }
        }
        return Ok(out);
    }

    for job in jobs {
        let block_index = tile_index_by_pos[&(job.col_off, job.row_off)];
        let tile = if source_layer == 0 {
            let scale = target_level as usize;
            let (src_col, src_row, src_cols, src_rows) =
                overview_source_window(job, scale, width, height, window);
            read_overview_source::<T>(
                input,
                out_bands,
                band_map,
                src_row,
                src_col,
                src_rows,
                src_cols,
                scale,
                job.rows,
                job.cols,
                opts,
                nodata,
            )?
        } else {
            let (src_col, src_row, src_cols, src_rows, downsample) =
                overview_read_region(job, target_level, source_factor, window);
            let raw = if out_bands == 1 {
                let band_index = band_map.map(|bands| bands[0] - 1).unwrap_or(0);
                let data = input.read_overview_band_window::<T>(
                    source_layer - 1,
                    band_index,
                    src_row,
                    src_col,
                    src_rows,
                    src_cols,
                )?;
                let data = data
                    .into_dimensionality::<ndarray::Ix2>()
                    .context("expected 2D overview source")?;
                let downsampled = downsample_2d(&data, job.rows, job.cols, downsample, opts, nodata)?;
                StripTile::Single(downsampled)
            } else {
                let data = input.read_overview_window::<T>(
                    source_layer - 1,
                    src_row,
                    src_col,
                    src_rows,
                    src_cols,
                )?;
                let data = data
                    .into_dimensionality::<ndarray::Ix3>()
                    .context("expected [rows, cols, bands] overview source")?;
                let data = if let Some(bands) = band_map {
                    select_bands(&data, bands)?
                } else {
                    data
                };
                let downsampled = downsample_3d(&data, job.rows, job.cols, downsample, opts, nodata)?;
                StripTile::Multi(downsampled)
            };
            raw
        };
        let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
        let block = remux_compress_tile(&samples, block_index, encoding)
            .map_err(|err| anyhow::anyhow!(err))?;
        out.push((block_index, job.col_off, job.row_off, tile, block));
    }
    Ok(out)
}

pub(crate) fn read_strip_row_batch<T>(
    input: &GeoTiffFile,
    out_bands: usize,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    row_off: usize,
    jobs: &[TileJob],
) -> Result<Vec<(usize, usize, StripTile<T>)>>
where
    T: TiffSample + Clone,
{
    let rows = jobs[0].rows;
    let src_row = window.map(|w| w.row_off + row_off).unwrap_or(row_off);
    let src_col0 = window.map(|w| w.col_off).unwrap_or(0);
    let read_cols = jobs
        .iter()
        .map(|job| job.col_off + job.cols)
        .max()
        .unwrap_or(0);

    let mut out = Vec::with_capacity(jobs.len());
    if out_bands == 1 {
        let band_index = band_map.map(|bands| bands[0] - 1).unwrap_or(0);
        let data = input.read_band_window::<T>(band_index, src_row, src_col0, rows, read_cols)?;
        let data = data
            .into_dimensionality::<ndarray::Ix2>()
            .context("expected 2D strip batch")?;
        for job in jobs {
            let tile = data
                .slice(s![.., job.col_off..job.col_off + job.cols])
                .to_owned();
            out.push((job.col_off, job.row_off, StripTile::Single(tile)));
        }
    } else {
        let data = input.read_window::<T>(src_row, src_col0, rows, read_cols)?;
        let data = data
            .into_dimensionality::<ndarray::Ix3>()
            .context("expected [rows, cols, bands] strip batch")?;
        for job in jobs {
            let slice = data.slice(s![.., job.col_off..job.col_off + job.cols, ..]);
            let tile_data = if let Some(bands) = band_map {
                select_bands(&slice.to_owned(), bands)?
            } else {
                slice.to_owned()
            };
            out.push((job.col_off, job.row_off, StripTile::Multi(tile_data)));
        }
    }
    Ok(out)
}

fn select_bands<T: Clone>(data: &Array3<T>, bands: &[usize]) -> Result<Array3<T>> {
    let mut slices = Vec::with_capacity(bands.len());
    for band in bands {
        let index = band - 1;
        if index >= data.len_of(Axis(2)) {
            anyhow::bail!("band {band} is not present in decoded window");
        }
        slices.push(data.index_axis(Axis(2), index).to_owned());
    }
    ndarray::stack(Axis(2), &slices.iter().map(|s| s.view()).collect::<Vec<_>>())
        .context("failed to stack band subset")
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

pub(crate) fn output_tile_encoding(opts: &CogOutputOptions, tile_size: usize, spp: u16) -> RemuxTileEncoding {
    crate::cog::tile_encoding_from_opts(opts, tile_size, spp, None)
}
