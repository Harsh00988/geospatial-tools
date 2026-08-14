use std::path::Path;

use anyhow::{bail, Context, Result};
use geotiff_reader::GeoTiffFile;
use tiff_core::SampleFormat;
use tiff_reader::TiffSample;

use crate::cog::CogOutputOptions;
use crate::crop::WriteWindow;
use crate::input::RasterProfile;
use crate::open::{open_input, GeoTiffHandle};
use crate::path::{log_convert_path, ConvertPath};
use crate::util::ensure_parent_dir;

pub struct ConvertRequest<'a> {
    pub input: &'a str,
    pub output: &'a Path,
    pub opts: &'a CogOutputOptions,
    pub mmap: bool,
    pub show_progress: bool,
    pub window: Option<WriteWindow>,
    pub bands: Option<Vec<usize>>,
}

pub struct ConvertResult {
    pub path: ConvertPath,
}

pub fn convert_geotiff(pool: &rayon::ThreadPool, request: &ConvertRequest<'_>) -> Result<ConvertResult> {
    request.opts.validate()?;
    ensure_parent_dir(request.output)?;
    let handle = open_input(request.input, request.mmap)?;
    let input = handle.as_file();
    let mut profile = RasterProfile::from_geotiff(input)?;
    if let Some(window) = &request.window {
        profile = profile.with_window(window);
    }
    if let Some(bands) = &request.bands {
        validate_bands(bands, input.band_count() as usize)?;
        profile = profile.with_band_subset(bands);
    }

    if let Some(path) = crate::remux::try_remux_cog(
        pool,
        input,
        request.output,
        &profile,
        request.opts,
        request.window.as_ref(),
        request.bands.as_deref(),
        request.show_progress,
    )? {
        log_convert_path(path, request.show_progress);
        return Ok(ConvertResult { path });
    }

    let path = dispatch_by_sample(pool, request, input, &profile, &handle)?;
    log_convert_path(path, request.show_progress);
    Ok(ConvertResult { path })
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
    _handle: &GeoTiffHandle,
) -> Result<ConvertPath> {
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
) -> Result<ConvertPath>
where
    T: TiffSample + geotiff_writer::NumericSample + Send + Sync + Clone + Copy + Default + PartialEq,
{
    let base_ifd = input
        .tiff()
        .ifd(input.base_ifd_index())
        .context("failed to read base IFD")?;
    crate::encode::convert_to_remux_cog::<T>(
        pool,
        input,
        request.output,
        profile,
        request.opts,
        request.window,
        request.bands.as_deref(),
        request.show_progress,
    )?;
    if base_ifd.is_tiled() {
        Ok(ConvertPath::TiledEncode)
    } else {
        Ok(ConvertPath::StripEncode)
    }
}
