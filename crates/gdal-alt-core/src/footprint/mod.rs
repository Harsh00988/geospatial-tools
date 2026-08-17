mod crs;
mod dem;
mod georef;
mod mask;
mod mask_jp2;
mod options;
mod parity;
mod rpc;
mod tps;
mod trace;

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Result};
use geo::algorithm::simplify::Simplify;
use geo::{Coord, LineString, MultiPolygon, Polygon};
use geojson::{Feature, FeatureCollection, GeoJson, Geometry, Value};
use geotiff_reader::GeoTiffFile;
use rayon::ThreadPool;
use serde_json::json;

use crate::crop::WriteWindow;
use crate::input::{detect_source, InputFormat, RasterProfile};
use crate::jp2::{open_jp2_source, resolve_georef, Jp2Raster};
use crate::open::open_geotiff;
use crate::util::thread_pool;

use georef::{
    resolve_footprint_georef_profile, resolve_footprint_georef_with_choice, FootprintGeoref,
    FootprintGeorefKind, FootprintGeorefState,
};

use dem::DemSampler;

pub use georef::FootprintGeorefChoice;
pub use options::{FootprintOptions, FootprintOutputFormat, ResolvedValiditySource, ValiditySourceChoice};
pub use parity::{bbox_iou, hausdorff_distance_degrees, metrics_close, ring_metrics_from_geojson, RingMetrics};

#[derive(Debug, Clone)]
pub struct FootprintResult {
    /// Primary geometry output (GeoJSON, WKT, or flat coordinates).
    pub body: String,
    /// GeoJSON output (alias for `body` when format is GeoJSON).
    pub geojson: String,
    pub validity_source: String,
    pub georef_source: String,
    pub ring_count: usize,
    pub vertex_count: usize,
}

pub fn extract_footprint(
    path: &str,
    mmap: bool,
    window: Option<WriteWindow>,
    opts: &FootprintOptions,
    jobs: usize,
) -> Result<FootprintResult> {
    let pool = thread_pool(jobs)?;
    let dem = load_dem_sampler(opts)?;
    match detect_source(path) {
        InputFormat::GeoTiff => {
            let input = open_geotiff(Path::new(path), mmap)?;
            let profile = RasterProfile::from_geotiff(&input)?;
            extract_footprint_geotiff(&input, &profile, window, opts, &pool, dem)
        }
        InputFormat::Jp2 => extract_footprint_jp2(path, mmap, window, opts, &pool, dem),
    }
}

fn load_dem_sampler(opts: &FootprintOptions) -> Result<Option<Arc<DemSampler>>> {
    let Some(path) = opts.dem_path.as_deref() else {
        return Ok(None);
    };
    let sampler = DemSampler::open(Path::new(path), false)?;
    Ok(Some(Arc::new(sampler)))
}

fn apply_rpc_options(georef: &mut FootprintGeorefState, opts: &FootprintOptions, dem: Option<Arc<DemSampler>>) {
    let FootprintGeoref::Rpc(rpc) = &mut georef.georef else {
        return;
    };
    let updated = if let Some(dem) = dem {
        rpc.clone().with_dem(dem)
    } else if let Some(height) = opts.rpc_height {
        rpc.clone().with_height(height)
    } else {
        return;
    };
    *rpc = updated;
}

pub fn extract_footprint_jp2(
    path: &str,
    mmap: bool,
    window: Option<WriteWindow>,
    opts: &FootprintOptions,
    pool: &ThreadPool,
    dem: Option<Arc<DemSampler>>,
) -> Result<FootprintResult> {
    let (source, local_path) = open_jp2_source(path, mmap)?;
    let data = source.as_ref();
    let raster = Jp2Raster::open(data)?;
    let georef = resolve_georef(data, local_path.as_deref())?;
    let mut profile = RasterProfile::from_jp2(&raster, georef);
    if let Some(win) = window {
        profile = profile.with_window(&win);
    }

    let source_kind = options::resolve_validity_source_jp2(&profile, opts)?;
    let nodata = match source_kind {
        ResolvedValiditySource::Nodata => Some(
            options::nodata_value_for_profile(&profile)
                .ok_or_else(|| anyhow::anyhow!("no usable nodata value for footprint"))?,
        ),
        _ => options::nodata_value_for_profile(&profile),
    };

    let mut georef_state = resolve_footprint_georef_state_profile(&profile, None, opts)?;
    apply_rpc_options(&mut georef_state, opts, dem);
    let (width, height) = output_dimensions(&profile, window.as_ref());

    let validity = mask_jp2::build_validity_mask_jp2(
        data,
        &raster,
        &profile,
        source_kind,
        window.as_ref(),
        nodata,
        opts.zero_threshold,
        opts.tile_size,
        pool,
    )?;

    finish_footprint(
        validity,
        width,
        height,
        &georef_state,
        source_kind,
        profile.epsg(),
        opts,
    )
}

pub fn extract_footprint_geotiff(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    window: Option<WriteWindow>,
    opts: &FootprintOptions,
    pool: &ThreadPool,
    dem: Option<Arc<DemSampler>>,
) -> Result<FootprintResult> {
    let source = options::resolve_validity_source(input, profile, opts)?;
    let nodata = match source {
        ResolvedValiditySource::Nodata => Some(
            options::nodata_value_for_profile(profile)
                .ok_or_else(|| anyhow::anyhow!("no usable nodata value for footprint"))?,
        ),
        _ => options::nodata_value_for_profile(profile),
    };

    let mut georef = resolve_footprint_georef_state(input, profile, window.as_ref(), opts)?;
    apply_rpc_options(&mut georef, opts, dem);
    let (width, height) = output_dimensions(profile, window.as_ref());
    let native_epsg = profile.epsg().or_else(|| input.epsg());

    let validity = mask::build_validity_mask(
        input,
        profile,
        source,
        window.as_ref(),
        nodata,
        opts.zero_threshold,
        opts.tile_size,
        pool,
    )?;

    finish_footprint(
        validity,
        width,
        height,
        &georef,
        source,
        native_epsg,
        opts,
    )
}

fn finish_footprint(
    validity: mask::ValidityMask,
    width: usize,
    height: usize,
    georef: &FootprintGeorefState,
    source: ResolvedValiditySource,
    native_epsg: Option<u32>,
    opts: &FootprintOptions,
) -> Result<FootprintResult> {
    let mut pixel_rings = if validity.all_valid() {
        vec![full_extent_pixel_ring(width, height)]
    } else if validity.none_valid() {
        Vec::new()
    } else {
        trace::trace_rings(validity.as_slice(), width, height)
    };
    if should_keep_outer_only(opts, source) {
        pixel_rings = trace::select_largest_ring(pixel_rings);
    }

    let geo_rings = pixel_rings
        .into_iter()
        .map(|ring| pixel_ring_to_geo(&ring, georef, opts))
        .collect::<Result<Vec<_>>>()?;

    let projected = project_rings_for_output(geo_rings, georef.kind, native_epsg, opts)?;
    let ring_count = projected.len();
    let vertex_count = projected
        .iter()
        .map(|ring| ring.0.len().saturating_sub(1))
        .sum();
    let geojson = rings_to_geojson(
        projected.clone(),
        source,
        georef.kind,
        native_epsg,
    )?;
    let body = match opts.output_format {
        FootprintOutputFormat::GeoJson => geojson.clone(),
        FootprintOutputFormat::Wkt => rings_to_wkt(&projected)?,
        FootprintOutputFormat::WktFlat => rings_to_flat_coords(&projected),
    };

    Ok(FootprintResult {
        body,
        geojson,
        validity_source: source.label().to_owned(),
        georef_source: georef.kind.label().to_owned(),
        ring_count,
        vertex_count,
    })
}

fn should_keep_outer_only(opts: &FootprintOptions, source: ResolvedValiditySource) -> bool {
    opts.outer_only
        || (!opts.all_rings && source == ResolvedValiditySource::NonZero)
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
    opts: &FootprintOptions,
) -> Result<FootprintGeorefState> {
    let choice = opts.georef.unwrap_or(FootprintGeorefChoice::Auto);
    let mut state = {
        let (georef, kind) =
            resolve_footprint_georef_with_choice(input, profile, choice, opts.tps_max_points)?;
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

fn resolve_footprint_georef_state_profile(
    profile: &RasterProfile,
    window: Option<&WriteWindow>,
    opts: &FootprintOptions,
) -> Result<FootprintGeorefState> {
    let mut state = {
        let (georef, kind) = resolve_footprint_georef_profile(profile, opts.tps_max_points);
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
    opts: &FootprintOptions,
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
    if opts.simplify_degrees <= 0.0 && opts.simplify_tolerance > 0.0 {
        Ok(line.simplify(&opts.simplify_tolerance))
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

fn project_rings_for_output(
    mut rings: Vec<LineString<f64>>,
    georef_kind: FootprintGeorefKind,
    epsg: Option<u32>,
    opts: &FootprintOptions,
) -> Result<Vec<LineString<f64>>> {
    if georef_kind != FootprintGeorefKind::Pixel {
        for ring in &mut rings {
            for coord in ring.coords_mut() {
                let (lon, lat) = crs::to_wgs84(epsg, coord.x, coord.y);
                coord.x = lon;
                coord.y = lat;
            }
            if opts.simplify_degrees > 0.0 {
                *ring = ring.simplify(&opts.simplify_degrees);
            }
        }
    }
    Ok(rings)
}

fn rings_to_geojson(
    rings: Vec<LineString<f64>>,
    source: ResolvedValiditySource,
    georef_kind: FootprintGeorefKind,
    epsg: Option<u32>,
) -> Result<String> {
    let crs = match georef_kind {
        FootprintGeorefKind::Pixel => Some("pixel".to_owned()),
        _ => Some("EPSG:4326".to_owned()),
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

fn rings_to_wkt(rings: &[LineString<f64>]) -> Result<String> {
    if rings.is_empty() {
        return Ok("POLYGON EMPTY".to_owned());
    }
    if rings.len() == 1 {
        return Ok(format!("POLYGON({})", ring_to_wkt_coords(&rings[0])));
    }
    let parts = rings
        .iter()
        .map(|ring| format!("({})", ring_to_wkt_coords(ring)))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("MULTIPOLYGON({parts})"))
}

fn ring_to_wkt_coords(ring: &LineString<f64>) -> String {
    ring.0
        .iter()
        .map(|coord| format!("{} {}", coord.x, coord.y))
        .collect::<Vec<_>>()
        .join(", ")
}

fn rings_to_flat_coords(rings: &[LineString<f64>]) -> String {
    rings
        .iter()
        .flat_map(|ring| ring.0.iter())
        .map(|coord| format!("{} {}", coord.x, coord.y))
        .collect::<Vec<_>>()
        .join(" ")
}
