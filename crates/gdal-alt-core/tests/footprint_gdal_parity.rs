use std::fs;
use std::path::Path;
use std::process::Command;

mod support;

use support::{
    footprint_fixture, footprint_golden, require_footprint_fixture, resolve_existing,
};

use gdal_alt_core::{
    bbox_iou, extract_footprint, metrics_close, ring_metrics_from_geojson, FootprintOptions,
};

fn gdal_available() -> bool {
    Command::new("gdal")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn gdal_footprint_geojson(path: &Path) -> Option<String> {
    let output = Command::new("gdal")
        .args([
            "footprint",
            "-of",
            "GeoJSON",
            path.to_str()?,
            "/vsistdout/",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn assert_gdal_bbox_parity(path: &Path, opts: FootprintOptions, min_iou: f64) {
    let fast = extract_footprint(&path.to_string_lossy(), false, None, &opts, 0).expect("fastfootprint");
    let gdal_json = gdal_footprint_geojson(path).expect("gdal footprint");
    let fast_metrics = ring_metrics_from_geojson(&fast.geojson).expect("fast metrics");
    let gdal_metrics = ring_metrics_from_geojson(&gdal_json).expect("gdal metrics");
    let iou = bbox_iou(fast_metrics.bbox, gdal_metrics.bbox);
    assert!(
        iou >= min_iou,
        "bbox IoU {iou} below {min_iou} for {}\nfast={:?}\ngdal={:?}",
        path.display(),
        fast_metrics.bbox,
        gdal_metrics.bbox
    );
}

fn assert_golden_parity(path: &Path, golden_name: &str, opts: FootprintOptions) {
    let golden_path = footprint_golden(golden_name);
    if !golden_path.is_file() {
        return;
    }
    let golden = fs::read_to_string(&golden_path).expect("golden");
    let fast = extract_footprint(&path.to_string_lossy(), false, None, &opts, 0).expect("fast");
    let actual = ring_metrics_from_geojson(&fast.geojson).expect("actual metrics");
    let expected = ring_metrics_from_geojson(&golden).expect("golden metrics");
    assert!(
        metrics_close(actual, expected, 1e-6, 1e-6),
        "golden mismatch for {}",
        path.display()
    );
}

#[test]
fn footprint_gdal_parity_committed_affine() {
    let path = require_footprint_fixture("affine_full.tif");
    if gdal_available() {
        assert_gdal_bbox_parity(&path, FootprintOptions::default(), 0.90);
    }
    assert_golden_parity(
        &path,
        "affine_full.geojson",
        gdal_alt_core::FootprintOptions {
            source: gdal_alt_core::ValiditySourceChoice::Full,
            ..FootprintOptions::default()
        },
    );
}

#[test]
fn footprint_gdal_parity_committed_sar() {
    let path = require_footprint_fixture("sar_nonzero.tif");
    let result = extract_footprint(&path.to_string_lossy(), false, None, &FootprintOptions::default(), 0)
        .expect("extract");
    assert_eq!(result.validity_source, "nonzero");
    assert_eq!(result.ring_count, 1);
    assert!(result.vertex_count >= 4);
}

#[test]
fn footprint_gdal_parity_sn33_when_available() {
    let path = resolve_existing(&[
        footprint_fixture("20260725_051351_SN33_L1B_MS_VISUAL.tif"),
        std::path::PathBuf::from(
            "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/20260725_051351_SN33_L1B_MS_VISUAL.tif",
        ),
    ]);
    let Some(path) = path else {
        return;
    };
    if gdal_available() {
        assert_gdal_bbox_parity(&path, FootprintOptions::default(), 0.90);
    }
}

#[test]
fn footprint_gdal_parity_iceye_when_available() {
    let path = resolve_existing(&[
        footprint_fixture("ICEYE_X37_GRD_SCW_954079909_20260802T104048.tif"),
        std::path::PathBuf::from(
            "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/ICEYE_X37_GRD_SCW_954079909_20260802T104048.tif",
        ),
    ]);
    let Some(path) = path else {
        return;
    };
    if gdal_available() {
        assert_gdal_bbox_parity(
            &path,
            FootprintOptions {
                source: gdal_alt_core::ValiditySourceChoice::Full,
                ..FootprintOptions::default()
            },
            0.85,
        );
    }
}

#[test]
fn footprint_gdal_parity_s2_jp2_when_available() {
    let path = resolve_existing(&[
        footprint_fixture("sample_rgba.jp2"),
        std::path::PathBuf::from(
            "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/S2C_MSIL2A_20260810T055631_N0512_R091_T42QUL_20260810T090514.jp2",
        ),
    ]);
    let Some(path) = path else {
        return;
    };
    if gdal_available() {
        assert_gdal_bbox_parity(&path, FootprintOptions::default(), 0.85);
    }
}
