mod crs;
mod georef;
mod mask;
mod options;
mod rpc;
mod trace;

use std::path::Path;

use anyhow::{bail, Result};
use geo::algorithm::simplify::Simplify;
use geo::{Coord, LineString, MultiPolygon, Polygon};
use geojson::{Feature, FeatureCollection, GeoJson, Geometry, Value};
use geotiff_reader::GeoTiffFile;
use rayon::ThreadPool;
use serde_json::json;

use crate::crop::WriteWindow;
use crate::input::RasterProfile;
use crate::open::open_geotiff;
use crate::util::thread_pool;

use georef::{resolve_footprint_georef, FootprintGeoref, FootprintGeorefKind, FootprintGeorefState};

pub use options::{FootprintOptions, ResolvedValiditySource, ValiditySourceChoice};

#[derive(Debug, Clone)]
pub struct FootprintResult {
    pub geojson: String,
    pub validity_source: String,
    pub georef_source: String,
    pub ring_count: usize,
}

pub fn extract_footprint(
    path: &str,
    mmap: bool,
    window: Option<WriteWindow>,
    opts: &FootprintOptions,
    jobs: usize,
) -> Result<FootprintResult> {
    let input = open_geotiff(Path::new(path), mmap)?;
    let profile = RasterProfile::from_geotiff(&input)?;
    let pool = thread_pool(jobs)?;
    extract_footprint_geotiff(&input, &profile, window, opts, &pool)
}

pub fn extract_footprint_geotiff(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    window: Option<WriteWindow>,
    opts: &FootprintOptions,
    pool: &ThreadPool,
) -> Result<FootprintResult> {
    let source = options::resolve_validity_source(input, profile, opts)?;
    let nodata = match source {
        ResolvedValiditySource::Nodata => Some(
            options::nodata_value_for_profile(profile)
                .ok_or_else(|| anyhow::anyhow!("no usable nodata value for footprint"))?,
        ),
        _ => options::nodata_value_for_profile(profile),
    };

    let georef = resolve_footprint_georef_state(input, profile, window.as_ref())?;
    let (width, height) = output_dimensions(profile, window.as_ref());

    let validity = mask::build_validity_mask(
        input,
        profile,
        source,
        window.as_ref(),
        nodata,
        opts.tile_size,
        pool,
    )?;

    let pixel_rings = if validity.all_valid() {
        vec![full_extent_pixel_ring(width, height)]
    } else if validity.none_valid() {
        Vec::new()
    } else {
        trace::trace_rings(validity.as_slice(), width, height)
    };

    let geo_rings = pixel_rings
        .into_iter()
        .map(|ring| pixel_ring_to_geo(&ring, &georef, opts.simplify_tolerance))
        .collect::<Result<Vec<_>>>()?;

    let ring_count = geo_rings.len();
    let geojson = rings_to_geojson(geo_rings, source, georef.kind, input.epsg())?;

    Ok(FootprintResult {
        geojson,
        validity_source: source.label().to_owned(),
        georef_source: georef.kind.label().to_owned(),
        ring_count,
    })
}

fn output_dimensions(profile: &RasterProfile, window: Option<&WriteWindow>) -> (usize, usize) {
    match window {
        Some(win) => (win.width, win.height),
        None => (profile.width as usize, profile.height as usize),
    }
}

fn resolve_footprint_georef_state(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    window: Option<&WriteWindow>,
) -> Result<FootprintGeorefState> {
    let mut state = {
        let (georef, kind) = resolve_footprint_georef(input, profile)?;
        FootprintGeorefState {
            georef,
            kind,
            col_off: 0.0,
            row_off: 0.0,
        }
    };
    if let Some(win) = window {
        if let FootprintGeoref::Affine(transform) = &state.georef {
            state.georef = FootprintGeoref::Affine(crate::crop::shift_transform(
                transform,
                win.col_off,
                win.row_off,
            ));
        } else {
            state.col_off = win.col_off as f64;
            state.row_off = win.row_off as f64;
        }
    }
    Ok(state)
}

fn pixel_ring_to_geo(
    ring: &[(f64, f64)],
    georef: &FootprintGeorefState,
    simplify_tolerance: f64,
) -> Result<LineString<f64>> {
    if ring.len() < 3 {
        bail!("footprint ring has fewer than three vertices");
    }
    let mut coords: Vec<Coord<f64>> = ring
        .iter()
        .map(|&(col, row)| {
            let (x, y) = georef.pixel_to_geo(col, row);
            Coord { x, y }
        })
        .collect();
    if coords.first() != coords.last() {
        coords.push(coords[0]);
    }
    let line = LineString::from(coords);
    if simplify_tolerance > 0.0 {
        Ok(line.simplify(&simplify_tolerance))
    } else {
        Ok(line)
    }
}

fn full_extent_pixel_ring(width: usize, height: usize) -> Vec<(f64, f64)> {
    vec![
        (0.0, 0.0),
        (width as f64, 0.0),
        (width as f64, height as f64),
        (0.0, height as f64),
        (0.0, 0.0),
    ]
}

fn rings_to_geojson(
    mut rings: Vec<LineString<f64>>,
    source: ResolvedValiditySource,
    georef_kind: FootprintGeorefKind,
    epsg: Option<u32>,
) -> Result<String> {
    let crs = match georef_kind {
        FootprintGeorefKind::Pixel => Some("pixel".to_owned()),
        _ => {
            for ring in &mut rings {
                for coord in ring.coords_mut() {
                    let (lon, lat) = crs::to_wgs84(epsg, coord.x, coord.y);
                    coord.x = lon;
                    coord.y = lat;
                }
            }
            Some("EPSG:4326".to_owned())
        }
    };
    if rings.is_empty() {
        let fc = FeatureCollection {
            bbox: None,
            features: vec![Feature {
                bbox: None,
                geometry: None,
                id: None,
                foreign_members: None,
                properties: serde_json::from_value(json!({
                    "validity_source": source.label(),
                    "georef_source": georef_kind.label(),
                    "crs": crs,
                    "epsg": if georef_kind == FootprintGeorefKind::Pixel { epsg } else { Some(4326u32) },
                    "native_epsg": epsg,
                    "empty": true,
                }))
                .ok(),
            }],
            foreign_members: None,
        };
        return Ok(GeoJson::FeatureCollection(fc).to_string());
    }

    let polygons: Vec<Polygon<f64>> = rings
        .into_iter()
        .map(|ring| Polygon::new(ring, vec![]))
        .collect();
    let geometry = Geometry::new(Value::from(&MultiPolygon(polygons)));
    let feature = Feature {
        bbox: None,
        geometry: Some(geometry),
        id: None,
        foreign_members: None,
        properties: serde_json::from_value(json!({
            "validity_source": source.label(),
            "georef_source": georef_kind.label(),
            "crs": crs,
            "epsg": if georef_kind == FootprintGeorefKind::Pixel { epsg } else { Some(4326u32) },
            "native_epsg": epsg,
        }))
        .ok(),
    };
    let fc = FeatureCollection {
        bbox: None,
        features: vec![feature],
        foreign_members: None,
    };
    Ok(GeoJson::FeatureCollection(fc).to_string())
}
