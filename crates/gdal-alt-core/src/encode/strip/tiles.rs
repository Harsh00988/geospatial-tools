use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use ndarray::{Array2, Array3, Axis, s};
use tiff_reader::TiffSample;

use crate::cog::TileJob;
use crate::crop::WriteWindow;

use super::tile::StripTile;

pub(super) fn encode_tile_samples<T>(
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

pub(super) fn read_strip_row_batch<T>(
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

pub(super) fn select_bands<T: Clone>(data: &Array3<T>, bands: &[usize]) -> Result<Array3<T>> {
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

pub(super) fn clone_strip_tile<T: Clone>(tile: &StripTile<T>) -> StripTile<T> {
    match tile {
        StripTile::Single(data) => StripTile::Single(data.clone()),
        StripTile::Multi(data) => StripTile::Multi(data.clone()),
    }
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
