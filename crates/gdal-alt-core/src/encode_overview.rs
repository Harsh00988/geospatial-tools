use anyhow::Result;
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{RemuxCompressedBlock, RemuxTileEncoding};
use tiff_reader::TiffSample;

use crate::cog::CogOutputOptions;
use crate::crop::WriteWindow;
use crate::remux::resolve_overview_read_source;
use crate::spool::LayerBlockSpool;
use crate::strip_encode::{
    build_base_layer_from_rows, build_strip_overview_from_decoded,
    build_strip_overview_layer_with_cache, StripTile,
};

/// Whether the next overview can be built from an in-memory parent pyramid level.
pub fn should_chain_from_parent(input: &GeoTiffFile, parent_level: u32, next_level: u32) -> bool {
    if next_level != parent_level.saturating_mul(2) {
        return false;
    }
    match resolve_overview_read_source(input, next_level) {
        Ok((_, factor, downsample)) => !(factor == next_level && downsample == 1),
        Err(_) => true,
    }
}

pub fn encode_overview_layers_to_spool<T>(
    input: &GeoTiffFile,
    spool: &mut LayerBlockSpool,
    width: u32,
    height: u32,
    tile_size: usize,
    out_bands: usize,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    opts: &CogOutputOptions,
    levels: &[u32],
    nodata: Option<T>,
    progress: Option<&crate::progress::StageBar>,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
{
    spool.write_layer(build_base_layer_from_rows::<T>(
        input,
        width,
        height,
        tile_size,
        out_bands,
        window,
        band_map,
        encoding,
        progress,
    )?)?;

    let mut parent_decoded: Option<Vec<(usize, usize, StripTile<T>)>> = None;
    let mut parent_level = 1u32;
    for (level_idx, &level) in levels.iter().enumerate() {
        let next_level = levels.get(level_idx + 1).copied();
        let chain_next = next_level.is_some_and(|next| should_chain_from_parent(input, level, next));

        let (layer, decoded) = if let Some(parent) = parent_decoded.take() {
            if level == parent_level.saturating_mul(2) {
                build_strip_overview_from_decoded::<T>(
                    parent,
                    width,
                    height,
                    level,
                    parent_level,
                    tile_size,
                    out_bands,
                    encoding,
                    opts,
                    nodata,
                    chain_next,
                    progress,
                )?
            } else {
                build_strip_overview_layer_with_cache::<T>(
                    input,
                    width,
                    height,
                    level,
                    tile_size,
                    out_bands,
                    window,
                    band_map,
                    encoding,
                    opts,
                    nodata,
                    chain_next,
                    progress,
                )?
            }
        } else {
            build_strip_overview_layer_with_cache::<T>(
                input,
                width,
                height,
                level,
                tile_size,
                out_bands,
                window,
                band_map,
                encoding,
                opts,
                nodata,
                chain_next,
                progress,
            )?
        };

        spool.write_layer(layer)?;
        if chain_next {
            parent_decoded = Some(decoded);
            parent_level = level;
        }
    }

    Ok(())
}

pub fn encode_layers_with_spool<T>(
    input: &GeoTiffFile,
    width: u32,
    height: u32,
    tile_size: usize,
    out_bands: usize,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    opts: &CogOutputOptions,
    levels: &[u32],
    nodata: Option<T>,
    progress: Option<&crate::progress::StageBar>,
) -> Result<Vec<Vec<RemuxCompressedBlock>>>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
{
    let mut spool = LayerBlockSpool::new()?;
    encode_overview_layers_to_spool::<T>(
        input,
        &mut spool,
        width,
        height,
        tile_size,
        out_bands,
        window,
        band_map,
        encoding,
        opts,
        levels,
        nodata,
        progress,
    )?;
    spool.read_all_layers()
}
