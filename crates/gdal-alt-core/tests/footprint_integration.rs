use gdal_alt_core::util::thread_pool;
use gdal_alt_core::{
    extract_footprint, open_geotiff, FootprintOptions, RasterProfile, ValiditySourceChoice,
};
use geotiff_writer::GeoTiffBuilder;
use ndarray::Array3;
use std::path::Path;

fn write_full_extent_fixture(path: &std::path::Path) {
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
        .write_3d(path, data.view())
        .expect("write fixture");
}

#[test]
fn footprint_full_extent_matches_bbox() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("full.tif");
    write_full_extent_fixture(&input);

    let opts = FootprintOptions {
        source: ValiditySourceChoice::Full,
        ..FootprintOptions::default()
    };
    let result = extract_footprint(
        &input.to_string_lossy(),
        false,
        None,
        &opts,
        0,
    )
    .expect("extract footprint");

    assert_eq!(result.validity_source, "full");
    assert_eq!(result.ring_count, 1);
    assert!(result.geojson.contains("\"type\":\"Polygon\"") || result.geojson.contains("\"type\":\"MultiPolygon\""));
    assert!(result.geojson.contains("-1.0"));
    assert!(result.geojson.contains("0.04"));
}

#[test]
fn footprint_auto_on_full_raster_uses_full_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("auto.tif");
    write_full_extent_fixture(&input);

    let result = extract_footprint(
        &input.to_string_lossy(),
        false,
        None,
        &FootprintOptions::default(),
        0,
    )
    .expect("extract footprint");

    assert_eq!(result.validity_source, "full");
    assert_eq!(result.ring_count, 1);
}

#[test]
fn footprint_gcp_georef_when_available() {
    let path = Path::new(
        "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/ICEYE_X37_GRD_SCW_954079909_20260802T104048.tif",
    );
    if !path.is_file() {
        eprintln!("skipping ICEYE GCP footprint test: fixture not found");
        return;
    }

    let result = extract_footprint(&path.to_string_lossy(), false, None, &FootprintOptions::default(), 0)
        .expect("extract footprint");

    assert_eq!(result.validity_source, "nonzero");
    assert!(result.ring_count >= 1);
    assert_eq!(result.georef_source, "rpc");
}

#[test]
fn footprint_external_masked_fixture_when_available() {
    let candidates = [
        "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/20260725_051351_SN33_L1B_MS_VISUAL.tif",
        "test_data/sn33_mask_fixture.tif",
    ];
    let Some(path) = candidates
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file())
    else {
        eprintln!("skipping SN33 footprint test: fixture not found");
        return;
    };

    let pool = thread_pool(0).expect("thread pool");
    let input = open_geotiff(path, false).expect("open");
    let profile = RasterProfile::from_geotiff(&input).expect("profile");
    let result = gdal_alt_core::extract_footprint_geotiff(
        &input,
        &profile,
        None,
        &FootprintOptions::default(),
        &pool,
    )
    .expect("extract");

    assert!(
        result.validity_source == "mask" || result.validity_source == "alpha",
        "expected mask or alpha source, got {}",
        result.validity_source
    );
    assert!(result.ring_count >= 1);
    assert!(result.geojson.contains("FeatureCollection"));
}

#[test]
fn footprint_jp2_when_available() {
    let candidates = [
        "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/S2C_MSIL2A_20260810T055631_N0512_R091_T42QUL_20260810T090514.jp2",
        "test_data/sentinel2_sample.jp2",
    ];
    let Some(path) = candidates
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file())
    else {
        eprintln!("skipping JP2 footprint test: fixture not found");
        return;
    };

    let result = extract_footprint(&path.to_string_lossy(), false, None, &FootprintOptions::default(), 0)
        .expect("extract jp2 footprint");

    assert!(result.ring_count >= 1);
    assert!(result.geojson.contains("EPSG:4326") || result.geojson.contains("4326"));
    assert!(result.georef_source == "affine" || result.georef_source == "pixel");
}
