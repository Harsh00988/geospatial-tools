use anyhow::{bail, Context, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{remux_compress_tile, RemuxCompressedBlock, RemuxTileEncoding};
use ndarray::Array3;
use rayon::prelude::*;
use tiff_core::Predictor;
use tiff_reader::TiffSample;

use crate::cog::{tile_jobs, CogOutputOptions, TileJob};
use crate::input::RasterProfile;
use crate::remux::layer_ifd;

pub fn build_chunky_band_permute_layers<T>(
    input: &GeoTiffFile,
    bands: &[usize],
    _profile: &RasterProfile,
    opts: &CogOutputOptions,
) -> Result<Vec<Vec<RemuxCompressedBlock>>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
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
    opts: &CogOutputOptions,
    tile_size: usize,
) -> Result<Vec<RemuxCompressedBlock>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let ifd = layer_ifd(input, layer_index)?;
    let jobs = tile_jobs(ifd.width(), ifd.height(), tile_size as u32);
    let encoding = tile_encoding(ifd, opts, tile_size, bands.len() as u16)?;

    let mut blocks = jobs
        .par_iter()
        .enumerate()
        .map(|(block_index, job)| {
            let data = read_chunky_tile::<T>(input, layer_index, job)?;
            let padded = permute_and_pad_tile_chunky(&data, bands, job.rows, job.cols, tile_size)?;
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

/// Single-pass band permute + edge padding into the output tile buffer.
fn permute_and_pad_tile_chunky<T: Copy + Default>(
    data: &Array3<T>,
    bands: &[usize],
    rows: usize,
    cols: usize,
    tile_size: usize,
) -> Result<Vec<T>> {
    let src_bands = data.shape()[2];
    let out_bands = bands.len();
    let mut out = vec![T::default(); tile_size * tile_size * out_bands];
    for row in 0..rows {
        for col in 0..cols {
            let dst_base = (row * tile_size + col) * out_bands;
            for (dst_b, &band) in bands.iter().enumerate() {
                let src_band = band - 1;
                if src_band >= src_bands {
                    bail!("band {band} is out of range for chunky permute");
                }
                out[dst_base + dst_b] = data[[row, col, src_band]];
            }
        }
    }
    Ok(out)
}

fn tile_encoding(
    ifd: &tiff_reader::Ifd,
    opts: &CogOutputOptions,
    tile_size: usize,
    spp: u16,
) -> Result<RemuxTileEncoding> {
    let predictor = Predictor::from_code(ifd.predictor()).unwrap_or(Predictor::None);
    let sample_format = crate::cog::tile_payload::ifd_sample_format(ifd)?;
    Ok(crate::cog::tile_encoding_from_opts(
        opts,
        tile_size,
        spp,
        Some(predictor),
        sample_format,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray::arr3;

    #[test]
    fn permute_and_pad_reorders_bands_in_one_pass() {
        let data = arr3(&[[[1u8, 2, 3]], [[4u8, 5, 6]]]);
        let out = permute_and_pad_tile_chunky(&data, &[3, 1], 2, 1, 2).unwrap();
        assert_eq!(out.len(), 2 * 2 * 2);
        // row0 col0: bands 3,1 -> 3,1
        assert_eq!(out[0], 3);
        assert_eq!(out[1], 1);
        // row1 col0
        assert_eq!(out[4], 6);
        assert_eq!(out[5], 4);
    }

    #[test]
    fn permute_rejects_out_of_range_band() {
        let data = arr3(&[[[1u8, 2]]]);
        assert!(permute_and_pad_tile_chunky(&data, &[3], 1, 1, 1).is_err());
    }
}
