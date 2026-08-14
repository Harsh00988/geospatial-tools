use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use ndarray::{Array2, Array3, Axis};
use tiff_reader::TiffSample;

use crate::cog::{CogOutputOptions, TileJob};
use crate::crop::WriteWindow;

use super::tile::StripTile;
use super::tiles::select_bands;

pub(super) fn copy_tile_region_2d<T: Copy>(
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

pub(super) fn copy_tile_region_3d<T: Copy>(
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

pub(super) fn overview_source_window(
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

pub(super) fn read_overview_source<T>(
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

pub(super) fn downsample_2d<T>(
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

pub(super) fn downsample_3d<T>(
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
