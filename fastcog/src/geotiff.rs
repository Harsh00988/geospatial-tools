use crate::cog::{configure_cog, tile_jobs};
use crate::config::Args;
use crate::input::RasterProfile;
use crate::progress::{ProgressTracker, StageBar};
use anyhow::{bail, Context, Result};
use geotiff_reader::GeoTiffFile;
use ndarray::{Array2, Array3};
use rayon::prelude::*;
use tiff_core::SampleFormat;
use tiff_reader::TiffSample;

pub fn convert(args: &Args, pool: &rayon::ThreadPool) -> Result<()> {
    let input = open_input(args)?;
    let profile = RasterProfile::from_geotiff(&input)?;
    dispatch_by_sample(args, &input, &profile, pool)
}

fn open_input(args: &Args) -> Result<GeoTiffFile> {
    if args.mmap {
        unsafe { GeoTiffFile::open_mmap(&args.input) }
    } else {
        GeoTiffFile::open(&args.input)
    }
    .with_context(|| format!("failed to open {}", args.input.display()))
}

fn dispatch_by_sample(
    args: &Args,
    input: &GeoTiffFile,
    profile: &RasterProfile,
    pool: &rayon::ThreadPool,
) -> Result<()> {
    let bits = profile.sample.bits_per_sample;
    let format = profile.sample.sample_format;
    match (bits, format) {
        (8, SampleFormat::Uint) => convert_typed::<u8>(args, input, profile, pool),
        (8, SampleFormat::Int) => convert_typed::<i8>(args, input, profile, pool),
        (16, SampleFormat::Uint) => convert_typed::<u16>(args, input, profile, pool),
        (16, SampleFormat::Int) => convert_typed::<i16>(args, input, profile, pool),
        (32, SampleFormat::Uint) => convert_typed::<u32>(args, input, profile, pool),
        (32, SampleFormat::Int) => convert_typed::<i32>(args, input, profile, pool),
        (32, SampleFormat::Float) => convert_typed::<f32>(args, input, profile, pool),
        (64, SampleFormat::Uint) => convert_typed::<u64>(args, input, profile, pool),
        (64, SampleFormat::Int) => convert_typed::<i64>(args, input, profile, pool),
        (64, SampleFormat::Float) => convert_typed::<f64>(args, input, profile, pool),
        _ => bail!(
            "unsupported sample layout: {bits} bits, {format:?} format ({}x{}x{} image)",
            profile.width,
            profile.height,
            profile.bands
        ),
    }
}

fn convert_typed<T>(
    args: &Args,
    input: &GeoTiffFile,
    profile: &RasterProfile,
    pool: &rayon::ThreadPool,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Send + Sync,
{
    let width = profile.width;
    let height = profile.height;
    let bands = profile.bands as usize;
    let tile_size = args.blocksize;

    let cog_builder = configure_cog(profile.base_builder(args), args, width, height);
    let mut writer = cog_builder
        .tile_writer_file::<T, _>(&args.output)
        .with_context(|| format!("failed to create COG writer for {}", args.output.display()))?;

    let tiles = tile_jobs(width, height, tile_size);
    let progress = ProgressTracker::new(args.show_progress());
    let read_bar = progress.stage("Read tiles", tiles.len() as u64);
    let write_bar = progress.stage("Write COG", tiles.len() as u64 + 1);

    let mut decoded = pool.install(|| {
        tiles
            .par_iter()
            .map(|job| read_tile(input, bands, job, &read_bar))
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
    bands: usize,
    job: &crate::cog::TileJob,
    read_bar: &StageBar,
) -> Result<(usize, usize, TileWindow<T>)>
where
    T: TiffSample,
{
    let crate::cog::TileJob {
        col_off,
        row_off,
        cols,
        rows,
    } = *job;

    let tile = if bands == 1 {
        let window = input.read_window::<T>(row_off, col_off, rows, cols)?;
        TileWindow::Single(
            window
                .into_dimensionality::<ndarray::Ix2>()
                .context("expected 2D raster window")?,
        )
    } else {
        let window = input.read_window::<T>(row_off, col_off, rows, cols)?;
        TileWindow::Multi(
            window
                .into_dimensionality::<ndarray::Ix3>()
                .context("expected [rows, cols, bands] raster window")?,
        )
    };

    read_bar.inc(1);
    Ok((col_off, row_off, tile))
}

enum TileWindow<T> {
    Single(Array2<T>),
    Multi(Array3<T>),
}
