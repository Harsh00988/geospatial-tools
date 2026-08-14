use std::collections::BTreeMap;

use anyhow::Result;
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{remux_compress_tile, RemuxTileEncoding};
use rayon::prelude::*;
use tiff_reader::TiffSample;

use crate::cog::{tile_jobs, TileJob};
use crate::crop::WriteWindow;
use crate::encode::sink::StreamingEncodeSink;
use crate::progress::StageBar;

use super::decode::{
    read_decoded_strip, should_use_strip_windowed_decode, should_use_tiled_source_decode,
    slice_strip_tile, strip_decode_window_rows, strip_rows_per_strip, with_decode_permit,
};
use super::tiles::{encode_tile_samples, read_strip_row_batch};

fn stream_base_layer_from_strip_windows_to_spool<T>(
    input: &GeoTiffFile,
    width: u32,
    height: u32,
    tile_size: usize,
    rows_per_strip: usize,
    out_bands: usize,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    writer: &impl StreamingEncodeSink,
    progress: Option<&StageBar>,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + Clone,
{
    let decode_window_rows = strip_decode_window_rows(rows_per_strip, tile_size);
    let image_height = height as usize;
    let image_width = width as usize;
    let tiles = tile_jobs(width, height, tile_size as u32);
    let tile_index_by_pos: std::collections::HashMap<(usize, usize), usize> = tiles
        .iter()
        .enumerate()
        .map(|(idx, job)| ((job.col_off, job.row_off), idx))
        .collect();

    let mut jobs_by_window: BTreeMap<(usize, usize), Vec<TileJob>> = BTreeMap::new();
    for job in tiles {
        let strip_idx = job.row_off / rows_per_strip;
        let strip_row_start = strip_idx * rows_per_strip;
        let rel_row = job.row_off.saturating_sub(strip_row_start);
        let window_idx = rel_row / decode_window_rows;
        jobs_by_window
            .entry((strip_idx, window_idx))
            .or_default()
            .push(job);
    }

    jobs_by_window
        .par_iter()
        .try_for_each(|(&(strip_idx, window_idx), jobs)| -> Result<()> {
            let strip_row_start = strip_idx * rows_per_strip;
            let window_row_start = strip_row_start + window_idx * decode_window_rows;
            let window_row_end = (window_row_start + decode_window_rows).min(image_height);
            let row_count = window_row_end.saturating_sub(window_row_start);
            let strip_tile = with_decode_permit(|| {
                read_decoded_strip::<T>(
                    input,
                    window_row_start,
                    0,
                    row_count,
                    image_width,
                    out_bands,
                    band_map,
                )
            })?;

            for job in jobs {
                let tile = slice_strip_tile(&strip_tile, job, window_row_start, 0, out_bands)?;
                let block_index = tile_index_by_pos[&(job.col_off, job.row_off)];
                let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
                let block = remux_compress_tile(&samples, block_index, encoding)
                    .map_err(|err| anyhow::anyhow!(err))?;
                writer.write_block(block_index, block)?;
            }

            if let Some(bar) = progress {
                bar.inc(1);
            }
            Ok(())
        })?;

    Ok(())
}

fn stream_base_layer_from_source_tiles_to_spool<T>(
    input: &GeoTiffFile,
    width: u32,
    height: u32,
    tile_size: usize,
    out_bands: usize,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    writer: &impl StreamingEncodeSink,
    progress: Option<&StageBar>,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + Clone,
{
    let ifd = input.tiff().ifd(input.base_ifd_index())?;
    let src_tile_w = ifd.tile_width().unwrap_or(tile_size as u32) as usize;
    let src_tile_h = ifd.tile_height().unwrap_or(tile_size as u32) as usize;
    let image_w = width as usize;
    let image_h = height as usize;
    let tiles_across = image_w.div_ceil(src_tile_w);
    let tiles_down = image_h.div_ceil(src_tile_h);

    let output_jobs = tile_jobs(width, height, tile_size as u32);
    let tile_index_by_pos: std::collections::HashMap<(usize, usize), usize> = output_jobs
        .iter()
        .enumerate()
        .map(|(idx, job)| ((job.col_off, job.row_off), idx))
        .collect();

    (0..tiles_down)
        .into_par_iter()
        .try_for_each(|src_tr| -> Result<()> {
            (0..tiles_across).try_for_each(|src_tc| -> Result<()> {
                let col_off = src_tc * src_tile_w;
                let row_off = src_tr * src_tile_h;
                let cols = src_tile_w.min(image_w.saturating_sub(col_off));
                let rows = src_tile_h.min(image_h.saturating_sub(row_off));
                let src_tile = with_decode_permit(|| {
                    read_decoded_strip::<T>(input, row_off, col_off, rows, cols, out_bands, band_map)
                })?;

                let src_col_end = col_off + cols;
                let src_row_end = row_off + rows;
                for job in &output_jobs {
                    if job.col_off + job.cols <= col_off
                        || job.col_off >= src_col_end
                        || job.row_off + job.rows <= row_off
                        || job.row_off >= src_row_end
                    {
                        continue;
                    }
                    let tile = slice_strip_tile(&src_tile, job, row_off, col_off, out_bands)?;
                    let block_index = tile_index_by_pos[&(job.col_off, job.row_off)];
                    let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
                    let block = remux_compress_tile(&samples, block_index, encoding)
                        .map_err(|err| anyhow::anyhow!(err))?;
                    writer.write_block(block_index, block)?;
                }

                if let Some(bar) = progress {
                    bar.inc(1);
                }
                Ok(())
            })
        })?;

    Ok(())
}

pub(crate) fn stream_base_layer_to_spool<T>(
    input: &GeoTiffFile,
    width: u32,
    height: u32,
    tile_size: usize,
    out_bands: usize,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    writer: &impl StreamingEncodeSink,
    progress: Option<&StageBar>,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    if should_use_tiled_source_decode(input, window) {
        return stream_base_layer_from_source_tiles_to_spool::<T>(
            input,
            width,
            height,
            tile_size,
            out_bands,
            band_map,
            encoding,
            writer,
            progress,
        );
    }

    let rows_per_strip = strip_rows_per_strip(input)?;
    if should_use_strip_windowed_decode(input, rows_per_strip, tile_size, window) {
        return stream_base_layer_from_strip_windows_to_spool::<T>(
            input,
            width,
            height,
            tile_size,
            rows_per_strip,
            out_bands,
            band_map,
            encoding,
            writer,
            progress,
        );
    }

    let tiles = tile_jobs(width, height, tile_size as u32);
    let tile_index_by_pos: std::collections::HashMap<(usize, usize), usize> = tiles
        .iter()
        .enumerate()
        .map(|(idx, job)| ((job.col_off, job.row_off), idx))
        .collect();
    let mut row_groups: BTreeMap<usize, Vec<TileJob>> = BTreeMap::new();
    for job in tiles {
        row_groups.entry(job.row_off).or_default().push(job);
    }

    row_groups.par_iter().try_for_each(|(&row_off, jobs)| -> Result<()> {
        let batch = with_decode_permit(|| {
            read_strip_row_batch::<T>(input, out_bands, window, band_map, row_off, jobs)
        })?;
        if let Some(bar) = progress {
            bar.inc(1);
        }
        for (col_off, row_off, tile) in batch {
            let block_index = tile_index_by_pos[&(col_off, row_off)];
            let samples = encode_tile_samples(&tile, tile_size, out_bands)?;
            let block = remux_compress_tile(&samples, block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            writer.write_block(block_index, block)?;
        }
        Ok(())
    })?;

    Ok(())
}
