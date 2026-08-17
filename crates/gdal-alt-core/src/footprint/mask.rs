use anyhow::Result;
use geotiff_reader::GeoTiffFile;
use ndarray::{Array2, Array3};
use rayon::prelude::*;
use rayon::ThreadPool;
use tiff_reader::Ifd;

use crate::cog::mask::discover_dataset_masks;
use crate::cog::semantics::associated_alpha_band_index;
use crate::cog::{tile_jobs, TileJob};
use crate::crop::WriteWindow;
use crate::input::RasterProfile;

use super::options::{NodataValue, ResolvedValiditySource};

pub struct ValidityMask {
    pub width: usize,
    pub height: usize,
    bytes: Vec<u8>,
}

impl ValidityMask {
    pub fn filled(width: usize, height: usize, value: u8) -> Self {
        Self {
            width,
            height,
            bytes: vec![value; width * height],
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub fn valid_count(&self) -> usize {
        self.bytes.iter().filter(|&&v| v != 0).count()
    }

    pub fn all_valid(&self) -> bool {
        self.valid_count() == self.width.saturating_mul(self.height)
    }

    pub fn none_valid(&self) -> bool {
        self.valid_count() == 0
    }

    fn write_tile(&mut self, job: &TileJob, tile: &[u8], cols: usize) {
        for row in 0..job.rows {
            let dst_row = job.row_off + row;
            let src_start = row * cols;
            let dst_start = dst_row * self.width + job.col_off;
            self.bytes[dst_start..dst_start + job.cols]
                .copy_from_slice(&tile[src_start..src_start + job.cols]);
        }
    }
}

pub fn build_validity_mask(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    source: ResolvedValiditySource,
    window: Option<&WriteWindow>,
    nodata: Option<NodataValue>,
    tile_size: u32,
    pool: &ThreadPool,
) -> Result<ValidityMask> {
    let (width, height) = output_dimensions(profile, window);
    if source == ResolvedValiditySource::Full {
        return Ok(ValidityMask::filled(width, height, 1));
    }

    let jobs = tile_jobs(width as u32, height as u32, tile_size);
    let mut mask = ValidityMask::filled(width, height, 0);
    let tiles = pool.install(|| {
        jobs.par_iter()
            .map(|job| {
                let tile = read_validity_tile(input, profile, source, window, nodata, job)?;
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

fn read_validity_tile(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    source: ResolvedValiditySource,
    window: Option<&WriteWindow>,
    nodata: Option<NodataValue>,
    job: &TileJob,
) -> Result<Vec<u8>> {
    let (src_col, src_row) = source_origin(window, job);
    match source {
        ResolvedValiditySource::DatasetMask => read_dataset_mask_tile(input, src_row, src_col, job),
        ResolvedValiditySource::AssociatedAlpha => {
            read_alpha_tile(input, profile, src_row, src_col, job)
        }
        ResolvedValiditySource::BlackRgb => {
            read_black_rgb_tile(input, profile, src_row, src_col, job)
        }
        ResolvedValiditySource::Nodata => {
            let nodata = nodata.ok_or_else(|| anyhow::anyhow!("missing nodata value"))?;
            read_nodata_tile(input, profile, nodata, src_row, src_col, job)
        }
        ResolvedValiditySource::Full => Ok(vec![1; job.rows * job.cols]),
    }
}

fn source_origin(window: Option<&WriteWindow>, job: &TileJob) -> (usize, usize) {
    match window {
        Some(win) => (win.col_off + job.col_off, win.row_off + job.row_off),
        None => (job.col_off, job.row_off),
    }
}

fn read_dataset_mask_tile(
    input: &GeoTiffFile,
    src_row: usize,
    src_col: usize,
    job: &TileJob,
) -> Result<Vec<u8>> {
    let masks = discover_dataset_masks(input)
        .ok_or_else(|| anyhow::anyhow!("dataset mask IFD missing"))?;
    let tiff = input.tiff();
    let ifd = tiff
        .ifd(masks.base_ifd_index)
        .map_err(|err| anyhow::anyhow!("mask IFD: {err}"))?;
    read_mask_ifd_tile(tiff, ifd, src_row, src_col, job)
}

fn read_mask_ifd_tile(
    tiff: &tiff_reader::TiffFile,
    ifd: &Ifd,
    src_row: usize,
    src_col: usize,
    job: &TileJob,
) -> Result<Vec<u8>> {
    let data = tiff.read_window_from_ifd::<u8>(ifd, src_row, src_col, job.rows, job.cols)?;
    let array = data
        .into_dimensionality::<ndarray::Ix2>()
        .map_err(|err| anyhow::anyhow!("mask window must be 2D: {err}"))?;
    Ok(binarize_mask_array(&array))
}

fn binarize_mask_array(array: &Array2<u8>) -> Vec<u8> {
    array
        .iter()
        .map(|&value| u8::from(value != 0))
        .collect()
}

fn read_alpha_tile(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    src_row: usize,
    src_col: usize,
    job: &TileJob,
) -> Result<Vec<u8>> {
    let band = associated_alpha_band_index(profile)
        .ok_or_else(|| anyhow::anyhow!("missing associated alpha band"))?;
    let band_index = band - 1;
    if profile.sample.bits_per_sample <= 8 {
        let data = input.read_band_window::<u8>(band_index, src_row, src_col, job.rows, job.cols)?;
        let array = data
            .into_dimensionality::<ndarray::Ix2>()
            .map_err(|err| anyhow::anyhow!("alpha window must be 2D: {err}"))?;
        Ok(alpha_to_validity(&array))
    } else {
        let data = input.read_band_window::<u16>(band_index, src_row, src_col, job.rows, job.cols)?;
        let array = data
            .into_dimensionality::<ndarray::Ix2>()
            .map_err(|err| anyhow::anyhow!("alpha window must be 2D: {err}"))?;
        Ok(alpha_u16_to_validity(&array))
    }
}

fn alpha_to_validity(array: &Array2<u8>) -> Vec<u8> {
    array.iter().map(|&value| u8::from(value > 0)).collect()
}

fn alpha_u16_to_validity(array: &Array2<u16>) -> Vec<u8> {
    array.iter().map(|&value| u8::from(value > 0)).collect()
}

fn read_black_rgb_tile(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    src_row: usize,
    src_col: usize,
    job: &TileJob,
) -> Result<Vec<u8>> {
    if profile.sample.bits_per_sample <= 8 {
        let data = input.read_window::<u8>(src_row, src_col, job.rows, job.cols)?;
        let array = data
            .into_dimensionality::<ndarray::Ix3>()
            .map_err(|err| anyhow::anyhow!("RGB window must be 3D: {err}"))?;
        return Ok(black_rgb_u8_to_validity(&array));
    }
    if profile.sample.bits_per_sample <= 16 {
        let data = input.read_window::<u16>(src_row, src_col, job.rows, job.cols)?;
        let array = data
            .into_dimensionality::<ndarray::Ix3>()
            .map_err(|err| anyhow::anyhow!("RGB window must be 3D: {err}"))?;
        return Ok(black_rgb_u16_to_validity(&array));
    }
    Ok(vec![1; job.rows * job.cols])
}

fn black_rgb_u8_to_validity(array: &Array3<u8>) -> Vec<u8> {
    let rows = array.shape()[0];
    let cols = array.shape()[1];
    let mut out = Vec::with_capacity(rows * cols);
    for row in 0..rows {
        for col in 0..cols {
            let valid = !(array[[row, col, 0]] == 0
                && array[[row, col, 1]] == 0
                && array[[row, col, 2]] == 0);
            out.push(u8::from(valid));
        }
    }
    out
}

fn black_rgb_u16_to_validity(array: &Array3<u16>) -> Vec<u8> {
    let rows = array.shape()[0];
    let cols = array.shape()[1];
    let mut out = Vec::with_capacity(rows * cols);
    for row in 0..rows {
        for col in 0..cols {
            let valid = !(array[[row, col, 0]] == 0
                && array[[row, col, 1]] == 0
                && array[[row, col, 2]] == 0);
            out.push(u8::from(valid));
        }
    }
    out
}

fn read_nodata_tile(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    nodata: NodataValue,
    src_row: usize,
    src_col: usize,
    job: &TileJob,
) -> Result<Vec<u8>> {
    match (profile.sample.sample_format, profile.sample.bits_per_sample) {
        (tiff_core::SampleFormat::Uint, 8) => {
            let data = input.read_band_window::<u8>(0, src_row, src_col, job.rows, job.cols)?;
            let array = data
                .into_dimensionality::<ndarray::Ix2>()
                .map_err(|err| anyhow::anyhow!("nodata window must be 2D: {err}"))?;
            Ok(array
                .iter()
                .map(|&value| u8::from(!nodata.matches_u8(value)))
                .collect())
        }
        (tiff_core::SampleFormat::Uint, 16) => {
            let data = input.read_band_window::<u16>(0, src_row, src_col, job.rows, job.cols)?;
            let array = data
                .into_dimensionality::<ndarray::Ix2>()
                .map_err(|err| anyhow::anyhow!("nodata window must be 2D: {err}"))?;
            Ok(array
                .iter()
                .map(|&value| u8::from(!nodata.matches_u16(value)))
                .collect())
        }
        (tiff_core::SampleFormat::Int, 16) => {
            let data = input.read_band_window::<i16>(0, src_row, src_col, job.rows, job.cols)?;
            let array = data
                .into_dimensionality::<ndarray::Ix2>()
                .map_err(|err| anyhow::anyhow!("nodata window must be 2D: {err}"))?;
            Ok(array
                .iter()
                .map(|&value| u8::from(!nodata.matches_i16(value)))
                .collect())
        }
        (tiff_core::SampleFormat::Float, 32) => {
            let data = input.read_band_window::<f32>(0, src_row, src_col, job.rows, job.cols)?;
            let array = data
                .into_dimensionality::<ndarray::Ix2>()
                .map_err(|err| anyhow::anyhow!("nodata window must be 2D: {err}"))?;
            Ok(array
                .iter()
                .map(|&value| u8::from(!nodata.matches_f32(value)))
                .collect())
        }
        (tiff_core::SampleFormat::Int, 8) => {
            let data = input.read_band_window::<i8>(0, src_row, src_col, job.rows, job.cols)?;
            let array = data
                .into_dimensionality::<ndarray::Ix2>()
                .map_err(|err| anyhow::anyhow!("nodata window must be 2D: {err}"))?;
            Ok(array
                .iter()
                .map(|&value| u8::from(!nodata.matches_i8(value)))
                .collect())
        }
        (tiff_core::SampleFormat::Uint, 32) => {
            let data = input.read_band_window::<u32>(0, src_row, src_col, job.rows, job.cols)?;
            let array = data
                .into_dimensionality::<ndarray::Ix2>()
                .map_err(|err| anyhow::anyhow!("nodata window must be 2D: {err}"))?;
            Ok(array
                .iter()
                .map(|&value| u8::from(!nodata.matches_u32(value)))
                .collect())
        }
        (tiff_core::SampleFormat::Int, 32) => {
            let data = input.read_band_window::<i32>(0, src_row, src_col, job.rows, job.cols)?;
            let array = data
                .into_dimensionality::<ndarray::Ix2>()
                .map_err(|err| anyhow::anyhow!("nodata window must be 2D: {err}"))?;
            Ok(array
                .iter()
                .map(|&value| u8::from(!nodata.matches_i32(value)))
                .collect())
        }
        (tiff_core::SampleFormat::Float, 64) => {
            let data = input.read_band_window::<f64>(0, src_row, src_col, job.rows, job.cols)?;
            let array = data
                .into_dimensionality::<ndarray::Ix2>()
                .map_err(|err| anyhow::anyhow!("nodata window must be 2D: {err}"))?;
            Ok(array
                .iter()
                .map(|&value| u8::from(!nodata.matches_f64(value)))
                .collect())
        }
        _ => Ok(vec![1; job.rows * job.cols]),
    }
}
