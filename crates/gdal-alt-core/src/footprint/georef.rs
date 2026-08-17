use anyhow::Result;
use geotiff_core::GeoTransform;
use geotiff_reader::GeoTiffFile;

use crate::input::RasterProfile;

use super::rpc::{read_rpc_model, RpcModel};
use super::tps::GcpTps;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FootprintGeorefKind {
    Affine,
    GcpGrid,
    GcpTps,
    GcpAffine,
    Rpc,
    Pixel,
}

impl FootprintGeorefKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Affine => "affine",
            Self::GcpGrid => "gcp_grid",
            Self::GcpTps => "gcp_tps",
            Self::GcpAffine => "gcp_affine",
            Self::Rpc => "rpc",
            Self::Pixel => "pixel",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FootprintGeorefState {
    pub georef: FootprintGeoref,
    pub kind: FootprintGeorefKind,
    pub col_off: f64,
    pub row_off: f64,
}

impl FootprintGeorefState {
    pub fn pixel_to_geo(&self, col: f64, row: f64) -> (f64, f64) {
        self.georef
            .pixel_to_geo(col + self.col_off, row + self.row_off)
    }
}

#[derive(Debug, Clone)]
pub enum FootprintGeoref {
    Affine(GeoTransform),
    GcpGrid(GcpGrid),
    GcpTps(GcpTps),
    Rpc(RpcModel),
    Pixel,
}

impl FootprintGeoref {
    pub fn pixel_to_geo(&self, col: f64, row: f64) -> (f64, f64) {
        match self {
            Self::Affine(transform) => transform.pixel_to_geo(col, row),
            Self::GcpGrid(grid) => grid.pixel_to_geo(col, row),
            Self::GcpTps(model) => model.pixel_to_geo(col, row),
            Self::Rpc(model) => model.pixel_to_geo(col, row),
            Self::Pixel => (col, row),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GcpGrid {
    col_values: Vec<f64>,
    row_values: Vec<f64>,
    lon: Vec<f64>,
    lat: Vec<f64>,
}

impl GcpGrid {
    pub fn try_from_tiepoints(tiepoints: &[f64]) -> Option<Self> {
        if tiepoints.len() < 12 || tiepoints.len() % 6 != 0 {
            return None;
        }

        let mut points = Vec::with_capacity(tiepoints.len() / 6);
        for chunk in tiepoints.chunks(6) {
            points.push((chunk[0], chunk[1], chunk[3], chunk[4]));
        }

        let mut col_values = points.iter().map(|point| point.0).collect::<Vec<_>>();
        col_values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
        col_values.dedup_by(|left, right| (*left - *right).abs() < 1e-6);

        let mut row_values = points.iter().map(|point| point.1).collect::<Vec<_>>();
        row_values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
        row_values.dedup_by(|left, right| (*left - *right).abs() < 1e-6);

        let ncols = col_values.len();
        let nrows = row_values.len();
        let mut lon = vec![f64::NAN; ncols * nrows];
        let mut lat = vec![f64::NAN; ncols * nrows];

        for (col, row, x, y) in points {
            let col_idx = find_axis_index(&col_values, col)?;
            let row_idx = find_axis_index(&row_values, row)?;
            let index = row_idx * ncols + col_idx;
            lon[index] = x;
            lat[index] = y;
        }

        if lon.iter().any(|value| !value.is_finite()) {
            return None;
        }

        Some(Self {
            col_values,
            row_values,
            lon,
            lat,
        })
    }

    pub fn pixel_to_geo(&self, col: f64, row: f64) -> (f64, f64) {
        let (col0, col1, tc) = bracket(&self.col_values, col);
        let (row0, row1, tr) = bracket(&self.row_values, row);
        let ncols = self.col_values.len();

        let lon00 = self.lon[row0 * ncols + col0];
        let lon10 = self.lon[row0 * ncols + col1];
        let lon01 = self.lon[row1 * ncols + col0];
        let lon11 = self.lon[row1 * ncols + col1];

        let lat00 = self.lat[row0 * ncols + col0];
        let lat10 = self.lat[row0 * ncols + col1];
        let lat01 = self.lat[row1 * ncols + col0];
        let lat11 = self.lat[row1 * ncols + col1];

        let lon = bilinear(lon00, lon10, lon01, lon11, tc, tr);
        let lat = bilinear(lat00, lat10, lat01, lat11, tc, tr);
        (lon, lat)
    }
}

pub fn resolve_footprint_georef(
    input: &GeoTiffFile,
    profile: &RasterProfile,
) -> Result<(FootprintGeoref, FootprintGeorefKind)> {
    if let Some(transform) = input.transform() {
        return Ok((FootprintGeoref::Affine(*transform), FootprintGeorefKind::Affine));
    }
    if let Some(affine) = profile.georef.affine {
        return Ok((FootprintGeoref::Affine(affine), FootprintGeorefKind::Affine));
    }
    if let Some(matrix) = profile.georef.transformation_matrix {
        return Ok((
            FootprintGeoref::Affine(GeoTransform::from_transformation_matrix(&matrix)),
            FootprintGeorefKind::Affine,
        ));
    }
    if let Some(matrix) = input.metadata().transformation.as_ref() {
        return Ok((
            FootprintGeoref::Affine(GeoTransform::from_transformation_matrix(matrix)),
            FootprintGeorefKind::Affine,
        ));
    }
    if let Some(scale) = input.metadata().pixel_scale {
        if let Some(tiepoint) = input.metadata().tiepoints.first() {
            let raster_type = profile.georef.crs.raster_type_enum();
            return Ok((
                FootprintGeoref::Affine(GeoTransform::from_tiepoint_and_scale_with_raster_type(
                    tiepoint, &scale, raster_type,
                )),
                FootprintGeorefKind::Affine,
            ));
        }
    }
    if let Some(model) = read_rpc_model(input) {
        return Ok((FootprintGeoref::Rpc(model), FootprintGeorefKind::Rpc));
    }

    let tiepoints = collect_tiepoints(input, profile);
    if let Some(grid) = GcpGrid::try_from_tiepoints(&tiepoints) {
        return Ok((FootprintGeoref::GcpGrid(grid), FootprintGeorefKind::GcpGrid));
    }
    if let Some(model) = GcpTps::try_from_tiepoints(&tiepoints) {
        return Ok((FootprintGeoref::GcpTps(model), FootprintGeorefKind::GcpTps));
    }
    if let Some(transform) = fit_affine_from_tiepoints(&tiepoints) {
        return Ok((
            FootprintGeoref::Affine(transform),
            FootprintGeorefKind::GcpAffine,
        ));
    }

    Ok((FootprintGeoref::Pixel, FootprintGeorefKind::Pixel))
}

pub fn resolve_footprint_georef_profile(profile: &RasterProfile) -> (FootprintGeoref, FootprintGeorefKind) {
    if let Some(affine) = profile.georef.affine {
        return (FootprintGeoref::Affine(affine), FootprintGeorefKind::Affine);
    }
    if let Some(matrix) = profile.georef.transformation_matrix {
        return (
            FootprintGeoref::Affine(GeoTransform::from_transformation_matrix(&matrix)),
            FootprintGeorefKind::Affine,
        );
    }
    if let Some(tiepoints) = profile.georef.model_tiepoints.as_ref() {
        if let Some(grid) = GcpGrid::try_from_tiepoints(tiepoints) {
            return (FootprintGeoref::GcpGrid(grid), FootprintGeorefKind::GcpGrid);
        }
        if let Some(model) = GcpTps::try_from_tiepoints(tiepoints) {
            return (FootprintGeoref::GcpTps(model), FootprintGeorefKind::GcpTps);
        }
        if let Some(transform) = fit_affine_from_tiepoints(tiepoints) {
            return (
                FootprintGeoref::Affine(transform),
                FootprintGeorefKind::GcpAffine,
            );
        }
    }

    (FootprintGeoref::Pixel, FootprintGeorefKind::Pixel)
}

fn collect_tiepoints(input: &GeoTiffFile, profile: &RasterProfile) -> Vec<f64> {
    if let Some(tiepoints) = &profile.georef.model_tiepoints {
        if !tiepoints.is_empty() {
            return tiepoints.clone();
        }
    }
    input
        .metadata()
        .tiepoints
        .iter()
        .flat_map(|tiepoint| tiepoint.iter().copied())
        .collect()
}

fn fit_affine_from_tiepoints(tiepoints: &[f64]) -> Option<GeoTransform> {
    if tiepoints.len() < 18 || tiepoints.len() % 6 != 0 {
        return None;
    }
    let points: Vec<(f64, f64, f64, f64)> = tiepoints
        .chunks(6)
        .map(|chunk| (chunk[0], chunk[1], chunk[3], chunk[4]))
        .collect();
    fit_affine_from_points(&points)
}

fn fit_affine_from_points(points: &[(f64, f64, f64, f64)]) -> Option<GeoTransform> {
    if points.len() < 3 {
        return None;
    }

    let mut ata = [[0.0; 3]; 3];
    let mut atx = [0.0; 3];
    let mut aty = [0.0; 3];
    for (col, row, x, y) in points {
        let row_vec = [1.0, *col, *row];
        for i in 0..3 {
            atx[i] += row_vec[i] * x;
            aty[i] += row_vec[i] * y;
            for j in 0..3 {
                ata[i][j] += row_vec[i] * row_vec[j];
            }
        }
    }

    let ax = solve_3x3(ata, atx)?;
    let ay = solve_3x3(ata, aty)?;
    Some(GeoTransform {
        origin_x: ax[0],
        pixel_width: ax[1],
        skew_x: ax[2],
        origin_y: ay[0],
        skew_y: ay[1],
        pixel_height: ay[2],
    })
}

fn solve_3x3(matrix: [[f64; 3]; 3], rhs: [f64; 3]) -> Option<[f64; 3]> {
    let mut a = matrix;
    let mut b = rhs;
    for pivot in 0..3 {
        let mut max_row = pivot;
        for row in pivot + 1..3 {
            if a[row][pivot].abs() > a[max_row][pivot].abs() {
                max_row = row;
            }
        }
        if a[max_row][pivot].abs() <= f64::EPSILON {
            return None;
        }
        if max_row != pivot {
            a.swap(pivot, max_row);
            b.swap(pivot, max_row);
        }
        for row in pivot + 1..3 {
            let factor = a[row][pivot] / a[pivot][pivot];
            for col in pivot..3 {
                a[row][col] -= factor * a[pivot][col];
            }
            b[row] -= factor * b[pivot];
        }
    }
    let mut x = [0.0; 3];
    for row in (0..3).rev() {
        let mut sum = b[row];
        for col in row + 1..3 {
            sum -= a[row][col] * x[col];
        }
        if a[row][row].abs() <= f64::EPSILON {
            return None;
        }
        x[row] = sum / a[row][row];
    }
    Some(x)
}

fn find_axis_index(values: &[f64], value: f64) -> Option<usize> {
    values
        .iter()
        .position(|candidate| (*candidate - value).abs() < 1e-6)
}

fn bracket(values: &[f64], value: f64) -> (usize, usize, f64) {
    if values.is_empty() {
        return (0, 0, 0.0);
    }
    if values.len() == 1 || value <= values[0] {
        return (0, 0, 0.0);
    }
    let last = values.len() - 1;
    if value >= values[last] {
        return (last, last, 0.0);
    }

    let upper = values.partition_point(|candidate| *candidate < value);
    let lower = upper - 1;
    let span = values[upper] - values[lower];
    let t = if span.abs() <= f64::EPSILON {
        0.0
    } else {
        (value - values[lower]) / span
    };
    (lower, upper, t)
}

fn bilinear(v00: f64, v10: f64, v01: f64, v11: f64, tx: f64, ty: f64) -> f64 {
    let top = v00 + tx * (v10 - v00);
    let bottom = v01 + tx * (v11 - v01);
    top + ty * (bottom - top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcp_grid_interpolates_corners() {
        let tiepoints = vec![
            0.0, 0.0, 0.0, 0.0, 10.0, 0.0, //
            10.0, 0.0, 0.0, 10.0, 10.0, 0.0, //
            0.0, 10.0, 0.0, 0.0, 0.0, 0.0, //
            10.0, 10.0, 0.0, 10.0, 0.0, 0.0, //
        ];
        let grid = GcpGrid::try_from_tiepoints(&tiepoints).expect("grid");
        assert_eq!(grid.pixel_to_geo(0.0, 0.0), (0.0, 10.0));
        assert_eq!(grid.pixel_to_geo(10.0, 10.0), (10.0, 0.0));
        assert_eq!(grid.pixel_to_geo(5.0, 5.0), (5.0, 5.0));
    }

    #[test]
    fn gcp_affine_fits_scattered_points() {
        let tiepoints = vec![
            0.0, 0.0, 0.0, 0.0, 0.0, 0.0, //
            10.0, 0.0, 0.0, 1.0, 0.0, 0.0, //
            0.0, 10.0, 0.0, 0.0, -1.0, 0.0, //
            10.0, 10.0, 0.0, 1.0, -1.0, 0.0, //
        ];
        let transform = fit_affine_from_tiepoints(&tiepoints).expect("affine");
        assert!((transform.pixel_to_geo(0.0, 0.0).0 - 0.0).abs() < 1e-9);
        assert!((transform.pixel_to_geo(10.0, 0.0).0 - 1.0).abs() < 1e-9);
        assert!((transform.pixel_to_geo(0.0, 10.0).1 - -1.0).abs() < 1e-9);
    }
}
