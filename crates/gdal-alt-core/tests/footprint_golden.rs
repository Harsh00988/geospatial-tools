mod support;

use support::{footprint_fixture, footprint_golden, require_footprint_fixture, resolve_existing};

use std::fs;

use gdal_alt_core::{
    extract_footprint, metrics_close, ring_metrics_from_geojson, FootprintGeorefChoice,
    FootprintOptions, FootprintOutputFormat, ValiditySourceChoice,
};

#[test]
fn footprint_golden_affine_full() {
    let path = require_footprint_fixture("affine_full.tif");
    let golden = fs::read_to_string(footprint_golden("affine_full.geojson")).expect("golden");
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
    let actual = ring_metrics_from_geojson(&result.geojson).expect("metrics");
    let expected = ring_metrics_from_geojson(&golden).expect("golden metrics");
    assert!(metrics_close(actual, expected, 1e-9, 1e-9));
    assert_eq!(result.ring_count, 1);
    assert_eq!(result.validity_source, "full");
}

#[test]
fn footprint_golden_masked_rgba() {
    let path = require_footprint_fixture("masked_rgba.tif");
    let golden = fs::read_to_string(footprint_golden("masked_rgba.geojson")).expect("golden");
    let result = extract_footprint(&path.to_string_lossy(), false, None, &FootprintOptions::default(), 0)
        .expect("extract");
    let actual = ring_metrics_from_geojson(&result.geojson).expect("metrics");
    let expected = ring_metrics_from_geojson(&golden).expect("golden metrics");
    assert!(metrics_close(actual, expected, 1e-6, 1e-6));
    assert_eq!(result.validity_source, "alpha");
    assert!(result.vertex_count >= 4);
}

#[test]
fn footprint_golden_sar_nonzero_outer_ring() {
    let path = require_footprint_fixture("sar_nonzero.tif");
    let result = extract_footprint(&path.to_string_lossy(), false, None, &FootprintOptions::default(), 0)
        .expect("extract");
    assert_eq!(result.validity_source, "nonzero");
    assert_eq!(result.ring_count, 1, "SAR auto should keep outer ring only");
}

#[test]
fn footprint_golden_gcp_scattered_tps() {
    let path = require_footprint_fixture("gcp_scattered.tif");
    let result = extract_footprint(
        &path.to_string_lossy(),
        false,
        None,
        &FootprintOptions {
            source: ValiditySourceChoice::Full,
            georef: Some(FootprintGeorefChoice::GcpTps),
            ..FootprintOptions::default()
        },
        0,
    )
    .expect("extract");
    assert_eq!(result.georef_source, "gcp_tps");
    assert_eq!(result.ring_count, 1);
}

#[test]
fn footprint_wkt_and_flat_output_formats() {
    let path = require_footprint_fixture("affine_full.tif");
    let wkt = extract_footprint(
        &path.to_string_lossy(),
        false,
        None,
        &FootprintOptions {
            source: ValiditySourceChoice::Full,
            output_format: FootprintOutputFormat::Wkt,
            ..FootprintOptions::default()
        },
        0,
    )
    .expect("wkt");
    assert!(wkt.body.starts_with("POLYGON("));

    let flat = extract_footprint(
        &path.to_string_lossy(),
        false,
        None,
        &FootprintOptions {
            source: ValiditySourceChoice::Full,
            output_format: FootprintOutputFormat::WktFlat,
            ..FootprintOptions::default()
        },
        0,
    )
    .expect("flat");
    assert!(flat.body.contains("-1 "));
}

#[test]
fn footprint_committed_jp2_when_available() {
    let path = resolve_existing(&[
        footprint_fixture("sample_rgba.jp2"),
        footprint_fixture("sample_masked.jp2"),
        std::path::PathBuf::from(
            "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/S2C_MSIL2A_20260810T055631_N0512_R091_T42QUL_20260810T090514.jp2",
        ),
    ]);
    let Some(path) = path else {
        eprintln!("skipping JP2 golden test: no JP2 fixture");
        return;
    };
    let result = extract_footprint(&path.to_string_lossy(), false, None, &FootprintOptions::default(), 0)
        .expect("extract jp2");
    assert!(result.ring_count >= 1);
    assert!(result.vertex_count >= 4);
}

#[test]
fn footprint_dem_fixture_opens() {
    let dem_path = require_footprint_fixture("dem_slope.tif")
        .to_string_lossy()
        .into_owned();
    let result = extract_footprint(
        &require_footprint_fixture("gcp_scattered.tif").to_string_lossy(),
        false,
        None,
        &FootprintOptions {
            source: ValiditySourceChoice::Full,
            dem_path: Some(dem_path),
            georef: Some(FootprintGeorefChoice::GcpTps),
            ..FootprintOptions::default()
        },
        0,
    )
    .expect("extract with dem path");
    assert_eq!(result.georef_source, "gcp_tps");
    assert!(result.vertex_count >= 4);
}

#[test]
fn footprint_external_iceye_when_available() {
    let path = resolve_existing(&[
        footprint_fixture("sar_nonzero.tif"),
        std::path::PathBuf::from(
            "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/ICEYE_X37_GRD_SCW_954079909_20260802T104048.tif",
        ),
    ]);
    let Some(path) = path else {
        return;
    };
    let result = extract_footprint(&path.to_string_lossy(), false, None, &FootprintOptions::default(), 0)
        .expect("extract");
    assert_eq!(result.ring_count, 1);
}
