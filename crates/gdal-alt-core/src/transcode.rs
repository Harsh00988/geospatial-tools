use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{remux_compress_tile, RemuxCompressedBlock, RemuxTileEncoding};
use ndarray::{Array2, Array3};
use rayon::prelude::*;
use tiff_core::{PlanarConfiguration, Predictor};
use tiff_reader::TiffSample;

use crate::cog::tile_payload::ifd_planar;
use crate::cog::{tile_jobs, CogOutputOptions, TileJob};
use crate::remux::layer_ifd;

pub fn build_transcode_layers<T>(
    input: &GeoTiffFile,
    profile: &crate::input::RasterProfile,
    opts: &CogOutputOptions,
) -> Result<Vec<Vec<RemuxCompressedBlock>>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let base_ifd = input.tiff().ifd(input.base_ifd_index())?;
    let tile_size = base_ifd.tile_width().unwrap_or(opts.blocksize) as usize;
    let bands = profile.bands as usize;
    let planar = ifd_planar(base_ifd) == PlanarConfiguration::Planar || bands == 1;
    let layer_count = 1 + input.overview_count();

    (0..layer_count)
        .into_par_iter()
        .map(|layer_index| {
            if planar {
                build_planar_transcode_layer::<T>(input, layer_index, opts, tile_size, bands)
            } else {
                build_chunky_transcode_layer::<T>(input, layer_index, opts, tile_size, bands)
            }
        })
        .collect()
}

fn build_planar_transcode_layer<T>(
    input: &GeoTiffFile,
    layer_index: usize,
    opts: &CogOutputOptions,
    tile_size: usize,
    bands: usize,
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let ifd = layer_ifd(input, layer_index)?;
    let jobs = tile_jobs(ifd.width(), ifd.height(), tile_size as u32);
    let encoding = output_tile_encoding(opts, tile_size, 1);

    let mut work: Vec<(usize, usize, TileJob)> = Vec::with_capacity(jobs.len() * bands);
    for (tile_idx, job) in jobs.iter().copied().enumerate() {
        for band in 0..bands {
            let block_index = band * jobs.len() + tile_idx;
            work.push((block_index, band, job));
        }
    }

    let mut blocks = work
        .par_iter()
        .map(|(block_index, band, job)| {
            let data = read_planar_tile::<T>(input, layer_index, *band, job)?;
            let padded = pad_tile_2d(&data, job.rows, job.cols, tile_size);
            let block = remux_compress_tile(&padded, *block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            Ok((*block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

fn build_chunky_transcode_layer<T>(
    input: &GeoTiffFile,
    layer_index: usize,
    opts: &CogOutputOptions,
    tile_size: usize,
    bands: usize,
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let ifd = layer_ifd(input, layer_index)?;
    let jobs = tile_jobs(ifd.width(), ifd.height(), tile_size as u32);
    let encoding = output_tile_encoding(opts, tile_size, bands as u16);

    let mut blocks = jobs
        .par_iter()
        .enumerate()
        .map(|(block_index, job)| {
            let data = read_chunky_tile::<T>(input, layer_index, job)?;
            let padded = pad_tile_chunky(&data, job.rows, job.cols, bands, tile_size);
            let block = remux_compress_tile(&padded, block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            Ok((block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

fn read_planar_tile<T>(
    input: &GeoTiffFile,
    layer_index: usize,
    band: usize,
    job: &TileJob,
) -> Result<Array2<T>>
where
    T: TiffSample,
{
    let data = if layer_index == 0 {
        input.read_band_window::<T>(band, job.row_off, job.col_off, job.rows, job.cols)?
    } else {
        input.read_overview_band_window::<T>(
            layer_index - 1,
            band,
            job.row_off,
            job.col_off,
            job.rows,
            job.cols,
        )?
    };
    data.into_dimensionality::<ndarray::Ix2>()
        .context("expected 2D band window")
}

fn read_chunky_tile<T>(
    input: &GeoTiffFile,
    layer_index: usize,
    job: &TileJob,
) -> Result<Array3<T>>
where
    T: TiffSample,
{
    let data = if layer_index == 0 {
        input.read_window::<T>(job.row_off, job.col_off, job.rows, job.cols)?
    } else {
        input.read_overview_window::<T>(
            layer_index - 1,
            job.row_off,
            job.col_off,
            job.rows,
            job.cols,
        )?
    };
    data.into_dimensionality::<ndarray::Ix3>()
        .context("expected [rows, cols, bands] window")
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
    crate::cog::tile_encoding_from_opts(opts, tile_size, spp, None)
}
