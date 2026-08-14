use anyhow::Result;
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{RemuxTileEncoding, StreamingRgbCogWriter};
use tiff_reader::TiffSample;

use crate::cog::{tile_jobs, CogOutputOptions};
use crate::crop::WriteWindow;
use crate::remux::resolve_overview_read_source;
use crate::spool::LayerBlockSpool;
use crate::strip_encode::{
    stream_base_layer_to_spool, stream_strip_overview_from_decoded_to_spool,
    stream_strip_overview_layer_with_cache_to_spool, DecodedTileSpool,
};

/// Whether the next overview should be built from the in-memory parent pyramid level.
pub fn should_chain_from_parent(input: &GeoTiffFile, parent_level: u32, next_level: u32) -> bool {
    if next_level != parent_level.saturating_mul(2) {
        return false;
    }
    if input.overview_count() == 0 {
        return true;
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
    let base_blocks = tile_jobs(width, height, tile_size as u32).len();
    let base_writer = spool.begin_streaming_layer(base_blocks)?;
    stream_base_layer_to_spool::<T>(
        input,
        width,
        height,
        tile_size,
        out_bands,
        window,
        band_map,
        encoding,
        &base_writer,
        progress,
    )?;
    spool.commit_streaming_layer(base_writer)?;

    let mut parent_decoded: Option<DecodedTileSpool<T>> = None;
    let mut parent_level = 1u32;
    for (level_idx, &level) in levels.iter().enumerate() {
        let next_level = levels.get(level_idx + 1).copied();
        let chain_next =
            next_level.is_some_and(|next| should_chain_from_parent(input, level, next));

        let ov_width = (width / level).max(1);
        let ov_height = (height / level).max(1);
        let block_count = tile_jobs(ov_width, ov_height, tile_size as u32).len();
        let layer_writer = spool.begin_streaming_layer(block_count)?;

        let decoded = if let Some(parent) = parent_decoded.take() {
            stream_strip_overview_from_decoded_to_spool::<T>(
                &parent,
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
                &layer_writer,
                progress,
            )?
        } else {
            stream_strip_overview_layer_with_cache_to_spool::<T>(
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
                &layer_writer,
                progress,
            )?
        };

        spool.commit_streaming_layer(layer_writer)?;
        if chain_next {
            parent_decoded = decoded;
            parent_level = level;
        }
    }

    Ok(())
}

pub fn encode_overview_layers_to_streaming_cog<T>(
    input: &GeoTiffFile,
    cog_writer: &StreamingRgbCogWriter,
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
    let base_blocks = tile_jobs(width, height, tile_size as u32).len();
    let base_writer = cog_writer.begin_layer(base_blocks).map_err(|e| anyhow::anyhow!(e))?;
    stream_base_layer_to_spool::<T>(
        input,
        width,
        height,
        tile_size,
        out_bands,
        window,
        band_map,
        encoding,
        &base_writer,
        progress,
    )?;
    cog_writer
        .commit_layer(base_writer)
        .map_err(|e| anyhow::anyhow!(e))?;

    let mut parent_decoded: Option<DecodedTileSpool<T>> = None;
    let mut parent_level = 1u32;
    for (level_idx, &level) in levels.iter().enumerate() {
        let next_level = levels.get(level_idx + 1).copied();
        let chain_next =
            next_level.is_some_and(|next| should_chain_from_parent(input, level, next));

        let ov_width = (width / level).max(1);
        let ov_height = (height / level).max(1);
        let block_count = tile_jobs(ov_width, ov_height, tile_size as u32).len();
        let layer_writer = cog_writer
            .begin_layer(block_count)
            .map_err(|e| anyhow::anyhow!(e))?;

        let decoded = if let Some(parent) = parent_decoded.take() {
            stream_strip_overview_from_decoded_to_spool::<T>(
                &parent,
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
                &layer_writer,
                progress,
            )?
        } else {
            stream_strip_overview_layer_with_cache_to_spool::<T>(
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
                &layer_writer,
                progress,
            )?
        };

        cog_writer
            .commit_layer(layer_writer)
            .map_err(|e| anyhow::anyhow!(e))?;
        if chain_next {
            parent_decoded = decoded;
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
) -> Result<LayerBlockSpool>
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
    Ok(spool)
}
