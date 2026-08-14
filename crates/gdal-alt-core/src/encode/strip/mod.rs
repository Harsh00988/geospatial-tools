//! Strip- and tile-oriented decode/encode for the COG conversion pipeline.

mod base;
mod config;
mod decode;
mod decode_cache;
mod overview;
mod regions;
mod tile;
mod tiles;

#[cfg(test)]
mod tests;

pub(crate) use base::stream_base_layer_to_spool;
pub(crate) use config::{encode_row_group_total, output_tile_encoding};
pub(crate) use overview::{
    stream_strip_overview_from_decoded_to_spool, stream_strip_overview_layer_with_cache_to_spool,
};
pub(crate) use tile::DecodedTileSpool;
