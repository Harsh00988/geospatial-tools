use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{remux_compress_tile, RemuxCompressedBlock, RemuxTileEncoding};
use ndarray::{Array2, Array3, Axis, s};
use rayon::prelude::*;
use tiff_reader::TiffSample;
use tiff_core::SampleFormat;

use crate::cog::tile_payload::input_compression;
use crate::cog::{configure_cog, overview_levels, tile_jobs, CogOutputOptions, TileJob};
use crate::crop::WriteWindow;
use crate::encode_overview::{encode_layers_with_spool, encode_overview_layers_to_streaming_cog};
use crate::input::RasterProfile;
use crate::progress::{ProgressTracker, StageBar};
use crate::remux::{encode_output_needs_mask_remux, remux_encoded_layers_from_spool, resolve_overview_read_source};
use crate::spool::StreamingEncodeSink;
use tiff_core::Compression;
use tempfile::tempfile;

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
    let encoding = output_tile_encoding(opts, tile_size, out_bands as u16, profile.sample.sample_format);
    let progress = ProgressTracker::new(show_progress);
    let encode_total = encode_row_group_total(width, height, tile_size, &levels);
    let encode_bar = progress.stage("Encode tiles", encode_total);

    if encode_output_needs_mask_remux(input, profile, opts) {
        let spool = pool.install(|| {
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
        remux_encoded_layers_from_spool(
            input,
            profile,
            opts,
            spool,
            output,
            window.as_ref(),
            Some(levels),
            None,
            show_progress,
        )?;
    } else {
        let cog = configure_cog(profile.base_builder(opts), opts, width, height);
        let stream = cog.open_streaming_rgb_writer::<T, _>(output, 1 + levels.len())?;
        pool.install(|| {
            encode_overview_layers_to_streaming_cog::<T>(
                input,
                &stream,
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
        stream.finish().map_err(|err| anyhow::anyhow!(err))?;
    }

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

/// How many tile-rows of raster data to hold in memory per strip decode window.
const STRIP_DECODE_TILE_ROWS: usize = 4;

/// Max concurrent strip row-group decoders (limits peak decode-buffer RAM).
/// Override with `FASTCOG_DECODE_WORKERS` (default 8).
const DEFAULT_STRIP_DECODE_PARALLELISM: usize = 8;

fn strip_decode_parallelism() -> usize {
    std::env::var("FASTCOG_DECODE_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_STRIP_DECODE_PARALLELISM)
        .clamp(1, rayon::current_num_threads().max(1))
}

struct DecodeConcurrency {
    available: std::sync::Mutex<usize>,
    notify: std::sync::Condvar,
}

struct DecodePermit<'a> {
    gate: &'a DecodeConcurrency,
}

impl Drop for DecodePermit<'_> {
    fn drop(&mut self) {
        let mut slots = self.gate.available.lock().expect("decode gate lock");
        *slots += 1;
        self.gate.notify.notify_one();
    }
}

impl DecodeConcurrency {
    fn new(limit: usize) -> Self {
        Self {
            available: std::sync::Mutex::new(limit),
            notify: std::sync::Condvar::new(),
        }
    }

    fn acquire(&self) -> DecodePermit<'_> {
        let mut slots = self.available.lock().expect("decode gate lock");
        while *slots == 0 {
            slots = self.notify.wait(slots).expect("decode gate wait");
        }
        *slots -= 1;
        DecodePermit { gate: self }
    }
}

fn decode_concurrency() -> &'static DecodeConcurrency {
    static GATE: OnceLock<DecodeConcurrency> = OnceLock::new();
    GATE.get_or_init(|| DecodeConcurrency::new(strip_decode_parallelism()))
}

/// Rows to decode per strip I/O window. Small strips decode in one shot; large strips
/// (e.g. ICEYE single-strip images) decode `STRIP_DECODE_TILE_ROWS` tile rows at a time.
pub(crate) fn strip_decode_window_rows(rows_per_strip: usize, tile_size: usize) -> usize {
    let max_window = tile_size.saturating_mul(STRIP_DECODE_TILE_ROWS).max(tile_size);
    rows_per_strip.min(max_window)
}

/// Use strip-window decode only for **compressed** single-strip images where each
/// row-group read would otherwise re-decompress the entire strip.
fn should_use_strip_windowed_decode(
    input: &GeoTiffFile,
    rows_per_strip: usize,
    tile_size: usize,
    window: Option<WriteWindow>,
) -> bool {
    if window.is_some() || rows_per_strip <= tile_size {
        return false;
    }
    let ifd = match input.tiff().ifd(input.base_ifd_index()) {
        Ok(ifd) => ifd,
        Err(_) => return false,
    };
    !ifd.is_tiled() && input_compression(ifd) != Compression::None
}

fn should_use_tiled_source_decode(input: &GeoTiffFile, window: Option<WriteWindow>) -> bool {
    if window.is_some() {
        return false;
    }
    input
        .tiff()
        .ifd(input.base_ifd_index())
        .map(|ifd| ifd.is_tiled())
        .unwrap_or(false)
}

fn stream_base_layer_from_strip_windows_to_spool<T>(
    input: &GeoTiffFile,
    width: u32,
    height: u32,
    tile_size: usize,
    rows_per_strip: usize,
    out_bands: usize,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    writer: &impl StreamingEncodeSink,
    progress: Option<&StageBar>,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + Clone,
{
    let decode_window_rows = strip_decode_window_rows(rows_per_strip, tile_size);
    let image_height = height as usize;
    let image_width = width as usize;
    let tiles = tile_jobs(width, height, tile_size as u32);
    let tile_index_by_pos: std::collections::HashMap<(usize, usize), usize> = tiles
        .iter()
        .enumerate()
        .map(|(idx, job)| ((job.col_off, job.row_off), idx))
        .collect();

    let mut jobs_by_window: BTreeMap<(usize, usize), Vec<TileJob>> = BTreeMap::new();
    for job in tiles {
        let strip_idx = job.row_off / rows_per_strip;
        let strip_row_start = strip_idx * rows_per_strip;
        let rel_row = job.row_off.saturating_sub(strip_row_start);
        let window_idx = rel_row / decode_window_rows;
        jobs_by_window
            .entry((strip_idx, window_idx))
            .or_default()
            .push(job);
    }

    let gate = decode_concurrency();
    jobs_by_window
        .par_iter()
        .try_for_each(|(&(strip_idx, window_idx), jobs)| -> Result<()> {
            let strip_row_start = strip_idx * rows_per_strip;
            let window_row_start = strip_row_start + window_idx * decode_window_rows;
            let window_row_end = (window_row_start + decode_window_rows).min(image_height);
            let row_count = window_row_end.saturating_sub(window_row_start);
            let strip_tile = {
                let _permit = gate.acquire();
                read_decoded_strip::<T>(
                    input,
                    window_row_start,
                    0,
                    row_count,
                    image_width,
                    out_bands,
                    band_map,
                )?
            };

            for job in jobs {
                let tile = slice_strip_tile(&strip_tile, job, window_row_start, 0, out_bands)?;
                let block_index = tile_index_by_pos[&(job.col_off, job.row_off)];
                let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
                let block = remux_compress_tile(&samples, block_index, encoding)
                    .map_err(|err| anyhow::anyhow!(err))?;
                writer.write_block(block_index, block)?;
            }

            if let Some(bar) = progress {
                bar.inc(1);
            }
            Ok(())
        })?;

    Ok(())
}

fn stream_base_layer_from_source_tiles_to_spool<T>(
    input: &GeoTiffFile,
    width: u32,
    height: u32,
    tile_size: usize,
    out_bands: usize,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    writer: &impl StreamingEncodeSink,
    progress: Option<&StageBar>,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + Clone,
{
    let ifd = input.tiff().ifd(input.base_ifd_index())?;
    let src_tile_w = ifd.tile_width().unwrap_or(tile_size as u32) as usize;
    let src_tile_h = ifd.tile_height().unwrap_or(tile_size as u32) as usize;
    let image_w = width as usize;
    let image_h = height as usize;
    let tiles_across = image_w.div_ceil(src_tile_w);
    let tiles_down = image_h.div_ceil(src_tile_h);

    let output_jobs = tile_jobs(width, height, tile_size as u32);
    let tile_index_by_pos: std::collections::HashMap<(usize, usize), usize> = output_jobs
        .iter()
        .enumerate()
        .map(|(idx, job)| ((job.col_off, job.row_off), idx))
        .collect();

    let gate = decode_concurrency();
    (0..tiles_down)
        .into_par_iter()
        .try_for_each(|src_tr| -> Result<()> {
            (0..tiles_across).try_for_each(|src_tc| -> Result<()> {
                let col_off = src_tc * src_tile_w;
                let row_off = src_tr * src_tile_h;
                let cols = src_tile_w.min(image_w.saturating_sub(col_off));
                let rows = src_tile_h.min(image_h.saturating_sub(row_off));
                let src_tile = {
                    let _permit = gate.acquire();
                    read_decoded_strip::<T>(input, row_off, col_off, rows, cols, out_bands, band_map)?
                };

                let src_col_end = col_off + cols;
                let src_row_end = row_off + rows;
                for job in &output_jobs {
                    if job.col_off + job.cols <= col_off
                        || job.col_off >= src_col_end
                        || job.row_off + job.rows <= row_off
                        || job.row_off >= src_row_end
                    {
                        continue;
                    }
                    let tile = slice_strip_tile(&src_tile, job, row_off, col_off, out_bands)?;
                    let block_index = tile_index_by_pos[&(job.col_off, job.row_off)];
                    let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
                    let block = remux_compress_tile(&samples, block_index, encoding)
                        .map_err(|err| anyhow::anyhow!(err))?;
                    writer.write_block(block_index, block)?;
                }

                if let Some(bar) = progress {
                    bar.inc(1);
                }
                Ok(())
            })
        })?;

    Ok(())
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
    strip_col_start: usize,
    out_bands: usize,
) -> Result<StripTile<T>> {
    let rel_row = job.row_off.saturating_sub(strip_row_start);
    let rel_col = job.col_off.saturating_sub(strip_col_start);
    match strip {
        StripTile::Single(data) => {
            let tile = data
                .slice(s![rel_row..rel_row + job.rows, rel_col..rel_col + job.cols])
                .to_owned();
            Ok(StripTile::Single(tile))
        }
        StripTile::Multi(data) => {
            let tile = data
                .slice(s![
                    rel_row..rel_row + job.rows,
                    rel_col..rel_col + job.cols,
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

pub(crate) fn stream_base_layer_to_spool<T>(
    input: &GeoTiffFile,
    width: u32,
    height: u32,
    tile_size: usize,
    out_bands: usize,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    writer: &impl StreamingEncodeSink,
    progress: Option<&StageBar>,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    if should_use_tiled_source_decode(input, window) {
        return stream_base_layer_from_source_tiles_to_spool::<T>(
            input,
            width,
            height,
            tile_size,
            out_bands,
            band_map,
            encoding,
            writer,
            progress,
        );
    }

    let rows_per_strip = strip_rows_per_strip(input)?;
    if should_use_strip_windowed_decode(input, rows_per_strip, tile_size, window) {
        return stream_base_layer_from_strip_windows_to_spool::<T>(
            input,
            width,
            height,
            tile_size,
            rows_per_strip,
            out_bands,
            band_map,
            encoding,
            writer,
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

    let gate = decode_concurrency();
    row_groups.par_iter().try_for_each(|(&row_off, jobs)| -> Result<()> {
        let batch = {
            let _permit = gate.acquire();
            read_strip_row_batch::<T>(input, out_bands, window, band_map, row_off, jobs)?
        };
        if let Some(bar) = progress {
            bar.inc(1);
        }
        for (col_off, row_off, tile) in batch {
            let block_index = tile_index_by_pos[&(col_off, row_off)];
            let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
            let block = remux_compress_tile(&samples, block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            writer.write_block(block_index, block)?;
        }
        Ok(())
    })?;

    Ok(())
}

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

    let gate = decode_concurrency();
    row_groups.par_iter().try_for_each(|(&row_off, row_jobs)| -> Result<()> {
        if let Some(bar) = progress {
            bar.inc(1);
        }
        let batch = {
            let _permit = gate.acquire();
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
            )?
        };
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

    let gate = decode_concurrency();
    jobs.par_iter()
        .enumerate()
        .try_for_each(|(block_index, job)| -> Result<()> {
            if let Some(bar) = progress {
                bar.inc(1);
            }
            let tile = {
                let _permit = gate.acquire();
                downsample_parent_tile::<T>(
                    &parent,
                    job,
                    parent_level,
                    downsample,
                    out_bands,
                    tile_size,
                    opts,
                    nodata,
                )?
            };
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

/// On-disk or in-memory cache of decoded overview tiles for pyramid chaining.
pub(crate) struct DecodedTileSpool<T> {
    inner: std::sync::Arc<std::sync::Mutex<DecodedTileStorage<T>>>,
    _marker: std::marker::PhantomData<T>,
}

enum DecodedTileStorage<T> {
    Memory(std::collections::HashMap<(usize, usize), StripTile<T>>),
    Disk {
        file: std::fs::File,
        index: std::collections::HashMap<(usize, usize), u64>,
    },
}

const DECODED_TILE_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

impl<T> DecodedTileSpool<T>
where
    T: TiffSample + Copy + Default,
{
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(DecodedTileStorage::Disk {
                file: tempfile()?,
                index: std::collections::HashMap::new(),
            })),
            _marker: std::marker::PhantomData,
        })
    }

    pub fn new_for_layer(
        width: u32,
        height: u32,
        level: u32,
        tile_size: usize,
        out_bands: usize,
    ) -> anyhow::Result<Self> {
        let ov_w = (width / level).max(1) as usize;
        let ov_h = (height / level).max(1) as usize;
        let tile_count = ov_w.div_ceil(tile_size) * ov_h.div_ceil(tile_size);
        let bytes_per_tile = tile_size
            .saturating_mul(tile_size)
            .saturating_mul(out_bands)
            .saturating_mul(std::mem::size_of::<T>());
        let estimated = tile_count.saturating_mul(bytes_per_tile);
        let storage = if estimated <= DECODED_TILE_MEMORY_LIMIT {
            DecodedTileStorage::Memory(std::collections::HashMap::new())
        } else {
            DecodedTileStorage::Disk {
                file: tempfile()?,
                index: std::collections::HashMap::new(),
            }
        };
        Ok(Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(storage)),
            _marker: std::marker::PhantomData,
        })
    }

    pub fn insert(&self, col: usize, row: usize, tile: &StripTile<T>) -> anyhow::Result<()> {
        use std::io::Seek;
        let mut inner = self.inner.lock().expect("decoded tile spool lock");
        match &mut *inner {
            DecodedTileStorage::Memory(tiles) => {
                tiles.insert((col, row), clone_strip_tile(tile));
                Ok(())
            }
            DecodedTileStorage::Disk { file, index } => {
                let offset = file.stream_position()?;
                spool_write_tile(file, tile)?;
                index.insert((col, row), offset);
                Ok(())
            }
        }
    }

    pub fn get(&self, col: usize, row: usize) -> anyhow::Result<Option<StripTile<T>>> {
        self.try_get(col, row)
    }

    pub fn try_get(&self, col: usize, row: usize) -> anyhow::Result<Option<StripTile<T>>> {
        use std::io::Seek;
        let mut inner = self.inner.lock().expect("decoded tile spool lock");
        match &mut *inner {
            DecodedTileStorage::Memory(tiles) => Ok(tiles.get(&(col, row)).map(clone_strip_tile)),
            DecodedTileStorage::Disk { file, index } => {
                let offset = match index.get(&(col, row)) {
                    Some(&offset) => offset,
                    None => return Ok(None),
                };
                file.seek(std::io::SeekFrom::Start(offset))?;
                spool_read_tile(file)
            }
        }
    }
}

fn clone_strip_tile<T: Clone>(tile: &StripTile<T>) -> StripTile<T> {
    match tile {
        StripTile::Single(data) => StripTile::Single(data.clone()),
        StripTile::Multi(data) => StripTile::Multi(data.clone()),
    }
}

impl<T> Clone for DecodedTileSpool<T> {
    fn clone(&self) -> Self {
        Self {
            inner: std::sync::Arc::clone(&self.inner),
            _marker: std::marker::PhantomData,
        }
    }
}

fn spool_write_tile<T: TiffSample + Copy>(
    file: &mut std::fs::File,
    tile: &StripTile<T>,
) -> anyhow::Result<()> {
    use std::io::Write;
    match tile {
        StripTile::Single(data) => {
            file.write_all(&[0u8])?;
            file.write_all(&(data.nrows() as u32).to_le_bytes())?;
            file.write_all(&(data.ncols() as u32).to_le_bytes())?;
            spool_write_slice(file, data.as_slice().context("contiguous single-band tile")?)?;
        }
        StripTile::Multi(data) => {
            file.write_all(&[1u8])?;
            file.write_all(&(data.shape()[0] as u32).to_le_bytes())?;
            file.write_all(&(data.shape()[1] as u32).to_le_bytes())?;
            file.write_all(&(data.shape()[2] as u16).to_le_bytes())?;
            spool_write_slice(file, data.as_slice().context("contiguous multi-band tile")?)?;
        }
    }
    Ok(())
}

fn spool_read_tile<T: TiffSample + Copy + Default>(
    file: &mut std::fs::File,
) -> anyhow::Result<Option<StripTile<T>>> {
    use std::io::Read;
    let mut tag = [0u8; 1];
    if file.read_exact(&mut tag).is_err() {
        return Ok(None);
    }
    let rows = spool_read_u32(file)? as usize;
    let cols = spool_read_u32(file)? as usize;
    match tag[0] {
        0 => Ok(Some(StripTile::Single(spool_read_array2(file, rows, cols)?))),
        1 => {
            let bands = spool_read_u16(file)? as usize;
            Ok(Some(StripTile::Multi(spool_read_array3(
                file, rows, cols, bands,
            )?)))
        }
        _ => anyhow::bail!("invalid decoded tile spool tag {}", tag[0]),
    }
}

fn spool_write_slice<T: TiffSample + Copy>(
    file: &mut std::fs::File,
    data: &[T],
) -> anyhow::Result<()> {
    use std::io::Write;
    let bytes = unsafe {
        std::slice::from_raw_parts(
            data.as_ptr().cast::<u8>(),
            data.len() * std::mem::size_of::<T>(),
        )
    };
    file.write_all(bytes)?;
    Ok(())
}

fn spool_read_array2<T: TiffSample + Copy + Default>(
    file: &mut std::fs::File,
    rows: usize,
    cols: usize,
) -> anyhow::Result<Array2<T>> {
    use std::io::Read;
    let len = rows
        .checked_mul(cols)
        .context("tile element count overflow")?;
    let mut data = vec![T::default(); len];
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            data.as_mut_ptr().cast::<u8>(),
            len * std::mem::size_of::<T>(),
        )
    };
    file.read_exact(bytes)?;
    Array2::from_shape_vec((rows, cols), data).context("invalid 2D tile shape")
}

fn spool_read_array3<T: TiffSample + Copy + Default>(
    file: &mut std::fs::File,
    rows: usize,
    cols: usize,
    bands: usize,
) -> anyhow::Result<Array3<T>> {
    use std::io::Read;
    let len = rows
        .checked_mul(cols)
        .and_then(|v| v.checked_mul(bands))
        .context("tile element count overflow")?;
    let mut data = vec![T::default(); len];
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            data.as_mut_ptr().cast::<u8>(),
            len * std::mem::size_of::<T>(),
        )
    };
    file.read_exact(bytes)?;
    Array3::from_shape_vec((rows, cols, bands), data).context("invalid 3D tile shape")
}

fn spool_read_u32(file: &mut std::fs::File) -> anyhow::Result<u32> {
    use std::io::Read;
    let mut buf = [0u8; 4];
    file.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn spool_read_u16(file: &mut std::fs::File) -> anyhow::Result<u16> {
    use std::io::Read;
    let mut buf = [0u8; 2];
    file.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
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

pub(crate) fn output_tile_encoding(
    opts: &CogOutputOptions,
    tile_size: usize,
    spp: u16,
    sample_format: SampleFormat,
) -> RemuxTileEncoding {
    crate::cog::tile_encoding_from_opts(opts, tile_size, spp, None, sample_format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiff_core::Compression;

    #[test]
    fn strip_decode_window_caps_large_strips() {
        assert_eq!(strip_decode_window_rows(25_976, 512), 512 * STRIP_DECODE_TILE_ROWS);
        assert_eq!(strip_decode_window_rows(512, 512), 512);
        assert_eq!(strip_decode_window_rows(1_024, 512), 1_024);
        assert_eq!(strip_decode_window_rows(2_048, 512), 2_048);
        assert_eq!(strip_decode_window_rows(2_049, 512), 512 * STRIP_DECODE_TILE_ROWS);
    }

    #[test]
    fn strip_windowed_decode_only_for_compressed_full_image_strips() {
        // Uncompressed ICEYE-like strip: row-group path is faster and already low RAM.
        assert!(!should_use_strip_windowed_decode_for_compression(
            Compression::None,
            25_976,
            512,
            None,
        ));
        assert!(should_use_strip_windowed_decode_for_compression(
            Compression::Lzw,
            25_976,
            512,
            None,
        ));
        assert!(!should_use_strip_windowed_decode_for_compression(
            Compression::Lzw,
            512,
            512,
            None,
        ));
        assert!(!should_use_strip_windowed_decode_for_compression(
            Compression::Lzw,
            25_976,
            512,
            Some(WriteWindow {
                col_off: 0,
                row_off: 0,
                width: 1024,
                height: 1024,
            }),
        ));
    }

    fn should_use_strip_windowed_decode_for_compression(
        compression: Compression,
        rows_per_strip: usize,
        tile_size: usize,
        window: Option<WriteWindow>,
    ) -> bool {
        if window.is_some() || rows_per_strip <= tile_size {
            return false;
        }
        compression != Compression::None
    }

    #[test]
    fn strip_window_count_scales_with_image_height() {
        let rows_per_strip = 25_976;
        let tile_size = 512;
        let image_height: usize = 25_976;
        let decode_window = strip_decode_window_rows(rows_per_strip, tile_size);
        let windows_per_strip = image_height.div_ceil(decode_window);
        // 25976 / 2048 = 13 windows (not 1 full-strip decode)
        assert_eq!(decode_window, 2048);
        assert_eq!(windows_per_strip, 13);
    }

    #[test]
    fn decoded_tile_spool_roundtrips_single_band_tile() {
        use ndarray::arr2;
        let tile = StripTile::Single(arr2(&[[1u16, 2], [3, 4]]));
        let spool = DecodedTileSpool::<u16>::new().unwrap();
        spool.insert(512, 1024, &tile).unwrap();
        let loaded = spool.get(512, 1024).unwrap().unwrap();
        match loaded {
            StripTile::Single(data) => assert_eq!(data[[0, 0]], 1),
            _ => panic!("expected single-band tile"),
        }
        assert!(spool.get(0, 0).unwrap().is_none());
    }
}
