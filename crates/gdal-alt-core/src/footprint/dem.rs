use std::path::Path;

use anyhow::{Context, Result};
use geotiff_core::GeoTransform;
use geotiff_reader::GeoTiffFile;

use super::crs;

/// Bilinear sampler for a single-band elevation GeoTIFF used by RPC height refinement.
#[derive(Debug)]
pub struct DemSampler {
    width: u32,
    height: u32,
    native_epsg: Option<u32>,
    transform: GeoTransform,
    samples: Vec<f32>,
}

impl DemSampler {
    pub fn open(path: &Path, mmap: bool) -> Result<Self> {
        let input = crate::open::open_geotiff(path, mmap)
            .with_context(|| format!("failed to open DEM {}", path.display()))?;
        let transform = input
            .transform()
            .copied()
            .context("DEM has no GeoTransform")?;
        let width = input.width();
        let height = input.height();
        let samples = read_dem_band(&input)?;
        Ok(Self {
            width,
            height,
            native_epsg: input.epsg(),
            transform,
            samples,
        })
    }

    pub fn sample_at_wgs84(&self, lon: f64, lat: f64) -> Option<f64> {
        let (x, y) = if self.native_epsg == Some(4326) {
            (lon, lat)
        } else if let Some(epsg) = self.native_epsg {
            crs::from_wgs84(epsg, lon, lat)
        } else {
            (lon, lat)
        };
        let col = (x - self.transform.origin_x) / self.transform.pixel_width;
        let row = (y - self.transform.origin_y) / self.transform.pixel_height;
        if !(0.0..self.width as f64).contains(&col) || !(0.0..self.height as f64).contains(&row) {
            return None;
        }
        Some(bilinear_sample(
            &self.samples,
            self.width as usize,
            self.height as usize,
            col,
            row,
        ))
    }
}

fn read_dem_band(input: &GeoTiffFile) -> Result<Vec<f32>> {
    let width = input.width() as usize;
    let height = input.height() as usize;
    if let Ok(data) = input.read_band_window::<f32>(0, 0, 0, height, width) {
        return Ok(data.into_iter().collect());
    }
    if let Ok(data) = input.read_band_window::<f64>(0, 0, 0, height, width) {
        return Ok(data.into_iter().map(|v| v as f32).collect());
    }
    if let Ok(data) = input.read_band_window::<i16>(0, 0, 0, height, width) {
        return Ok(data.into_iter().map(|v| v as f32).collect());
    }
    if let Ok(data) = input.read_band_window::<u16>(0, 0, 0, height, width) {
        return Ok(data.into_iter().map(|v| v as f32).collect());
    }
    anyhow::bail!("unsupported DEM band type; expected float or integer elevation")
}

fn bilinear_sample(
    samples: &[f32],
    width: usize,
    height: usize,
    col: f64,
    row: f64,
) -> f64 {
    let col0 = col.floor().clamp(0.0, (width - 1) as f64) as usize;
    let row0 = row.floor().clamp(0.0, (height - 1) as f64) as usize;
    let col1 = (col0 + 1).min(width - 1);
    let row1 = (row0 + 1).min(height - 1);
    let tc = col - col0 as f64;
    let tr = row - row0 as f64;
    let v00 = samples[row0 * width + col0] as f64;
    let v10 = samples[row0 * width + col1] as f64;
    let v01 = samples[row1 * width + col0] as f64;
    let v11 = samples[row1 * width + col1] as f64;
    let top = v00 + tc * (v10 - v00);
    let bottom = v01 + tc * (v11 - v01);
    top + tr * (bottom - top)
}
