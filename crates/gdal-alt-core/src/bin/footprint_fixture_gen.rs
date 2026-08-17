//! Generate committed footprint regression fixtures under `test_data/footprint/`.
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use gdal_alt_core::{extract_footprint, FootprintOptions, ValiditySourceChoice};
use geotiff_writer::GeoTiffBuilder;
use ndarray::{Array2, Array3};
use tiff_core::{ExtraSample, PhotometricInterpretation};

fn main() -> Result<()> {
    let output_dir = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("test_data/footprint"));
    fs::create_dir_all(&output_dir)?;
    fs::create_dir_all(output_dir.join("golden"))?;

    write_affine_full(&output_dir.join("affine_full.tif"))?;
    write_masked_rgba(&output_dir.join("masked_rgba.tif"))?;
    write_sar_nonzero(&output_dir.join("sar_nonzero.tif"))?;
    write_gcp_scattered(&output_dir.join("gcp_scattered.tif"))?;
    write_dem_slope(&output_dir.join("dem_slope.tif"))?;
    try_write_jp2_fixtures(&output_dir)?;

    write_golden_geojson(&output_dir)?;
    println!("wrote footprint fixtures to {}", output_dir.display());
    Ok(())
}

fn write_affine_full(path: &Path) -> Result<()> {
    let width = 64usize;
    let height = 48usize;
    let mut data = Array3::<u8>::zeros((height, width, 3));
    for y in 0..height {
        for x in 0..width {
            data[[y, x, 0]] = 180;
            data[[y, x, 1]] = 90;
            data[[y, x, 2]] = 30;
        }
    }
    GeoTiffBuilder::new(width as u32, height as u32)
        .bands(3)
        .epsg(4326)
        .pixel_scale(0.01, 0.01)
        .origin(-1.0, 1.0)
        .write_3d(path, data.view())
        .with_context(|| format!("write {}", path.display()))
}

fn write_masked_rgba(path: &Path) -> Result<()> {
    let width = 64usize;
    let height = 48usize;
    let mut data = Array3::<u8>::zeros((height, width, 4));
    for y in 0..height {
        for x in 0..width {
            let hole = x >= 20 && x < 44 && y >= 14 && y < 34;
            data[[y, x, 0]] = 200;
            data[[y, x, 1]] = 100;
            data[[y, x, 2]] = 50;
            data[[y, x, 3]] = if hole { 0 } else { 255 };
        }
    }
    GeoTiffBuilder::new(width as u32, height as u32)
        .bands(4)
        .photometric(PhotometricInterpretation::Rgb)
        .extra_samples(vec![ExtraSample::AssociatedAlpha])
        .epsg(4326)
        .pixel_scale(0.01, 0.01)
        .origin(-1.0, 1.0)
        .write_3d(path, data.view())
        .with_context(|| format!("write {}", path.display()))
}

fn write_sar_nonzero(path: &Path) -> Result<()> {
    let width = 80usize;
    let height = 60usize;
    let mut data = Array2::<u16>::zeros((height, width));
    for y in 0..height {
        for x in 0..width {
            let border = x < 8 || y < 8 || x >= width - 8 || y >= height - 8;
            data[[y, x]] = if border { 0 } else { 1200 + ((x + y) % 50) as u16 };
        }
    }
    GeoTiffBuilder::new(width as u32, height as u32)
        .bands(1)
        .photometric(PhotometricInterpretation::MinIsBlack)
        .epsg(4326)
        .pixel_scale(0.02, 0.02)
        .origin(10.0, 30.0)
        .write_2d(path, data.view())
        .with_context(|| format!("write {}", path.display()))
}

fn write_gcp_scattered(path: &Path) -> Result<()> {
    let width = 40u32;
    let height = 30u32;
    let mut data = Array2::<u8>::from_elem((height as usize, width as usize), 128);
    for y in 0..height as usize {
        for x in 0..width as usize {
            if x > 4 && x < width as usize - 5 && y > 3 && y < height as usize - 4 {
                data[[y, x]] = 200;
            }
        }
    }
    let tiepoints = vec![
        0.0, 0.0, 0.0, 10.0, 30.0, 0.0, //
        39.0, 0.0, 0.0, 11.0, 30.0, 0.0, //
        0.0, 29.0, 0.0, 10.0, 29.0, 0.0, //
        39.0, 29.0, 0.0, 11.0, 29.0, 0.0, //
        20.0, 15.0, 0.0, 10.5, 29.5, 0.0, //
    ];
    GeoTiffBuilder::new(width, height)
        .bands(1)
        .photometric(PhotometricInterpretation::MinIsBlack)
        .model_tiepoints(tiepoints)
        .write_2d(path, data.view())
        .with_context(|| format!("write {}", path.display()))
}

fn write_dem_slope(path: &Path) -> Result<()> {
    let width = 32usize;
    let height = 32usize;
    let mut data = Array2::<f32>::zeros((height, width));
    for y in 0..height {
        for x in 0..width {
            data[[y, x]] = 100.0 + x as f32 * 2.0 + y as f32;
        }
    }
    GeoTiffBuilder::new(width as u32, height as u32)
        .bands(1)
        .epsg(4326)
        .pixel_scale(0.01, 0.01)
        .origin(10.0, 30.0)
        .write_2d(path, data.view())
        .with_context(|| format!("write {}", path.display()))
}

fn try_write_jp2_fixtures(output_dir: &Path) -> Result<()> {
    let rgb = output_dir.join("affine_full.tif");
    let jp2 = output_dir.join("sample_rgba.jp2");
    let Ok(status) = Command::new("gdal")
        .args([
            "translate",
            "-of",
            "JP2OpenJPEG",
            rgb.to_str().unwrap(),
            jp2.to_str().unwrap(),
            "-co",
            "QUALITY=80",
        ])
        .status()
    else {
        return Ok(());
    };
    if status.success() {
        let _ = Command::new("gdal")
            .args([
                "translate",
                "-of",
                "JP2OpenJPEG",
                output_dir.join("masked_rgba.tif").to_str().unwrap(),
                output_dir.join("sample_masked.jp2").to_str().unwrap(),
            ])
            .status();
    }
    Ok(())
}

fn write_golden_geojson(output_dir: &Path) -> Result<()> {
    let cases = [
        ("affine_full.tif", "affine_full.geojson", FootprintOptions {
            source: ValiditySourceChoice::Full,
            ..FootprintOptions::default()
        }),
        ("masked_rgba.tif", "masked_rgba.geojson", FootprintOptions::default()),
        ("sar_nonzero.tif", "sar_nonzero.geojson", FootprintOptions::default()),
        ("gcp_scattered.tif", "gcp_scattered.geojson", FootprintOptions {
            source: ValiditySourceChoice::Full,
            ..FootprintOptions::default()
        }),
    ];
    for (input, golden, opts) in cases {
        let path = output_dir.join(input);
        let result = extract_footprint(&path.to_string_lossy(), false, None, &opts, 0)?;
        fs::write(output_dir.join("golden").join(golden), result.geojson)?;
    }
    Ok(())
}
