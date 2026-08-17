use gdal_alt_core::cog::CogOutputOptions;
use gdal_alt_core::util::thread_pool;
use gdal_alt_core::validate::validate_cog;
use gdal_alt_core::{convert_geotiff, ConvertRequest, Jp2Raster};
use std::path::PathBuf;

fn s2_jp2_path() -> Option<PathBuf> {
    let candidates = [
        "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/S2C_MSIL2A_20260810T055631_N0512_R091_T42QUL_20260810T090514.jp2",
        "test_data/sentinel2_sample.jp2",
    ];
    candidates
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

#[test]
fn convert_sentinel2_jp2_to_cog_when_available() {
    let Some(input) = s2_jp2_path() else {
        return;
    };
    let output = tempfile::tempdir()
        .expect("tempdir")
        .path()
        .join("sentinel2_cog.tif");
    let opts = CogOutputOptions {
        blocksize: 512,
        ..CogOutputOptions::default()
    };
    let pool = thread_pool(0).expect("thread pool");
    let input_str = input.to_string_lossy();
    convert_geotiff(
        &pool,
        &ConvertRequest {
            input: &input_str,
            output: &output,
            opts: &opts,
            mmap: true,
            show_progress: false,
            window: None,
            bands: None,
        },
    )
    .expect("jp2 convert");

    let report = validate_cog(&output, false).expect("validate");
    assert!(report.is_valid(), "validation failed: {:?}", report.issues);
    assert!(output.metadata().unwrap().len() > 1024 * 1024);
}

#[test]
fn jp2_header_reads_sentinel2_when_available() {
    let Some(input) = s2_jp2_path() else {
        return;
    };
    let bytes = std::fs::read(&input).expect("read jp2");
    let raster = Jp2Raster::open(&bytes).expect("open jp2 header");
    assert_eq!(raster.bands, 3);
    assert_eq!(raster.bits_per_sample, 8);
    assert!(raster.width > 1000);
    assert!(raster.height > 1000);
}
