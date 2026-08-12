use std::collections::BTreeMap;

use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{remux_compress_tile, RemuxCompressedBlock, RemuxTileEncoding};
use ndarray::{Array2, Array3, Axis, s};
use rayon::prelude::*;
use tiff_core::Predictor;
use tiff_reader::TiffSample;

use crate::cog::{overview_levels, tile_jobs, CogOutputOptions, TileJob};
use crate::crop::WriteWindow;
use crate::input::RasterProfile;
use crate::remux::remux_encoded_layers;

pub fn convert_strip_to_remux_cog<T>(
    pool: &rayon::ThreadPool,
    input: &GeoTiffFile,
    output: &std::path::Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let width = profile.width;
    let height = profile.height;
    let out_bands = profile.bands as usize;
    let tile_size = opts.blocksize as usize;
    let levels = overview_levels(opts, width, height);
    let encoding = output_tile_encoding(opts, tile_size, out_bands as u16);

    let base_layer = pool.install(|| {
        build_strip_base_layer::<T>(
            input,
            width,
            height,
            tile_size,
            out_bands,
            window,
            band_map,
            encoding,
        )
    })?;

    let mut layers = Vec::with_capacity(1 + levels.len());
    layers.push(base_layer);

    for &level in levels.iter() {
        let layer = build_strip_overview_layer::<T>(
            input,
            width,
            height,
            level,
            tile_size,
            out_bands,
            window,
            band_map,
            encoding,
            opts,
        )?;
        layers.push(layer);
    }

    remux_encoded_layers(profile, opts, layers, output, Some(levels))
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
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
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

    let mut decoded = row_groups
        .par_iter()
        .map(|(&row_off, jobs)| {
            read_strip_row_batch::<T>(input, out_bands, window, band_map, row_off, jobs)
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    decoded.sort_by_key(|(col, row, _)| (*row, *col));

    decoded
        .par_iter()
        .map(|(col_off, row_off, tile)| {
            let block_index = tile_index_by_pos[&( *col_off, *row_off)];
            let samples = encode_tile_samples(tile, tile_size, out_bands)?;
            let block = remux_compress_tile(&samples, block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            Ok((block_index, block))
        })
        .collect::<Result<Vec<_>>>()
        .map(|mut blocks| {
            blocks.sort_by_key(|(index, _)| *index);
            blocks.into_iter().map(|(_, block)| block).collect()
        })
}

fn build_strip_overview_layer<T>(
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
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let scale = level as usize;
    let ov_width = width.div_ceil(level);
    let ov_height = height.div_ceil(level);
    let jobs = tile_jobs(ov_width, ov_height, tile_size as u32);

    let mut blocks = jobs
        .par_iter()
        .enumerate()
        .map(|(block_index, job)| {
            let (src_col, src_row, src_cols, src_rows) =
                overview_source_window(job, scale, width as usize, height as usize, window);
            let tile = read_overview_source::<T>(
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
            )?;
            let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
            let block = remux_compress_tile(&samples, block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            Ok((block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
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
) -> Result<StripTile<T>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    if out_bands == 1 {
        let band_index = band_map.map(|bands| bands[0] - 1).unwrap_or(0);
        let data = input.read_band_window::<T>(band_index, src_row, src_col, src_rows, src_cols)?;
        let data = data
            .into_dimensionality::<ndarray::Ix2>()
            .context("expected 2D overview source")?;
        let downsampled = downsample_2d(&data, out_rows, out_cols, scale, opts)?;
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
        let downsampled = downsample_3d(&data, out_rows, out_cols, scale, opts)?;
        Ok(StripTile::Multi(downsampled))
    }
}

fn downsample_2d<T>(
    src: &Array2<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
    opts: &CogOutputOptions,
) -> Result<Array2<T>>
where
    T: geotiff_writer::NumericSample + Copy + Default,
{
    match opts.resampling {
        crate::cog::ResamplingChoice::Nearest => Ok(nearest_downsample_2d(src, out_rows, out_cols, scale)),
        crate::cog::ResamplingChoice::Average => Ok(average_downsample_2d(src, out_rows, out_cols, scale)),
    }
}

fn downsample_3d<T>(
    src: &Array3<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
    opts: &CogOutputOptions,
) -> Result<Array3<T>>
where
    T: geotiff_writer::NumericSample + Copy + Default,
{
    match opts.resampling {
        crate::cog::ResamplingChoice::Nearest => {
            Ok(nearest_downsample_3d(src, out_rows, out_cols, scale))
        }
        crate::cog::ResamplingChoice::Average => {
            Ok(average_downsample_3d(src, out_rows, out_cols, scale))
        }
    }
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

enum StripTile<T> {
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

fn read_strip_row_batch<T>(
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

fn output_tile_encoding(opts: &CogOutputOptions, tile_size: usize, spp: u16) -> RemuxTileEncoding {
    RemuxTileEncoding {
        compression: opts.compression.to_compression(),
        predictor: Predictor::None,
        samples_per_pixel: spp,
        tile_width: tile_size,
        tile_height: tile_size as u32,
        deflate_level: opts.deflate_level,
    }
}
