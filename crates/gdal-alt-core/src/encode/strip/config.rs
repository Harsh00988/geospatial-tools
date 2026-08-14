use tiff_core::SampleFormat;

use crate::cog::{tile_encoding_from_opts, CogOutputOptions};
use geotiff_writer::RemuxTileEncoding;

pub(crate) fn encode_row_group_total(
    width: u32,
    height: u32,
    tile_size: usize,
    levels: &[u32],
) -> u64 {
    let mut total = row_group_count(width, height, tile_size);
    for &level in levels {
        let ov_w = (width / level).max(1);
        let ov_h = (height / level).max(1);
        total += row_group_count(ov_w, ov_h, tile_size);
    }
    total
}

fn row_group_count(_width: u32, height: u32, tile_size: usize) -> u64 {
    (height as usize).div_ceil(tile_size) as u64
}

pub(crate) fn output_tile_encoding(
    opts: &CogOutputOptions,
    tile_size: usize,
    spp: u16,
    sample_format: SampleFormat,
) -> RemuxTileEncoding {
    tile_encoding_from_opts(opts, tile_size, spp, None, sample_format)
}
