use anyhow::Result;
use ndarray::{Array2, Array3};
use rayon::prelude::*;
use rayon::ThreadPool;
use tiff_core::SampleFormat;

use crate::cog::semantics::associated_alpha_band_index;
use crate::cog::{tile_jobs, TileJob};
use crate::crop::WriteWindow;
use crate::input::RasterProfile;
use crate::jp2::{Jp2Sample, Region};
use crate::jp2::Jp2Raster;

use super::mask::{
    alpha_u16_to_validity_pub, alpha_u8_to_validity, black_rgb_u16_to_validity_pub,
    black_rgb_u8_to_validity_pub,     nonzero_i16_to_validity, nonzero_u16_to_validity, nonzero_u8_to_validity, ValidityMask,
};
use super::options::{NodataValue, ResolvedValiditySource};

pub fn build_validity_mask_jp2(
    data: &[u8],
    raster: &Jp2Raster,
    profile: &RasterProfile,
    source: ResolvedValiditySource,
    window: Option<&WriteWindow>,
    nodata: Option<NodataValue>,
    zero_threshold: f64,
    tile_size: u32,
    pool: &ThreadPool,
) -> Result<ValidityMask> {
    let (width, height) = output_dimensions(profile, window);
    if source == ResolvedValiditySource::Full {
        return Ok(ValidityMask::filled(width, height, 1));
    }

    let (x_off, y_off) = window_offset(window);
    let jobs = tile_jobs(width as u32, height as u32, tile_size);
    let mut mask = ValidityMask::filled(width, height, 0);
    let tiles = pool.install(|| {
        jobs.par_iter()
            .map(|job| {
                let tile = read_validity_tile_jp2(
                    data,
                    raster,
                    profile,
                    source,
                    x_off,
                    y_off,
                    nodata,
                    zero_threshold,
                    job,
                )?;
                Ok((job.clone(), tile))
            })
            .collect::<Result<Vec<_>>>()
    })?;

    for (job, tile) in tiles {
        mask.write_tile(&job, &tile, job.cols);
    }
    Ok(mask)
}

fn output_dimensions(profile: &RasterProfile, window: Option<&WriteWindow>) -> (usize, usize) {
    match window {
        Some(win) => (win.width, win.height),
        None => (profile.width as usize, profile.height as usize),
    }
}

fn window_offset(window: Option<&WriteWindow>) -> (u32, u32) {
    match window {
        Some(win) => (win.col_off as u32, win.row_off as u32),
        None => (0, 0),
    }
}

fn read_validity_tile_jp2(
    data: &[u8],
    raster: &Jp2Raster,
    profile: &RasterProfile,
    source: ResolvedValiditySource,
    x_off: u32,
    y_off: u32,
    nodata: Option<NodataValue>,
    zero_threshold: f64,
    job: &TileJob,
) -> Result<Vec<u8>> {
    let region = Region {
        x0: x_off + job.col_off as u32,
        y0: y_off + job.row_off as u32,
        x1: x_off + job.col_off as u32 + job.cols as u32,
        y1: y_off + job.row_off as u32 + job.rows as u32,
    };
    let image = region.decode(data)?;
    match source {
        ResolvedValiditySource::AssociatedAlpha => {
            let band = associated_alpha_band_index(profile)
                .ok_or_else(|| anyhow::anyhow!("missing associated alpha band"))?;
            if raster.bits_per_sample <= 8 {
                let planes = u8::planes(&image, raster.sample_format, raster.bits_per_sample, Some(&[band]))?;
                let array = plane_to_array2(&planes.planes[0], job.rows, job.cols)?;
                Ok(alpha_u8_to_validity(&array))
            } else {
                let planes = u16::planes(&image, raster.sample_format, raster.bits_per_sample, Some(&[band]))?;
                let array = plane_to_array2(&planes.planes[0], job.rows, job.cols)?;
                Ok(alpha_u16_to_validity_pub(&array))
            }
        }
        ResolvedValiditySource::BlackRgb => decode_black_rgb_tile(
            &image,
            raster,
            job.rows,
            job.cols,
        ),
        ResolvedValiditySource::Nodata => {
            let nodata = nodata.ok_or_else(|| anyhow::anyhow!("missing nodata value"))?;
            decode_nodata_tile(&image, raster, nodata, job.rows, job.cols)
        }
        ResolvedValiditySource::NonZero => {
            decode_nonzero_tile(&image, raster, zero_threshold, job.rows, job.cols)
        }
        ResolvedValiditySource::DatasetMask => decode_jp2_bitmask_tile(&image, raster, job.rows, job.cols),
        ResolvedValiditySource::Full => {
            Ok(vec![1; job.rows * job.cols])
        }
    }
}

fn plane_to_array2<T: Copy + Default>(plane: &[T], rows: usize, cols: usize) -> Result<Array2<T>> {
    if plane.len() != rows * cols {
        anyhow::bail!(
            "JP2 tile size mismatch: expected {} values, got {}",
            rows * cols,
            plane.len()
        );
    }
    Array2::from_shape_vec((rows, cols), plane.to_vec())
        .map_err(|err| anyhow::anyhow!("failed to shape JP2 tile: {err}"))
}

fn decode_black_rgb_tile(
    image: &jpeg2k::Image,
    raster: &Jp2Raster,
    rows: usize,
    cols: usize,
) -> Result<Vec<u8>> {
    if raster.bits_per_sample <= 8 {
        let planes = u8::planes(image, raster.sample_format, raster.bits_per_sample, Some(&[1, 2, 3]))?;
        let mut array = Array3::<u8>::zeros((rows, cols, 3));
        for band in 0..3 {
            let plane = plane_to_array2(&planes.planes[band], rows, cols)?;
            for row in 0..rows {
                for col in 0..cols {
                    array[[row, col, band]] = plane[[row, col]];
                }
            }
        }
        Ok(black_rgb_u8_to_validity_pub(&array))
    } else {
        let planes = u16::planes(image, raster.sample_format, raster.bits_per_sample, Some(&[1, 2, 3]))?;
        let mut array = Array3::<u16>::zeros((rows, cols, 3));
        for band in 0..3 {
            let plane = plane_to_array2(&planes.planes[band], rows, cols)?;
            for row in 0..rows {
                for col in 0..cols {
                    array[[row, col, band]] = plane[[row, col]];
                }
            }
        }
        Ok(black_rgb_u16_to_validity_pub(&array))
    }
}

fn decode_nodata_tile(
    image: &jpeg2k::Image,
    raster: &Jp2Raster,
    nodata: NodataValue,
    rows: usize,
    cols: usize,
) -> Result<Vec<u8>> {
    match (raster.sample_format, raster.bits_per_sample) {
        (SampleFormat::Uint, 8) => {
            let planes = u8::planes(image, raster.sample_format, raster.bits_per_sample, Some(&[1]))?;
            let array = plane_to_array2(&planes.planes[0], rows, cols)?;
            Ok(array
                .iter()
                .map(|&value| u8::from(!nodata.matches_u8(value)))
                .collect())
        }
        (SampleFormat::Uint, 16) => {
            let planes = u16::planes(image, raster.sample_format, raster.bits_per_sample, Some(&[1]))?;
            let array = plane_to_array2(&planes.planes[0], rows, cols)?;
            Ok(array
                .iter()
                .map(|&value| u8::from(!nodata.matches_u16(value)))
                .collect())
        }
        (SampleFormat::Int, 16) => {
            let planes = i16::planes(image, raster.sample_format, raster.bits_per_sample, Some(&[1]))?;
            let array = plane_to_array2(&planes.planes[0], rows, cols)?;
            Ok(array
                .iter()
                .map(|&value| u8::from(!nodata.matches_i16(value)))
                .collect())
        }
        _ => Ok(vec![1; rows * cols]),
    }
}

fn decode_jp2_bitmask_tile(
    image: &jpeg2k::Image,
    raster: &Jp2Raster,
    rows: usize,
    cols: usize,
) -> Result<Vec<u8>> {
    let planes = u8::planes(image, raster.sample_format, raster.bits_per_sample, Some(&[1]))?;
    let array = plane_to_array2(&planes.planes[0], rows, cols)?;
    Ok(array.iter().map(|&value| u8::from(value > 0)).collect())
}

fn decode_nonzero_tile(
    image: &jpeg2k::Image,
    raster: &Jp2Raster,
    zero_threshold: f64,
    rows: usize,
    cols: usize,
) -> Result<Vec<u8>> {
    match (raster.sample_format, raster.bits_per_sample) {
        (SampleFormat::Uint, 8) => {
            let planes = u8::planes(image, raster.sample_format, raster.bits_per_sample, Some(&[1]))?;
            let array = plane_to_array2(&planes.planes[0], rows, cols)?;
            Ok(nonzero_u8_to_validity(&array, zero_threshold))
        }
        (SampleFormat::Uint, 16) => {
            let planes = u16::planes(image, raster.sample_format, raster.bits_per_sample, Some(&[1]))?;
            let array = plane_to_array2(&planes.planes[0], rows, cols)?;
            Ok(nonzero_u16_to_validity(&array, zero_threshold))
        }
        (SampleFormat::Int, 16) => {
            let planes = i16::planes(image, raster.sample_format, raster.bits_per_sample, Some(&[1]))?;
            let array = plane_to_array2(&planes.planes[0], rows, cols)?;
            Ok(nonzero_i16_to_validity(&array, zero_threshold))
        }
        _ => Ok(vec![1; rows * cols]),
    }
}
