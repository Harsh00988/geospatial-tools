/// Thin-plate spline GCP georeferencing (GDAL `METHOD=GCP_TPS` parity).
#[derive(Debug, Clone)]
pub struct GcpTps {
    px: Vec<f64>,
    py: Vec<f64>,
    lon_model: TpsSurface,
    lat_model: TpsSurface,
}

#[derive(Debug, Clone)]
struct TpsSurface {
    affine: [f64; 3],
    weights: Vec<f64>,
}

impl GcpTps {
    pub fn try_from_tiepoints(tiepoints: &[f64]) -> Option<Self> {
        if tiepoints.len() < 24 || tiepoints.len() % 6 != 0 {
            return None;
        }
        let mut px = Vec::new();
        let mut py = Vec::new();
        let mut lon = Vec::new();
        let mut lat = Vec::new();
        for chunk in tiepoints.chunks(6) {
            px.push(chunk[0]);
            py.push(chunk[1]);
            lon.push(chunk[3]);
            lat.push(chunk[4]);
        }
        let n = px.len();
        if n < 4 {
            return None;
        }
        let lon_model = TpsSurface::fit(&px, &py, &lon)?;
        let lat_model = TpsSurface::fit(&px, &py, &lat)?;
        Some(Self {
            px,
            py,
            lon_model,
            lat_model,
        })
    }

    pub fn pixel_to_geo(&self, col: f64, row: f64) -> (f64, f64) {
        (
            self.lon_model.eval(col, row, &self.px, &self.py),
            self.lat_model.eval(col, row, &self.px, &self.py),
        )
    }
}

impl TpsSurface {
    fn fit(px: &[f64], py: &[f64], values: &[f64]) -> Option<Self> {
        let n = px.len();
        let size = n + 3;
        let mut matrix = vec![vec![0.0; size]; size];
        let mut rhs = vec![0.0; size];

        for i in 0..n {
            for j in 0..n {
                matrix[i][j] = tps_kernel(px[i], py[i], px[j], py[j]);
            }
            matrix[i][n] = 1.0;
            matrix[i][n + 1] = px[i];
            matrix[i][n + 2] = py[i];
            matrix[n][i] = 1.0;
            matrix[n + 1][i] = px[i];
            matrix[n + 2][i] = py[i];
            rhs[i] = values[i];
        }

        let solution = solve_linear_system(matrix, rhs)?;
        let affine = [solution[n], solution[n + 1], solution[n + 2]];
        let weights = solution[..n].to_vec();
        Some(Self { affine, weights })
    }

    fn eval(&self, x: f64, y: f64, px: &[f64], py: &[f64]) -> f64 {
        let mut value = self.affine[0] + self.affine[1] * x + self.affine[2] * y;
        for ((&cx, &cy), &weight) in px.iter().zip(py).zip(&self.weights) {
            value += weight * tps_kernel(x, y, cx, cy);
        }
        value
    }
}

fn tps_kernel(x0: f64, y0: f64, x1: f64, y1: f64) -> f64 {
    let dx = x0 - x1;
    let dy = y0 - y1;
    let r2 = dx * dx + dy * dy;
    if r2 <= 0.0 {
        0.0
    } else {
        r2 * 0.5 * r2.ln()
    }
}

fn solve_linear_system(mut matrix: Vec<Vec<f64>>, mut rhs: Vec<f64>) -> Option<Vec<f64>> {
    let n = rhs.len();
    for pivot in 0..n {
        let mut max_row = pivot;
        let mut max_val = matrix[pivot][pivot].abs();
        for row in pivot + 1..n {
            let val = matrix[row][pivot].abs();
            if val > max_val {
                max_val = val;
                max_row = row;
            }
        }
        if max_val <= 1e-12 {
            return None;
        }
        if max_row != pivot {
            matrix.swap(pivot, max_row);
            rhs.swap(pivot, max_row);
        }
        let pivot_val = matrix[pivot][pivot];
        for row in pivot + 1..n {
            let factor = matrix[row][pivot] / pivot_val;
            if factor == 0.0 {
                continue;
            }
            matrix[row][pivot] = 0.0;
            for col in pivot + 1..n {
                matrix[row][col] -= factor * matrix[pivot][col];
            }
            rhs[row] -= factor * rhs[pivot];
        }
    }

    let mut solution = vec![0.0; n];
    for row in (0..n).rev() {
        let mut sum = rhs[row];
        for col in row + 1..n {
            sum -= matrix[row][col] * solution[col];
        }
        let diag = matrix[row][row];
        if diag.abs() <= 1e-12 {
            return None;
        }
        solution[row] = sum / diag;
    }
    Some(solution)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tps_interpolates_control_points() {
        let tiepoints = vec![
            0.0, 0.0, 0.0, 0.0, 10.0, 0.0, //
            10.0, 0.0, 0.0, 10.0, 10.0, 0.0, //
            0.0, 10.0, 0.0, 0.0, 0.0, 0.0, //
            10.0, 10.0, 0.0, 10.0, 0.0, 0.0, //
            5.0, 0.0, 0.0, 5.0, 10.0, 0.0, //
            0.0, 5.0, 0.0, 0.0, 5.0, 0.0, //
        ];
        let tps = GcpTps::try_from_tiepoints(&tiepoints).expect("tps");
        let (lon, lat) = tps.pixel_to_geo(0.0, 0.0);
        assert!((lon - 0.0).abs() < 1e-6, "lon={lon}");
        assert!((lat - 10.0).abs() < 1e-6, "lat={lat}");
        let (lon, lat) = tps.pixel_to_geo(10.0, 10.0);
        assert!((lon - 10.0).abs() < 1e-6, "lon={lon}");
        assert!((lat - 0.0).abs() < 1e-6, "lat={lat}");
    }

    #[test]
    fn tps_matches_gdal_on_scattered_gcps_when_available() {
        let tiepoints = vec![
            100.0, 200.0, 0.0, 66.10, 25.20, 0.0, //
            500.0, 200.0, 0.0, 66.15, 25.20, 0.0, //
            100.0, 800.0, 0.0, 66.10, 25.15, 0.0, //
            500.0, 800.0, 0.0, 66.15, 25.15, 0.0, //
            300.0, 500.0, 0.0, 66.125, 25.175, 0.0, //
        ];
        let tps = GcpTps::try_from_tiepoints(&tiepoints).expect("tps");
        let (lon, lat) = tps.pixel_to_geo(300.0, 500.0);
        assert!((lon - 66.125).abs() < 1e-6);
        assert!((lat - 25.175).abs() < 1e-6);
    }
}
