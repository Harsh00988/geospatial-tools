use crate::crop::WriteWindow;
use tiff_core::Compression;

use super::decode::STRIP_DECODE_TILE_ROWS;
use super::decode::strip_decode_window_rows;
use super::tile::{DecodedTileSpool, StripTile};

#[test]
fn strip_decode_window_caps_large_strips() {
    assert_eq!(strip_decode_window_rows(25_976, 512), 512 * STRIP_DECODE_TILE_ROWS);
    assert_eq!(strip_decode_window_rows(512, 512), 512);
    assert_eq!(strip_decode_window_rows(1_024, 512), 1_024);
    assert_eq!(strip_decode_window_rows(2_048, 512), 2_048);
    assert_eq!(strip_decode_window_rows(2_049, 512), 512 * STRIP_DECODE_TILE_ROWS);
}

#[test]
fn strip_windowed_decode_only_for_compressed_full_image_strips() {
    // Uncompressed ICEYE-like strip: row-group path is faster and already low RAM.
    assert!(!should_use_strip_windowed_decode_for_compression(
        Compression::None,
        25_976,
        512,
        None,
    ));
    assert!(should_use_strip_windowed_decode_for_compression(
        Compression::Lzw,
        25_976,
        512,
        None,
    ));
    assert!(!should_use_strip_windowed_decode_for_compression(
        Compression::Lzw,
        512,
        512,
        None,
    ));
    assert!(!should_use_strip_windowed_decode_for_compression(
        Compression::Lzw,
        25_976,
        512,
        Some(WriteWindow {
            col_off: 0,
            row_off: 0,
            width: 1024,
            height: 1024,
        }),
    ));
}

fn should_use_strip_windowed_decode_for_compression(
    compression: Compression,
    rows_per_strip: usize,
    tile_size: usize,
    window: Option<WriteWindow>,
) -> bool {
    if window.is_some() || rows_per_strip <= tile_size {
        return false;
    }
    compression != Compression::None
}

#[test]
fn strip_window_count_scales_with_image_height() {
    let rows_per_strip = 25_976;
    let tile_size = 512;
    let image_height: usize = 25_976;
    let decode_window = strip_decode_window_rows(rows_per_strip, tile_size);
    let windows_per_strip = image_height.div_ceil(decode_window);
    // 25976 / 2048 = 13 windows (not 1 full-strip decode)
    assert_eq!(decode_window, 2048);
    assert_eq!(windows_per_strip, 13);
}

#[test]
fn decoded_tile_spool_roundtrips_single_band_tile() {
    use ndarray::arr2;
    let tile = StripTile::Single(arr2(&[[1u16, 2], [3, 4]]));
    let spool = DecodedTileSpool::<u16>::new().unwrap();
    spool.insert(512, 1024, &tile).unwrap();
    let loaded = spool.get(512, 1024).unwrap().unwrap();
    match loaded {
        StripTile::Single(data) => assert_eq!(data[[0, 0]], 1),
        _ => panic!("expected single-band tile"),
    }
    assert!(spool.get(0, 0).unwrap().is_none());
}
