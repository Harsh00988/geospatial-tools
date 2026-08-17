use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use geotiff_writer::cog::{pack_planar_tile, LayerEncodePlan, PackedPlanarTile};
use geotiff_writer::GeoTiffBuilder;
use ndarray::Array2;
use rayon::prelude::*;
use tiff_core::{PlanarConfiguration, SampleFormat};

use crate::cog::{
    apply_compression, configure_cog_with_levels, layer_sizes, overview_levels, CogOutputOptions,
    ResamplingChoice, tiff_variant,
};
use crate::crop::{shift_transform, WriteWindow};
use crate::input::{apply_georef, GeorefProfile};
use crate::jp2::decode::{self, Jp2Sample, Planes, Region};
use crate::jp2::profile::Jp2Raster;
use crate::jp2::source::Jp2Source;
use crate::progress::{ProgressTracker, StageBar};
use crate::resample::downsample_2d;
use crate::util::ensure_parent_dir;

const OUTPUT_BUFFER_BYTES: usize = 8 * 1024 * 1024;

pub fn convert(
    pool: &rayon::ThreadPool,
    source: &Jp2Source,
    input_path: Option<&Path>,
    output: &Path,
    opts: &CogOutputOptions,
    window: Option<WriteWindow>,
    bands: Option<&[usize]>,
    show_progress: bool,
) -> Result<()> {
    let data = source.as_ref();
    let mut raster = Jp2Raster::open(data)?;
    if let Some(selected) = bands {
        raster = raster.with_band_subset(selected)?;
    }

    let georef = super::resolve_georef(data, input_path)?;
    let (width, height, x_off, y_off, georef) = output_geometry(&raster, window, georef)?;

    match (raster.bits_per_sample, raster.sample_format) {
        (8, SampleFormat::Uint) => {
            convert_typed::<u8>(pool, source, raster, georef, width, height, x_off, y_off, output, opts, bands, show_progress)
        }
        (8, SampleFormat::Int) => {
            convert_typed::<i8>(pool, source, raster, georef, width, height, x_off, y_off, output, opts, bands, show_progress)
        }
        (12 | 16, SampleFormat::Uint) => {
            convert_typed::<u16>(pool, source, raster, georef, width, height, x_off, y_off, output, opts, bands, show_progress)
        }
        (12 | 16, SampleFormat::Int) => {
            convert_typed::<i16>(pool, source, raster, georef, width, height, x_off, y_off, output, opts, bands, show_progress)
        }
        (bits, format) => anyhow::bail!("unsupported JP2 sample layout: {bits}-bit {format:?}"),
    }
}

fn output_geometry(
    raster: &Jp2Raster,
    window: Option<WriteWindow>,
    mut georef: GeorefProfile,
) -> Result<(u32, u32, u32, u32, GeorefProfile)> {
    match window {
        None => Ok((raster.width, raster.height, 0, 0, georef)),
        Some(window) => {
            if let Some(affine) = georef.affine.as_mut() {
                *affine = shift_transform(affine, window.col_off, window.row_off);
            }
            Ok((
                window.width as u32,
                window.height as u32,
                window.col_off as u32,
                window.row_off as u32,
                georef,
            ))
        }
    }
}

fn convert_typed<T: Jp2Sample>(
    pool: &rayon::ThreadPool,
    source: &Jp2Source,
    raster: Jp2Raster,
    georef: GeorefProfile,
    width: u32,
    height: u32,
    x_off: u32,
    y_off: u32,
    output: &Path,
    opts: &CogOutputOptions,
    bands: Option<&[usize]>,
    show_progress: bool,
) -> Result<()> {
    let full_width = raster.width;
    let full_height = raster.height;
    let levels = overview_levels(opts, width, height);
    let tile_size = opts.blocksize;

    let base = apply_georef(
        apply_compression(
            GeoTiffBuilder::new(width, height)
                .bands(raster.bands)
                .tile_size(tile_size, tile_size)
                .photometric(raster.photometric)
                .planar_configuration(PlanarConfiguration::Planar)
                .tiff_variant(tiff_variant(
                    width,
                    height,
                    raster.bands,
                    u16::from(raster.bits_per_sample),
                )),
            opts,
            raster.sample_format,
        ),
        &georef,
    );

    let cog_builder = configure_cog_with_levels(base, opts, levels.clone());
    let overview_sizes = layer_sizes(width, height, &levels);

    ensure_parent_dir(output)?;
    let file = File::create(output)
        .with_context(|| format!("failed to create {}", output.display()))?;
    let sink = BufWriter::with_capacity(OUTPUT_BUFFER_BYTES, file);
    let mut stream = cog_builder.begin_planar_stream::<T, _>(
        sink,
        width as usize,
        height as usize,
        &overview_sizes,
    )?;
    let encode_plan = stream.encode_plan();
    let progress = ProgressTracker::new(show_progress);
    let shared = Arc::new(source.clone());

    let levels_for_overviews = levels.clone();
    let region_count = decode::regions(width, height, x_off, y_off).len() as u64;
    let decode_bar = progress.stage("JP2 decode", region_count);
    let overview_bar = if levels.is_empty() {
        StageBar::Noop
    } else {
        progress.stage("Overviews", levels.len() as u64)
    };
    let write_bar = progress.stage("Write COG", 1 + levels.len() as u64);

    let overview_packed = if levels_for_overviews.is_empty() {
        Vec::new()
    } else {
        pool.install(|| {
            levels_for_overviews
                .par_iter()
                .map(|&level| {
                    let packed = pack_overview::<T>(
                        shared.as_ref().as_ref(),
                        full_width,
                        full_height,
                        level,
                        encode_plan,
                        raster.sample_format,
                        raster.bits_per_sample,
                        bands,
                        opts.resampling,
                    )?;
                    overview_bar.inc(1);
                    Ok(packed)
                })
                .collect::<Result<Vec<_>>>()
        })?
    };
    if !levels_for_overviews.is_empty() {
        overview_bar.done("done");
    }

    let base_packed = pool.install(|| -> Result<Vec<PackedPlanarTile>> {
        let ctx = TileGrid::new(width, height, x_off, y_off, encode_plan);
        let base_packed = encode_base_layer::<T>(
            shared.as_ref().as_ref(),
            &ctx,
            raster.sample_format,
            raster.bits_per_sample,
            bands,
            decode_bar.clone(),
        )?;
        decode_bar.done("done");
        Ok(base_packed)
    })?;

    stream.commit_layer(0, base_packed)?;
    write_bar.inc(1);

    for (idx, packed) in overview_packed.into_iter().enumerate() {
        stream.commit_layer(1 + idx, packed)?;
        write_bar.inc(1);
    }

    stream
        .finish()
        .with_context(|| format!("failed to finalize COG {}", output.display()))?;

    write_bar.done("done");
    progress.finish();
    Ok(())
}

struct TileGrid {
    width: u32,
    height: u32,
    x_off: u32,
    y_off: u32,
    plan: LayerEncodePlan,
    tile_width: usize,
    tile_height: usize,
    tiles_across: usize,
    tiles_down: usize,
    tiles_per_plane: usize,
}

impl TileGrid {
    fn new(width: u32, height: u32, x_off: u32, y_off: u32, plan: LayerEncodePlan) -> Self {
        let tile_width = plan.tile_width;
        let tile_height = plan.tile_height;
        let tiles_across = (width as usize).div_ceil(tile_width);
        let tiles_down = (height as usize).div_ceil(tile_height);
        Self {
            width,
            height,
            x_off,
            y_off,
            plan,
            tile_width,
            tile_height,
            tiles_across,
            tiles_down,
            tiles_per_plane: tiles_across * tiles_down,
        }
    }
}

fn pack_overview<T: Jp2Sample>(
    data: &[u8],
    full_width: u32,
    full_height: u32,
    level: u32,
    plan: LayerEncodePlan,
    sample_format: SampleFormat,
    bits_per_sample: u8,
    bands: Option<&[usize]>,
    resampling: ResamplingChoice,
) -> Result<Vec<PackedPlanarTile>> {
    let reduce = decode::openjpeg_reduce(level);
    let image = decode::decode_overview(data, reduce)?;
    let mut width = image.width() as usize;
    let mut height = image.height() as usize;
    let mut planes = T::planes(&image, sample_format, bits_per_sample, bands)?;

    let target_w = (full_width / level).max(1) as usize;
    let target_h = (full_height / level).max(1) as usize;
    let decoded_factor = 1u32 << reduce;
    if decoded_factor < level {
        planes = downsample_planes::<T>(&planes, width, height, target_w, target_h, resampling)?;
        width = target_w;
        height = target_h;
    }

    T::pack_planes(&planes, width, height, plan)
}

fn downsample_planes<T: Jp2Sample>(
    planes: &Planes<T>,
    width: usize,
    height: usize,
    target_w: usize,
    target_h: usize,
    resampling: ResamplingChoice,
) -> Result<Planes<T>> {
    let scale_x = width.div_ceil(target_w).max(1);
    let scale_y = height.div_ceil(target_h).max(1);
    let scale = scale_x.max(scale_y);
    let mut out_planes = Vec::with_capacity(planes.planes.len());
    for plane in &planes.planes {
        let array = Array2::from_shape_vec((height, width), plane.clone())
            .context("invalid JP2 overview plane shape")?;
        let down = downsample_2d(&array, target_h, target_w, scale, resampling, None);
        out_planes.push(down.into_raw_vec_and_offset().0);
    }
    Ok(Planes {
        planes: out_planes,
        width: target_w,
    })
}

fn encode_base_layer<T: Jp2Sample>(
    data: &[u8],
    grid: &TileGrid,
    sample_format: SampleFormat,
    bits_per_sample: u8,
    bands: Option<&[usize]>,
    progress: StageBar,
) -> Result<Vec<PackedPlanarTile>> {
    decode::regions(grid.width, grid.height, grid.x_off, grid.y_off)
        .par_iter()
        .map(|region| {
            pack_region::<T>(
                data,
                region,
                grid,
                sample_format,
                bits_per_sample,
                bands,
            )
        })
        .inspect(|result| {
            if result.is_ok() {
                progress.inc(1);
            }
        })
        .collect::<Result<Vec<Vec<PackedPlanarTile>>>>()
        .map(|nested| nested.into_iter().flatten().collect())
}

fn pack_region<T: Jp2Sample>(
    data: &[u8],
    region: &Region,
    grid: &TileGrid,
    sample_format: SampleFormat,
    bits_per_sample: u8,
    bands: Option<&[usize]>,
) -> Result<Vec<PackedPlanarTile>> {
    let image = region.decode(data)?;
    let Planes { planes, width: comp_w } =
        T::planes(&image, sample_format, bits_per_sample, bands)?;

    let tw = grid.tile_width;
    let th = grid.tile_height;
    let first_cog_col = (region.x0 - grid.x_off) as usize / tw;
    let last_cog_col = (region.x1 - grid.x_off - 1) as usize / tw;
    let first_cog_row = (region.y0 - grid.y_off) as usize / th;
    let last_cog_row = (region.y1 - grid.y_off - 1) as usize / th;

    let mut packed = Vec::new();
    for (band, plane) in planes.iter().enumerate() {
        for cog_row in first_cog_row..=last_cog_row.min(grid.tiles_down - 1) {
            for cog_col in first_cog_col..=last_cog_col.min(grid.tiles_across - 1) {
                let col_off = cog_col * tw;
                let row_off = cog_row * th;
                let cols = tw.min(grid.width as usize - col_off);
                let rows = th.min(grid.height as usize - row_off);
                let local_col = col_off + grid.x_off as usize - region.x0 as usize;
                let local_row = row_off + grid.y_off as usize - region.y0 as usize;

                let mut tile_data = vec![T::default(); tw * th];
                for row in 0..rows {
                    let src_row = (local_row + row) * comp_w + local_col;
                    let dst_row = row * tw;
                    tile_data[dst_row..dst_row + cols]
                        .copy_from_slice(&plane[src_row..src_row + cols]);
                }

                let block_index =
                    band * grid.tiles_per_plane + cog_row * grid.tiles_across + cog_col;
                packed.push(
                    pack_planar_tile(&tile_data, block_index, grid.plan)
                        .map_err(|err| anyhow::anyhow!(err))?,
                );
            }
        }
    }

    Ok(packed)
}
