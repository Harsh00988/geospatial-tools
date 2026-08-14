use std::sync::OnceLock;

use anyhow::{Context, Result};
use geotiff_reader::GeoTiffFile;
use ndarray::{Axis, s};
use tiff_core::Compression;
use tiff_reader::TiffSample;

use crate::cog::tile_payload::input_compression;
use crate::cog::TileJob;
use crate::crop::WriteWindow;

use super::tile::StripTile;
use super::tiles::select_bands;

pub(crate) const STRIP_DECODE_TILE_ROWS: usize = 4;

const DEFAULT_STRIP_DECODE_PARALLELISM: usize = 8;

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

pub(super) fn with_decode_permit<R>(run: impl FnOnce() -> R) -> R {
    let _permit = decode_concurrency().acquire();
    run()
}

fn decode_concurrency() -> &'static DecodeConcurrency {
    static GATE: OnceLock<DecodeConcurrency> = OnceLock::new();
    GATE.get_or_init(|| DecodeConcurrency::new(strip_decode_parallelism()))
}

fn strip_decode_parallelism() -> usize {
    std::env::var("FASTCOG_DECODE_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_STRIP_DECODE_PARALLELISM)
        .clamp(1, rayon::current_num_threads().max(1))
}

pub(super) fn strip_rows_per_strip(input: &GeoTiffFile) -> Result<usize> {
    let ifd = input.tiff().ifd(input.base_ifd_index())?;
    Ok(ifd.rows_per_strip().max(1) as usize)
}

/// Rows to decode per strip I/O window. Small strips decode in one shot; large strips
/// (e.g. ICEYE single-strip images) decode `STRIP_DECODE_TILE_ROWS` tile rows at a time.
pub(crate) fn strip_decode_window_rows(rows_per_strip: usize, tile_size: usize) -> usize {
    let max_window = tile_size.saturating_mul(STRIP_DECODE_TILE_ROWS).max(tile_size);
    rows_per_strip.min(max_window)
}

/// Use strip-window decode only for **compressed** single-strip images where each
/// row-group read would otherwise re-decompress the entire strip.
pub(super) fn should_use_strip_windowed_decode(
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

pub(super) fn should_use_tiled_source_decode(input: &GeoTiffFile, window: Option<WriteWindow>) -> bool {
    if window.is_some() {
        return false;
    }
    input
        .tiff()
        .ifd(input.base_ifd_index())
        .map(|ifd| ifd.is_tiled())
        .unwrap_or(false)
}

pub(super) fn read_decoded_strip<T>(
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

pub(super) fn slice_strip_tile<T: Clone>(
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
