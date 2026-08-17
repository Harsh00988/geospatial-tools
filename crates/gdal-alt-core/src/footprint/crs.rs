use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Once;

use proj::Proj;

static PROJ_ENV: Once = Once::new();

fn init_proj_env() {
    PROJ_ENV.call_once(|| {
        // Avoid stale system/anaconda PROJ databases breaking bundled PROJ.
        std::env::remove_var("PROJ_LIB");
        std::env::remove_var("PROJ_DATA");
    });
}

/// Reproject map coordinates to WGS84 lon/lat for GeoJSON output.
///
/// GeoJSON (RFC 7946) expects `[longitude, latitude]` in EPSG:4326.
pub fn to_wgs84(epsg: Option<u32>, x: f64, y: f64) -> (f64, f64) {
    let Some(epsg) = epsg else {
        return (x, y);
    };
    if epsg == 4326 {
        return (x, y);
    }
    if epsg == 3857 {
        return web_mercator_to_wgs84(x, y);
    }
    match transform_epsg(epsg, 4326, x, y) {
        Ok((lon, lat)) => (lon, lat),
        Err(err) => {
            eprintln!("footprint: EPSG:{epsg} -> EPSG:4326 failed ({err}); passing coordinates through");
            (x, y)
        }
    }
}

fn web_mercator_to_wgs84(x: f64, y: f64) -> (f64, f64) {
    const R: f64 = 6_378_137.0;
    let lon = (x / R).to_degrees();
    let lat = (2.0 * (y / R).exp().atan() - std::f64::consts::FRAC_PI_2).to_degrees();
    (lon, lat)
}

/// Reproject WGS84 lon/lat to a target EPSG (inverse of [`to_wgs84`]).
pub fn from_wgs84(epsg: u32, lon: f64, lat: f64) -> (f64, f64) {
    if epsg == 4326 {
        return (lon, lat);
    }
    if epsg == 3857 {
        const R: f64 = 6_378_137.0;
        let x = lon.to_radians() * R;
        let y = ((lat.to_radians() / 2.0 + std::f64::consts::FRAC_PI_4).tan()).ln() * R;
        return (x, y);
    }
    match transform_epsg(4326, epsg, lon, lat) {
        Ok(point) => point,
        Err(err) => {
            eprintln!("footprint: EPSG:4326 -> EPSG:{epsg} failed ({err}); passing coordinates through");
            (lon, lat)
        }
    }
}

fn transform_epsg(from: u32, to: u32, x: f64, y: f64) -> Result<(f64, f64), proj::ProjError> {
    init_proj_env();
    thread_local! {
        static CACHE: RefCell<HashMap<(u32, u32), Proj>> = RefCell::new(HashMap::new());
    }

    CACHE.with(|cache| -> Result<(f64, f64), proj::ProjError> {
        let mut cache = cache.borrow_mut();
        if !cache.contains_key(&(from, to)) {
            let from_crs = format!("EPSG:{from}");
            let to_crs = format!("EPSG:{to}");
            let proj = Proj::new_known_crs(&from_crs, &to_crs, None)
                .map_err(|err| proj::ProjError::Projection(err.to_string()))?;
            cache.insert((from, to), proj);
        }
        let proj = cache.get(&(from, to)).expect("transformer cached");
        proj.convert((x, y))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_mercator_corner_matches_gdal_reference() {
        let x = 7_449_552.776_673_075;
        let y = 2_857_827.613_526_296;
        let (lon, lat) = to_wgs84(Some(3857), x, y);
        assert!((lon - 66.920_471_191).abs() < 1e-5, "lon={lon}");
        assert!((lat - 24.854_042_167).abs() < 1e-5, "lat={lat}");
    }

    #[test]
    fn wgs84_is_identity() {
        let (lon, lat) = to_wgs84(Some(4326), 66.9, 24.85);
        assert_eq!(lon, 66.9);
        assert_eq!(lat, 24.85);
    }

    #[test]
    fn utm_zone_42n_reprojects_to_plausible_lon_lat() {
        let easting = 745_520.0;
        let northing = 2_854_158.0;
        let (lon, lat) = to_wgs84(Some(32642), easting, northing);
        assert!((50.0..90.0).contains(&lon), "lon={lon}");
        assert!((20.0..30.0).contains(&lat), "lat={lat}");
    }
}
