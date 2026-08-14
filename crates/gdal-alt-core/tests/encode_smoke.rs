use gdal_alt_core::cog::CogOutputOptions;
use gdal_alt_core::util::thread_pool;
use gdal_alt_core::open::open_geotiff;
use gdal_alt_core::{convert_geotiff, ConvertRequest};
use geotiff_writer::GeoTiffBuilder;
use ndarray::Array3;
use std::path::PathBuf;

fn write_strip_rgb_fixture(path: &std::path::Path) {
    let width = 1024usize;
    let height = 1024usize;
    let mut data = Array3::<u8>::zeros((height, width, 3));
    for y in 0..height {
        for x in 0..width {
            data[[y, x, 0]] = (x % 256) as u8;
            data[[y, x, 1]] = (y % 256) as u8;
            data[[y, x, 2]] = ((x + y) % 256) as u8;
        }
    }
    GeoTiffBuilder::new(width as u32, height as u32)
        .bands(3)
        .epsg(4326)
        .pixel_scale(0.01, 0.01)
        .origin(-180.0, 90.0)
        .compression(tiff_core::Compression::Deflate)
        .write_3d(path, data.view())
        .expect("write fixture");
}

fn write_tiled_rgb_fixture(path: &std::path::Path) {
    let width = 2048usize;
    let height = 2048usize;
    let mut data = Array3::<u8>::zeros((height, width, 3));
    for y in 0..height {
        for x in 0..width {
            data[[y, x, 0]] = (x % 256) as u8;
            data[[y, x, 1]] = (y % 256) as u8;
            data[[y, x, 2]] = ((x ^ y) % 256) as u8;
        }
    }
    GeoTiffBuilder::new(width as u32, height as u32)
        .bands(3)
        .epsg(4326)
        .pixel_scale(0.01, 0.01)
        .origin(-180.0, 90.0)
        .compression(tiff_core::Compression::Deflate)
        .tile_size(256, 256)
        .write_3d(path, data.view())
        .expect("write fixture");
}

fn encode_fixture(input: &std::path::Path, output: &std::path::Path, opts: &CogOutputOptions) {
    let pool = thread_pool(0).expect("thread pool");
    let input_str = input.to_string_lossy();
    let request = ConvertRequest {
        input: &input_str,
        output,
        opts,
        mmap: false,
        show_progress: false,
        window: None,
        bands: None,
    };
    convert_geotiff(&pool, &request).expect("convert");
    let report = gdal_alt_core::validate::validate_cog(output, false).expect("validate");
    assert!(report.is_valid(), "validation failed: {:?}", report.issues);
}

fn encode_fixture_tiled_smoke(input: &std::path::Path, output: &std::path::Path, opts: &CogOutputOptions) {
    let pool = thread_pool(0).expect("thread pool");
    let input_str = input.to_string_lossy();
    let request = ConvertRequest {
        input: &input_str,
        output,
        opts,
        mmap: false,
        show_progress: false,
        window: None,
        bands: None,
    };
    convert_geotiff(&pool, &request).expect("convert");
    let reopened = open_geotiff(output, false).expect("reopen");
    let base = reopened
        .tiff()
        .ifd(reopened.base_ifd_index())
        .expect("base ifd");
    assert!(base.is_tiled(), "expected tiled COG output");
    assert!(output.metadata().map(|m| m.len()).unwrap_or(0) > 0);
}

#[test]
fn encode_smoke_strip_rgb_fixture() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("strip_rgb.tif");
    let output = dir.path().join("strip_rgb_cog.tif");
    write_strip_rgb_fixture(&input);
    let opts = CogOutputOptions::default();
    encode_fixture(&input, &output, &opts);
}

#[test]
fn encode_smoke_tiled_rgb_fixture() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("tiled_rgb.tif");
    let output = dir.path().join("tiled_rgb_cog.tif");
    write_tiled_rgb_fixture(&input);
    let mut opts = CogOutputOptions::default();
    opts.blocksize = 256;
    encode_fixture_tiled_smoke(&input, &output, &opts);
}

#[test]
fn encode_smoke_external_fixtures_when_available() {
    let candidates: Vec<PathBuf> = [
        "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/20260725_051351_SN33_L1B_MS_VISUAL.tif",
        "test_data/sn33_mask_fixture.tif",
    ]
    .into_iter()
    .map(PathBuf::from)
    .filter(|path| path.is_file())
    .collect();
    if candidates.is_empty() {
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let opts = CogOutputOptions::default();
    for (index, input) in candidates.into_iter().take(3).enumerate() {
        let output = dir.path().join(format!("external_{index}.tif"));
        encode_fixture(&input, &output, &opts);
    }
}
