use std::collections::BTreeMap;

use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{remux_compress_tile, RemuxCompressedBlock, RemuxTileEncoding};
use ndarray::{Array2, Array3, s};
use rayon::prelude::*;
use tiff_reader::TiffSample;

use crate::cog::{tile_jobs, CogOutputOptions, TileJob};
use crate::crop::WriteWindow;
use crate::encode::sink::StreamingEncodeSink;
use crate::progress::StageBar;
use crate::remux::resolve_overview_read_source;

use super::decode::with_decode_permit;
use super::regions::{
    copy_tile_region_2d, copy_tile_region_3d, downsample_2d, downsample_3d, overview_source_window,
    read_overview_source,
};
use super::tile::{DecodedTileSpool, StripTile};
use super::tiles::{encode_tile_samples, select_bands};

pub(crate) fn stream_strip_overview_layer_with_cache_to_spool<T>(
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
    writer: &impl StreamingEncodeSink,
    progress: Option<&StageBar>,
) -> Result<Option<DecodedTileSpool<T>>>
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

    let decoded_spool = if cache_decoded {
        Some(DecodedTileSpool::new_for_layer(
            width,
            height,
            level,
            tile_size,
            out_bands,
        )?)
    } else {
        None
    };

    row_groups.par_iter().try_for_each(|(&row_off, row_jobs)| -> Result<()> {
        if let Some(bar) = progress {
            bar.inc(1);
        }
        let batch = with_decode_permit(|| {
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
        })?;
        for (block_index, col_off, row_off, tile, block) in batch {
            if let Some(spool) = decoded_spool.as_ref() {
                spool.insert(col_off, row_off, &tile)?;
            }
            writer.write_block(block_index, block)?;
        }
        Ok(())
    })?;

    Ok(decoded_spool)
}

pub(crate) fn stream_strip_overview_from_decoded_to_spool<T>(
    parent_tiles: &DecodedTileSpool<T>,
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
    writer: &impl StreamingEncodeSink,
    progress: Option<&StageBar>,
) -> Result<Option<DecodedTileSpool<T>>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
{
    let downsample = (level / parent_level).max(1) as usize;
    let ov_width = (width / level).max(1);
    let ov_height = (height / level).max(1);
    let jobs = tile_jobs(ov_width, ov_height, tile_size as u32);
    let parent = parent_tiles.clone();
    let decoded_spool = if cache_decoded {
        Some(DecodedTileSpool::new_for_layer(
            width,
            height,
            level,
            tile_size,
            out_bands,
        )?)
    } else {
        None
    };

    jobs.par_iter()
        .enumerate()
        .try_for_each(|(block_index, job)| -> Result<()> {
            if let Some(bar) = progress {
                bar.inc(1);
            }
            let tile = with_decode_permit(|| {
                downsample_parent_tile::<T>(
                    &parent,
                    job,
                    parent_level,
                    downsample,
                    out_bands,
                    tile_size,
                    opts,
                    nodata,
                )
            })?;
            if let Some(spool) = decoded_spool.as_ref() {
                spool.insert(job.col_off, job.row_off, &tile)?;
            }
            let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
            let block = remux_compress_tile(&samples, block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            writer.write_block(block_index, block)?;
            Ok(())
        })?;

    Ok(decoded_spool)
}

fn downsample_parent_tile<T>(
    parent: &DecodedTileSpool<T>,
    job: &TileJob,
    _parent_level: u32,
    downsample: usize,
    out_bands: usize,
    tile_size: usize,
    opts: &CogOutputOptions,
    nodata: Option<T>,
) -> Result<StripTile<T>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
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
                    let Some(tile) = parent.try_get(tile_col, tile_row)? else {
                        continue;
                    };
                    let StripTile::Single(data) = tile else {
                        anyhow::bail!("expected single-band parent tile");
                    };
                    copy_tile_region_2d(
                        &data,
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
                    let Some(tile) = parent.try_get(tile_col, tile_row)? else {
                        continue;
                    };
                    let StripTile::Multi(data) = tile else {
                        anyhow::bail!("expected multi-band parent tile");
                    };
                    copy_tile_region_3d(
                        &data,
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

    if source_layer == 0 && layer_downsample == target_level as usize {
        let scale = target_level as usize;
        let mut min_src_col = usize::MAX;
        let mut max_src_end = 0usize;
        let mut min_src_row = usize::MAX;
        let mut max_src_row_end = 0usize;
        for job in jobs {
            let (src_col, src_row, src_cols, src_rows) =
                overview_source_window(job, scale, width, height, window);
            min_src_col = min_src_col.min(src_col);
            max_src_end = max_src_end.max(src_col.saturating_add(src_cols));
            min_src_row = min_src_row.min(src_row);
            max_src_row_end = max_src_row_end.max(src_row.saturating_add(src_rows));
        }
        let read_cols = max_src_end.saturating_sub(min_src_col);
        let read_rows = max_src_row_end.saturating_sub(min_src_row);

        if out_bands == 1 {
            let band_index = band_map.map(|bands| bands[0] - 1).unwrap_or(0);
            let data = input.read_band_window::<T>(
                band_index,
                min_src_row,
                min_src_col,
                read_rows,
                read_cols,
            )?;
            let data = data
                .into_dimensionality::<ndarray::Ix2>()
                .context("expected 2D batched overview source")?;
            for job in jobs {
                let block_index = tile_index_by_pos[&(job.col_off, job.row_off)];
                let (src_col, src_row, src_cols, src_rows) =
                    overview_source_window(job, scale, width, height, window);
                let rel_col = src_col.saturating_sub(min_src_col);
                let rel_row = src_row.saturating_sub(min_src_row);
                let slice = data.slice(s![
                    rel_row..rel_row + src_rows,
                    rel_col..rel_col + src_cols
                ]);
                let downsampled =
                    downsample_2d(&slice.to_owned(), job.rows, job.cols, scale, opts, nodata)?;
                let strip_tile = StripTile::Single(downsampled);
                let samples = encode_tile_samples(&strip_tile, tile_size, out_bands)?;
                let block = remux_compress_tile(&samples, block_index, encoding)
                    .map_err(|err| anyhow::anyhow!(err))?;
                out.push((
                    block_index,
                    job.col_off,
                    job.row_off,
                    strip_tile,
                    block,
                ));
            }
        } else {
            let data = input.read_window::<T>(min_src_row, min_src_col, read_rows, read_cols)?;
            let data = data
                .into_dimensionality::<ndarray::Ix3>()
                .context("expected [rows, cols, bands] batched overview source")?;
            for job in jobs {
                let block_index = tile_index_by_pos[&(job.col_off, job.row_off)];
                let (src_col, src_row, src_cols, src_rows) =
                    overview_source_window(job, scale, width, height, window);
                let rel_col = src_col.saturating_sub(min_src_col);
                let rel_row = src_row.saturating_sub(min_src_row);
                let slice = data.slice(s![
                    rel_row..rel_row + src_rows,
                    rel_col..rel_col + src_cols,
                    ..
                ]);
                let tile_data = if let Some(bands) = band_map {
                    select_bands(&slice.to_owned(), bands)?
                } else {
                    slice.to_owned()
                };
                let downsampled =
                    downsample_3d(&tile_data, job.rows, job.cols, scale, opts, nodata)?;
                let strip_tile = StripTile::Multi(downsampled);
                let samples = encode_tile_samples(&strip_tile, tile_size, out_bands)?;
                let block = remux_compress_tile(&samples, block_index, encoding)
                    .map_err(|err| anyhow::anyhow!(err))?;
                out.push((
                    block_index,
                    job.col_off,
                    job.row_off,
                    strip_tile,
                    block,
                ));
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
