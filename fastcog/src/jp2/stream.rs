use crate::config::Args;
use crate::jp2::decode::{self, Region, RgbPlanes};
use crate::jp2::profile::Jp2Raster;
use gdal_alt_core::cog::{
    apply_compression, configure_cog_with_levels, layer_sizes, overview_levels, tiff_variant,
};
use gdal_alt_core::input::{apply_georef, GeorefProfile};
use gdal_alt_core::progress::{ProgressTracker, StageBar};
use gdal_alt_core::util::ensure_parent_dir;
use anyhow::{Context, Result};
use geotiff_writer::cog::{pack_u8_planar_tile, LayerEncodePlan, PackedPlanarTile};
use geotiff_writer::GeoTiffBuilder;
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::File;
use std::io::BufWriter;
use std::sync::Arc;
use tiff_core::PlanarConfiguration;

const OUTPUT_BUFFER_BYTES: usize = 8 * 1024 * 1024;

pub fn convert(
    args: &Args,
    pool: &rayon::ThreadPool,
    mmap: Arc<Mmap>,
    raster: Jp2Raster,
    georef: GeorefProfile,
) -> Result<()> {
    let width = raster.width;
    let height = raster.height;
    let opts = args.cog_options();
    // OpenJPEG supports reduce factors 1..=4 (overview levels 2, 4, 8, 16).
    let levels: Vec<u32> = overview_levels(&opts, width, height)
        .into_iter()
        .take_while(|&level| level.trailing_zeros() <= 4)
        .collect();
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
            &opts,
        ),
        &georef,
    );

    let cog_builder = configure_cog_with_levels(base, &opts, levels.clone());
    let overview_sizes = layer_sizes(width, height, &levels);

    let output = {
        ensure_parent_dir(&args.output)?;
        File::create(&args.output)
            .with_context(|| format!("failed to create {}", args.output.display()))?
    };
    let sink = BufWriter::with_capacity(OUTPUT_BUFFER_BYTES, output);
    let mut stream = cog_builder.begin_planar_stream::<u8, _>(
        sink,
        width as usize,
        height as usize,
        &overview_sizes,
    )?;
    let encode_plan = stream.encode_plan();
    let progress = ProgressTracker::new(args.show_progress());

    let mmap_base = Arc::clone(&mmap);
    let mmap_overviews = Arc::clone(&mmap);
    let levels_for_overviews = levels.clone();
    let region_count = decode::regions(width, height).len() as u64;
    let decode_bar = progress.stage("JP2 decode", region_count);
    let overview_bar = if levels.is_empty() {
        StageBar::Noop
    } else {
        progress.stage("Overviews", levels.len() as u64)
    };
    let write_bar = progress.stage("Write COG", 1 + levels.len() as u64);

    // OpenJPEG is not safe for concurrent region decodes on one codestream, but each
    // reduce-level overview decode is an independent session and can run in parallel.
    let overview_packed = if levels_for_overviews.is_empty() {
        Vec::new()
    } else {
        pool.install(|| {
            levels_for_overviews
                .par_iter()
                .map(|&level| {
                    let packed = pack_overview(mmap_overviews.as_ref(), level, encode_plan)?;
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
        let ctx = TileGrid::new(width, height, encode_plan);
        let base_packed = encode_base_layer(mmap_base.as_ref(), &ctx, decode_bar.clone())?;
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
        .with_context(|| format!("failed to finalize COG {}", args.output.display()))?;

    write_bar.done("done");
    progress.finish();
    Ok(())
}

struct TileGrid {
    width: u32,
    height: u32,
    plan: LayerEncodePlan,
    tile_width: usize,
    tile_height: usize,
    tiles_across: usize,
    tiles_down: usize,
    tiles_per_plane: usize,
}

impl TileGrid {
    fn new(width: u32, height: u32, plan: LayerEncodePlan) -> Self {
        let tile_width = plan.tile_width;
        let tile_height = plan.tile_height;
        let tiles_across = (width as usize).div_ceil(tile_width);
        let tiles_down = (height as usize).div_ceil(tile_height);
        Self {
            width,
            height,
            plan,
            tile_width,
            tile_height,
            tiles_across,
            tiles_down,
            tiles_per_plane: tiles_across * tiles_down,
        }
    }
}

fn pack_overview(data: &[u8], level: u32, plan: LayerEncodePlan) -> Result<Vec<PackedPlanarTile>> {
    let image = decode::decode_overview(data, level.trailing_zeros())?;
    let width = image.width() as usize;
    let height = image.height() as usize;
    let planes = decode::rgb_planes(&image)?;
    decode::pack_rgb_planes(&planes, width, height, plan)
}

fn encode_base_layer(
    data: &[u8],
    grid: &TileGrid,
    progress: StageBar,
) -> Result<Vec<PackedPlanarTile>> {
    decode::regions(grid.width, grid.height)
        .par_iter()
        .map(|region| pack_region(data, region, grid))
        .inspect(|result| {
            if result.is_ok() {
                progress.inc(1);
            }
        })
        .collect::<Result<Vec<Vec<PackedPlanarTile>>>>()
        .map(|nested| nested.into_iter().flatten().collect())
}

fn pack_region(data: &[u8], region: &Region, grid: &TileGrid) -> Result<Vec<PackedPlanarTile>> {
    let image = region.decode(data)?;
    let RgbPlanes {
        planes,
        width: comp_w,
    } = decode::rgb_planes(&image)?;

    let tw = grid.tile_width;
    let th = grid.tile_height;
    let first_cog_col = region.x0 as usize / tw;
    let last_cog_col = (region.x1 as usize - 1) / tw;
    let first_cog_row = region.y0 as usize / th;
    let last_cog_row = (region.y1 as usize - 1) / th;

    let mut packed = Vec::new();
    for (band, plane) in planes.iter().enumerate() {
        for cog_row in first_cog_row..=last_cog_row.min(grid.tiles_down - 1) {
            for cog_col in first_cog_col..=last_cog_col.min(grid.tiles_across - 1) {
                let col_off = cog_col * tw;
                let row_off = cog_row * th;
                let cols = tw.min(grid.width as usize - col_off);
                let rows = th.min(grid.height as usize - row_off);
                let local_col = col_off - region.x0 as usize;
                let local_row = row_off - region.y0 as usize;

                let mut tile_data = vec![0u8; tw * th];
                for row in 0..rows {
                    let src_row = (local_row + row) * comp_w + local_col;
                    let dst_row = row * tw;
                    tile_data[dst_row..dst_row + cols]
                        .copy_from_slice(&plane[src_row..src_row + cols]);
                }

                let block_index =
                    band * grid.tiles_per_plane + cog_row * grid.tiles_across + cog_col;
                packed.push(
                    pack_u8_planar_tile(&tile_data, block_index, grid.plan)
                        .map_err(|err| anyhow::anyhow!(err))?,
                );
            }
        }
    }

    Ok(packed)
}
