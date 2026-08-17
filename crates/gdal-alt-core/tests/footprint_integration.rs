mod support;

use support::{footprint_fixture, require_footprint_fixture};

use gdal_alt_core::{
    extract_footprint, open_geotiff, FootprintOptions, RasterProfile, ValiditySourceChoice,
};
use geotiff_writer::GeoTiffBuilder;
use ndarray::Array3;

#[test]
fn footprint_committed_masked_rgba() {
    let path = require_footprint_fixture("masked_rgba.tif");
    let result = extract_footprint(&path.to_string_lossy(), false, None, &FootprintOptions::default(), 0)
        .expect("extract");
    assert_eq!(result.validity_source, "alpha");
    assert!(result.ring_count >= 1);
}

#[test]
fn footprint_committed_gcp_georef() {
    let path = require_footprint_fixture("gcp_scattered.tif");
    let result = extract_footprint(
        &path.to_string_lossy(),
        false,
        None,
        &FootprintOptions {
            source: ValiditySourceChoice::Full,
            ..FootprintOptions::default()
        },
        0,
    )
    .expect("extract");
    assert_eq!(result.georef_source, "gcp_tps");
}

#[test]
fn footprint_inprocess_full_extent_matches_bbox() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("full.tif");
    let width = 128usize;
    let height = 96usize;
    let mut data = Array3::<u8>::zeros((height, width, 3));
    for y in 0..height {
        for x in 0..width {
            data[[y, x, 0]] = 200;
            data[[y, x, 1]] = 100;
            data[[y, x, 2]] = 50;
        }
    }
    GeoTiffBuilder::new(width as u32, height as u32)
        .bands(3)
        .epsg(4326)
        .pixel_scale(0.01, 0.01)
        .origin(-1.0, 1.0)
        .compression(tiff_core::Compression::Deflate)
        .write_3d(&input, data.view())
        .expect("write fixture");

    let result = extract_footprint(
        &input.to_string_lossy(),
        false,
        None,
        &FootprintOptions {
            source: ValiditySourceChoice::Full,
            ..FootprintOptions::default()
        },
        0,
    )
    .expect("extract footprint");

    assert_eq!(result.validity_source, "full");
    assert_eq!(result.ring_count, 1);
    assert!(result.geojson.contains("-1.0"));
}

#[test]
fn footprint_external_sn33_when_available() {
    let path = support::resolve_existing(&[
        footprint_fixture("20260725_051351_SN33_L1B_MS_VISUAL.tif"),
        std::path::PathBuf::from(
            "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/20260725_051351_SN33_L1B_MS_VISUAL.tif",
        ),
        std::path::PathBuf::from("test_data/sn33_mask_fixture.tif"),
    ]);
    let Some(path) = path else {
        return;
    };

    let pool = gdal_alt_core::util::thread_pool(0).expect("thread pool");
    let input = open_geotiff(&path, false).expect("open");
    let profile = RasterProfile::from_geotiff(&input).expect("profile");
    let result = gdal_alt_core::extract_footprint_geotiff(
        &input,
        &profile,
        None,
        &FootprintOptions::default(),
        &pool,
        None,
    )
    .expect("extract");

    assert!(
        result.validity_source == "mask" || result.validity_source == "alpha",
        "expected mask or alpha source, got {}",
        result.validity_source
    );
}
