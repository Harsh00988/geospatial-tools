use gdal_alt_core::cog::discover_dataset_masks;
use gdal_alt_core::cog::CogOutputOptions;
use gdal_alt_core::crop::window_from_srcwin;
use gdal_alt_core::open::open_geotiff;
use gdal_alt_core::util::thread_pool;
use gdal_alt_core::validate::{validate_cog, ValidationLevel};
use gdal_alt_core::{convert_geotiff, ConvertRequest};
use std::path::{Path, PathBuf};

fn sn33_path() -> Option<PathBuf> {
    let path = Path::new("/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/20260725_051351_SN33_L1B_MS_VISUAL.tif");
    path.exists().then(|| path.to_path_buf())
}

fn default_opts() -> CogOutputOptions {
    CogOutputOptions::default()
}

#[test]
fn discovers_dataset_masks_on_sn33_when_available() {
    let Some(path) = sn33_path() else {
        return;
    };
    let input = open_geotiff(&path, false).expect("open sn33");
    let masks = discover_dataset_masks(&input).expect("dataset masks");
    assert_eq!(masks.base_ifd_index, 1);
    assert_eq!(masks.overview_ifd_indices.len(), 4);
}

#[test]
fn convert_preserves_dataset_masks_when_available() {
    let Some(path) = sn33_path() else {
        return;
    };
    let output = std::env::temp_dir().join("gdal_alt_mask_convert.tif");
    let opts = default_opts();
    let pool = thread_pool(0).expect("thread pool");
    let input_str = path.to_string_lossy();
    let request = ConvertRequest {
        input: &input_str,
        output: &output,
        opts: &opts,
        mmap: false,
        show_progress: false,
        window: None,
        bands: None,
    };
    convert_geotiff(&pool, &request).expect("convert");

    let report = validate_cog(&output, false).expect("validate");
    assert!(report.is_valid(), "validation failed: {:?}", report.issues);
    let reopened = open_geotiff(&output, false).expect("reopen");
    assert!(discover_dataset_masks(&reopened).is_some());
}

#[test]
fn crop_preserves_dataset_masks_when_available() {
    let Some(path) = sn33_path() else {
        return;
    };
    let input = open_geotiff(&path, false).expect("open sn33");
    let window = window_from_srcwin(input.width(), input.height(), 1024, 1024, 2048, 2048)
        .expect("srcwin");
    let output = std::env::temp_dir().join("gdal_alt_mask_crop.tif");
    let opts = default_opts();
    let pool = thread_pool(0).expect("thread pool");
    let input_str = path.to_string_lossy();
    let request = ConvertRequest {
        input: &input_str,
        output: &output,
        opts: &opts,
        mmap: false,
        show_progress: false,
        window: Some(window),
        bands: None,
    };
    convert_geotiff(&pool, &request).expect("crop convert");

    let reopened = open_geotiff(&output, false).expect("reopen crop");
    assert!(
        discover_dataset_masks(&reopened).is_some(),
        "cropped output should retain dataset mask IFDs"
    );
    let report = validate_cog(&output, false).expect("validate crop");
    assert!(
        !report
            .issues
            .iter()
            .any(|issue| issue.level == ValidationLevel::Error),
        "crop validation errors: {:?}",
        report.issues
    );
}
