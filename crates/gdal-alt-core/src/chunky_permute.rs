use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{remux_compress_tile, RemuxCompressedBlock, RemuxTileEncoding};
use ndarray::{Array3, Axis};
use rayon::prelude::*;
use tiff_core::Predictor;
use tiff_reader::{Ifd, TiffSample};

use crate::cog::{tile_jobs, CogOutputOptions, TileJob};
use crate::input::RasterProfile;
use crate::remux::layer_ifd;

pub fn build_chunky_band_permute_layers<T>(
    input: &GeoTiffFile,
    bands: &[usize],
    profile: &RasterProfile,
    opts: &CogOutputOptions,
) -> Result<Vec<Vec<RemuxCompressedBlock>>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let _ = profile;
    let base_ifd = input.tiff().ifd(input.base_ifd_index())?;
    let tile_size = base_ifd.tile_width().unwrap_or(opts.blocksize) as usize;
    let layer_count = 1 + input.overview_count();

    (0..layer_count)
        .into_par_iter()
        .map(|layer_index| {
            build_chunky_band_permute_layer::<T>(
                input,
                layer_index,
                bands,
                profile,
                opts,
                tile_size,
            )
        })
        .collect()
}

fn build_chunky_band_permute_layer<T>(
    input: &GeoTiffFile,
    layer_index: usize,
    bands: &[usize],
    _profile: &RasterProfile,
    opts: &CogOutputOptions,
    tile_size: usize,
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let ifd = layer_ifd(input, layer_index)?;
    let jobs = tile_jobs(ifd.width(), ifd.height(), tile_size as u32);
    let encoding = tile_encoding(ifd, opts, tile_size, bands.len() as u16);

    let mut blocks = jobs
        .par_iter()
        .enumerate()
        .map(|(block_index, job)| {
            let data = read_chunky_tile::<T>(input, layer_index, job)?;
            let data = select_bands(&data, bands)?;
            let padded = pad_tile_chunky(&data, job.rows, job.cols, bands.len(), tile_size);
            let block = remux_compress_tile(&padded, block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            Ok((block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;

    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
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
        .context("expected [rows, cols, bands] tile window")
}

fn select_bands<T: Copy>(data: &Array3<T>, bands: &[usize]) -> Result<Array3<T>> {
    let mut slices = Vec::with_capacity(bands.len());
    for band in bands {
        let index = band - 1;
        if index >= data.len_of(Axis(2)) {
            anyhow::bail!("band {band} is out of range for chunky permute");
        }
        slices.push(data.index_axis(Axis(2), index).to_owned());
    }
    ndarray::stack(Axis(2), &slices.iter().map(|s| s.view()).collect::<Vec<_>>())
        .context("failed to stack permuted bands")
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
