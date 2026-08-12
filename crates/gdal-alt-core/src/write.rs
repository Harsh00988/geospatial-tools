use std::path::Path;

use anyhow::{bail, Context, Result};
use geotiff_reader::GeoTiffFile;
use ndarray::{Array2, Array3, Axis};
use rayon::prelude::*;
use tiff_core::SampleFormat;
use tiff_reader::TiffSample;

use crate::cog::{configure_cog, tile_jobs, CogOutputOptions};
use crate::crop::WriteWindow;
use crate::input::RasterProfile;
use crate::open::open_geotiff;
use crate::progress::{ProgressTracker, StageBar};

pub struct ConvertRequest<'a> {
    pub input: &'a Path,
    pub output: &'a Path,
    pub opts: &'a CogOutputOptions,
    pub mmap: bool,
    pub show_progress: bool,
    pub window: Option<WriteWindow>,
    pub bands: Option<Vec<usize>>,
}

pub fn convert_geotiff(pool: &rayon::ThreadPool, request: &ConvertRequest<'_>) -> Result<()> {
    request.opts.validate()?;
    let input = open_geotiff(request.input, request.mmap)?;
    let mut profile = RasterProfile::from_geotiff(&input)?;
    if let Some(window) = &request.window {
        profile = profile.with_window(window);
    }
    if let Some(bands) = &request.bands {
        validate_bands(bands, input.band_count() as usize)?;
        profile = profile.with_band_subset(bands);
    }

    if crate::remux::try_remux_cog(
        &input,
        request.output,
        &profile,
        request.opts,
        request.window.as_ref(),
        request.bands.as_deref(),
    )? {
        return Ok(());
    }

    dispatch_by_sample(pool, request, &input, &profile)
}

fn validate_bands(bands: &[usize], band_count: usize) -> Result<()> {
    if bands.is_empty() {
        bail!("at least one band must be selected");
    }
    for band in bands {
        if *band == 0 || *band > band_count {
            bail!("band {band} is out of range (1..={band_count})");
        }
    }
    Ok(())
}

fn dispatch_by_sample(
    pool: &rayon::ThreadPool,
    request: &ConvertRequest<'_>,
    input: &GeoTiffFile,
    profile: &RasterProfile,
) -> Result<()> {
    let bits = profile.sample.bits_per_sample;
    let format = profile.sample.sample_format;
    match (bits, format) {
        (8, SampleFormat::Uint) => convert_typed::<u8>(pool, request, input, profile),
        (8, SampleFormat::Int) => convert_typed::<i8>(pool, request, input, profile),
        (16, SampleFormat::Uint) => convert_typed::<u16>(pool, request, input, profile),
        (16, SampleFormat::Int) => convert_typed::<i16>(pool, request, input, profile),
        (32, SampleFormat::Uint) => convert_typed::<u32>(pool, request, input, profile),
        (32, SampleFormat::Int) => convert_typed::<i32>(pool, request, input, profile),
        (32, SampleFormat::Float) => convert_typed::<f32>(pool, request, input, profile),
        (64, SampleFormat::Uint) => convert_typed::<u64>(pool, request, input, profile),
        (64, SampleFormat::Int) => convert_typed::<i64>(pool, request, input, profile),
        (64, SampleFormat::Float) => convert_typed::<f64>(pool, request, input, profile),
        _ => bail!(
            "unsupported sample layout: {bits} bits, {format:?} format ({}x{}x{} image)",
            profile.width,
            profile.height,
            profile.bands
        ),
    }
}

fn convert_typed<T>(
    pool: &rayon::ThreadPool,
    request: &ConvertRequest<'_>,
    input: &GeoTiffFile,
    profile: &RasterProfile,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Send + Sync + Clone,
{
    let width = profile.width;
    let height = profile.height;
    let out_bands = profile.bands as usize;
    let tile_size = request.opts.blocksize;
    let window = request.window;
    let band_map = request.bands.clone();

    let cog_builder = configure_cog(profile.base_builder(request.opts), request.opts, width, height);
    let mut writer = cog_builder
        .tile_writer_file::<T, _>(request.output)
        .with_context(|| format!("failed to create COG writer for {}", request.output.display()))?;

    let tiles = tile_jobs(width, height, tile_size);
    let progress = ProgressTracker::new(request.show_progress);
    let read_bar = progress.stage("Read tiles", tiles.len() as u64);
    let write_bar = progress.stage("Write COG", tiles.len() as u64 + 1);

    let mut decoded = pool.install(|| {
        tiles
            .par_iter()
            .map(|job| read_tile(input, out_bands, window, band_map.as_deref(), job, &read_bar))
            .collect::<Result<Vec<_>>>()
    })?;
    read_bar.done("done");

    decoded.sort_by_key(|(col, row, _)| (*row, *col));

    for (col_off, row_off, tile) in decoded {
        match tile {
            TileWindow::Single(data) => writer
                .write_tile(col_off, row_off, &data.view())
                .context("failed to write COG tile")?,
            TileWindow::Multi(data) => writer
                .write_tile_3d(col_off, row_off, &data.view())
                .context("failed to write COG tile")?,
        }
        write_bar.inc(1);
    }

    writer.finish().context("failed to finalize COG")?;
    write_bar.inc(1);
    write_bar.done("done");
    progress.finish();
    Ok(())
}

fn read_tile<T>(
    input: &GeoTiffFile,
    out_bands: usize,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    job: &crate::cog::TileJob,
    read_bar: &StageBar,
) -> Result<(usize, usize, TileWindow<T>)>
where
    T: TiffSample + Clone,
{
    let crate::cog::TileJob {
        col_off,
        row_off,
        cols,
        rows,
    } = *job;

    let (src_col, src_row) = match window {
        Some(w) => (w.col_off + col_off, w.row_off + row_off),
        None => (col_off, row_off),
    };

    let tile = if out_bands == 1 {
        let data = if let Some(bands) = band_map {
            let band_index = bands[0] - 1;
            input.read_band_window::<T>(band_index, src_row, src_col, rows, cols)?
        } else {
            input.read_window::<T>(src_row, src_col, rows, cols)?
        };
        let data = data
            .into_dimensionality::<ndarray::Ix2>()
            .context("expected 2D raster window")?;
        TileWindow::Single(data)
    } else {
        let window_data = input.read_window::<T>(src_row, src_col, rows, cols)?;
        let data = window_data
            .into_dimensionality::<ndarray::Ix3>()
            .context("expected [rows, cols, bands] raster window")?;
        let data = if let Some(bands) = band_map {
            select_bands(&data, bands)?
        } else {
            data
        };
        TileWindow::Multi(data)
    };

    read_bar.inc(1);
    Ok((col_off, row_off, tile))
}

fn select_bands<T: Clone>(data: &Array3<T>, bands: &[usize]) -> Result<Array3<T>> {
    let mut slices = Vec::with_capacity(bands.len());
    for band in bands {
        let index = band - 1;
        if index >= data.len_of(Axis(2)) {
            bail!("band {band} is not present in decoded window");
        }
        slices.push(data.index_axis(Axis(2), index).to_owned());
    }
    ndarray::stack(Axis(2), &slices.iter().map(|s| s.view()).collect::<Vec<_>>())
        .context("failed to stack band subset")
}

enum TileWindow<T> {
    Single(Array2<T>),
    Multi(Array3<T>),
}
