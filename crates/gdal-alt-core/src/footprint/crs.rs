/// Reproject map coordinates to WGS84 lon/lat for GeoJSON output.
///
/// GeoJSON (RFC 7946) expects `[longitude, latitude]` in EPSG:4326. Native raster
/// CRS values (e.g. Web Mercator meters) must be converted before serialization.
pub fn to_wgs84(epsg: Option<u32>, x: f64, y: f64) -> (f64, f64) {
    match epsg {
        Some(3857) => web_mercator_to_wgs84(x, y),
        Some(4326) | None => (x, y),
        _ => (x, y),
    }
}

fn web_mercator_to_wgs84(x: f64, y: f64) -> (f64, f64) {
    const R: f64 = 6_378_137.0;
    let lon = (x / R).to_degrees();
    let lat = (2.0 * (y / R).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
    (lon, lat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_mercator_origin_is_null_island() {
        let (lon, lat) = to_wgs84(Some(3857), 0.0, 0.0);
        assert!((lon).abs() < 1e-9);
        assert!((lat).abs() < 1e-9);
    }

    #[test]
    fn sn33_corner_matches_gdal_reference() {
        // GDAL: pixel (0,0) of SN33 visual -> 24.854042167, 66.920471191
        let x = 7_449_552.776_673_075;
        let y = 2_857_827.613_526_296;
        let (lon, lat) = to_wgs84(Some(3857), x, y);
        assert!((lon - 66.920_471_191).abs() < 1e-6, "lon={lon}");
        assert!((lat - 24.854_042_167).abs() < 1e-6, "lat={lat}");
    }

    #[test]
    fn wgs84_is_identity() {
        let (lon, lat) = to_wgs84(Some(4326), 66.9, 24.85);
        assert_eq!(lon, 66.9);
        assert_eq!(lat, 24.85);
    }
}
