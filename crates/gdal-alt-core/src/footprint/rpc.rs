const RPC_COEFFICIENT_TAG: u16 = 50844;

#[derive(Debug, Clone)]
pub struct RpcModel {
    #[allow(dead_code)]
    pub err_bias: f64,
    #[allow(dead_code)]
    pub err_rand: f64,
    pub line_off: f64,
    pub samp_off: f64,
    pub lat_off: f64,
    pub long_off: f64,
    pub height_off: f64,
    pub line_scale: f64,
    pub samp_scale: f64,
    pub lat_scale: f64,
    pub long_scale: f64,
    pub height_scale: f64,
    pub line_num: [f64; 20],
    pub line_den: [f64; 20],
    pub samp_num: [f64; 20],
    pub samp_den: [f64; 20],
}

impl RpcModel {
    pub fn from_coefficients(values: &[f64]) -> Option<Self> {
        if values.len() < 92 {
            return None;
        }
        let take = |start: usize| values[start];
        let take20 = |start: usize| {
            let mut out = [0.0; 20];
            out.copy_from_slice(&values[start..start + 20]);
            out
        };
        Some(Self {
            err_bias: take(0),
            err_rand: take(1),
            line_off: take(2),
            samp_off: take(3),
            lat_off: take(4),
            long_off: take(5),
            height_off: take(6),
            line_scale: take(7),
            samp_scale: take(8),
            lat_scale: take(9),
            long_scale: take(10),
            height_scale: take(11),
            line_num: take20(12),
            line_den: take20(32),
            samp_num: take20(52),
            samp_den: take20(72),
        })
    }

    pub fn pixel_to_geo(&self, col: f64, row: f64) -> (f64, f64) {
        let line = row;
        let sample = col;
        let height = self.height_off;
        let mut lat = self.lat_off;
        let mut lon = self.long_off;

        for _ in 0..20 {
            let (pred_line, pred_sample) = self.geo_to_pixel(lat, lon, height);
            let dline = line - pred_line;
            let dsamp = sample - pred_sample;
            if dline.abs() < 1e-9 && dsamp.abs() < 1e-9 {
                break;
            }
            let (dline_dlat, dline_dlon) = self.geo_to_pixel_derivatives(lat, lon, height, true);
            let (dsamp_dlat, dsamp_dlon) = self.geo_to_pixel_derivatives(lat, lon, height, false);
            let det = dline_dlat * dsamp_dlon - dline_dlon * dsamp_dlat;
            if det.abs() <= f64::EPSILON {
                break;
            }
            let dlat = (dline * dsamp_dlon - dsamp * dline_dlon) / det;
            let dlon = (-dline * dsamp_dlat + dsamp * dline_dlat) / det;
            lat += dlat;
            lon += dlon;
        }

        (lon, lat)
    }

    fn geo_to_pixel(&self, lat: f64, lon: f64, height: f64) -> (f64, f64) {
        let mut diff_lon = lon - self.long_off;
        if diff_lon < -270.0 {
            diff_lon += 360.0;
        } else if diff_lon > 270.0 {
            diff_lon -= 360.0;
        }
        let lon_n = diff_lon / self.long_scale;
        let lat_n = (lat - self.lat_off) / self.lat_scale;
        let height_n = (height - self.height_off) / self.height_scale;
        let terms = rpc_terms(lon_n, lat_n, height_n);
        let sample = eval_rational(&self.samp_num, &self.samp_den, &terms) * self.samp_scale
            + self.samp_off
            + 0.5;
        let line =
            eval_rational(&self.line_num, &self.line_den, &terms) * self.line_scale + self.line_off + 0.5;
        (line, sample)
    }

    fn geo_to_pixel_derivatives(
        &self,
        lat: f64,
        lon: f64,
        height: f64,
        line_component: bool,
    ) -> (f64, f64) {
        let eps = 1e-7;
        let (line0, sample0) = self.geo_to_pixel(lat, lon, height);
        let (line1, sample1) = self.geo_to_pixel(lat + eps, lon, height);
        let (line2, sample2) = self.geo_to_pixel(lat, lon + eps, height);
        if line_component {
            ((line1 - line0) / eps, (line2 - line0) / eps)
        } else {
            ((sample1 - sample0) / eps, (sample2 - sample0) / eps)
        }
    }
}

/// GDAL `RPCComputeTerms()` layout: normalized longitude, latitude, height.
fn rpc_terms(lon: f64, lat: f64, height: f64) -> [f64; 20] {
    [
        1.0,
        lon,
        lat,
        height,
        lon * lat,
        lon * height,
        lat * height,
        lon * lon,
        lat * lat,
        height * height,
        lon * lat * height,
        lon * lon * lon,
        lon * lat * lat,
        lon * height * height,
        lon * lon * lat,
        lat * lat * lat,
        lat * height * height,
        lon * lon * height,
        lat * lat * height,
        height * height * height,
    ]
}

fn eval_rational(num: &[f64; 20], den: &[f64; 20], terms: &[f64; 20]) -> f64 {
    let numerator: f64 = num.iter().zip(terms).map(|(c, t)| c * t).sum();
    let denominator: f64 = den.iter().zip(terms).map(|(c, t)| c * t).sum();
    if denominator.abs() <= f64::EPSILON {
        numerator
    } else {
        numerator / denominator
    }
}

pub fn read_rpc_model(input: &geotiff_reader::GeoTiffFile) -> Option<RpcModel> {
    use tiff_reader::TagValue;

    for ifd in input.tiff().ifds() {
        let Some(tag) = ifd.tag(RPC_COEFFICIENT_TAG) else {
            continue;
        };
        let TagValue::Double(values) = &tag.value else {
            continue;
        };
        if let Some(model) = RpcModel::from_coefficients(values) {
            return Some(model);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open::open_geotiff;
    use std::path::Path;

    #[test]
    fn rpc_terms_has_twenty_entries() {
        let terms = rpc_terms(0.1, -0.2, 0.0);
        assert_eq!(terms.len(), 20);
        assert_eq!(terms[0], 1.0);
    }

    #[test]
    fn iceye_rpc_matches_gdal_reference_when_available() {
        let path = Path::new(
            "/home/harsh.goyal@SUHORA.LOCAL/Downloads/test_data/ICEYE_X37_GRD_SCW_954079909_20260802T104048.tif",
        );
        if !path.is_file() {
            return;
        }
        let input = open_geotiff(path, false).expect("open");
        let rpc = read_rpc_model(&input).expect("rpc");
        let (lon, lat) = rpc.pixel_to_geo(0.0, 0.0);
        assert!((lon - 65.636875249).abs() < 1e-4, "lon delta {}", lon - 65.636875249);
        assert!((lat - 25.371831435).abs() < 1e-4, "lat delta {}", lat - 25.371831435);
        let (lon, lat) = rpc.pixel_to_geo(5000.0, 1000.0);
        assert!((lon - 66.202130905).abs() < 1e-4, "lon delta {}", lon - 66.202130905);
        assert!((lat - 25.146157723).abs() < 1e-4, "lat delta {}", lat - 25.146157723);
    }
}
