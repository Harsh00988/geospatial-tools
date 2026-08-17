use geo::{Coord, LineString};
use geojson::{GeoJson, Value};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RingMetrics {
    pub vertex_count: usize,
    pub area: f64,
    pub bbox: (f64, f64, f64, f64),
    pub centroid: (f64, f64),
}

pub fn ring_metrics_from_geojson(geojson: &str) -> Option<RingMetrics> {
    let value: GeoJson = geojson.parse().ok()?;
    let GeoJson::FeatureCollection(collection) = value else {
        return None;
    };
    let geometry = collection.features.first()?.geometry.as_ref()?;
    let ring = primary_ring(geometry)?;
    Some(ring_metrics(&ring))
}

pub fn ring_metrics(ring: &LineString<f64>) -> RingMetrics {
    let coords = &ring.0;
    let vertex_count = coords.len().saturating_sub(1);
    let area = ring_area_abs(coords);
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    for coord in coords {
        min_x = min_x.min(coord.x);
        min_y = min_y.min(coord.y);
        max_x = max_x.max(coord.x);
        max_y = max_y.max(coord.y);
        sum_x += coord.x;
        sum_y += coord.y;
    }
    let count = coords.len().max(1) as f64;
    RingMetrics {
        vertex_count,
        area,
        bbox: (min_x, min_y, max_x, max_y),
        centroid: (sum_x / count, sum_y / count),
    }
}

pub fn metrics_close(left: RingMetrics, right: RingMetrics, area_tol: f64, centroid_tol: f64) -> bool {
    (left.area - right.area).abs() <= area_tol
        && (left.centroid.0 - right.centroid.0).abs() <= centroid_tol
        && (left.centroid.1 - right.centroid.1).abs() <= centroid_tol
        && bbox_iou(left.bbox, right.bbox) >= 0.999
}

pub fn bbox_iou(
    left: (f64, f64, f64, f64),
    right: (f64, f64, f64, f64),
) -> f64 {
    let ix0 = left.0.max(right.0);
    let iy0 = left.1.max(right.1);
    let ix1 = left.2.min(right.2);
    let iy1 = left.3.min(right.3);
    if ix1 <= ix0 || iy1 <= iy0 {
        return 0.0;
    }
    let intersection = (ix1 - ix0) * (iy1 - iy0);
    let left_area = (left.2 - left.0) * (left.3 - left.1);
    let right_area = (right.2 - right.0) * (right.3 - right.1);
    intersection / (left_area + right_area - intersection)
}

pub fn hausdorff_distance_degrees(left: &LineString<f64>, right: &LineString<f64>) -> f64 {
    let mut max_dist = 0.0_f64;
    for a in &left.0 {
        let mut best = f64::INFINITY;
        for b in &right.0 {
            let dx = a.x - b.x;
            let dy = a.y - b.y;
            best = best.min((dx * dx + dy * dy).sqrt());
        }
        max_dist = max_dist.max(best);
    }
    for b in &right.0 {
        let mut best = f64::INFINITY;
        for a in &left.0 {
            let dx = a.x - b.x;
            let dy = a.y - b.y;
            best = best.min((dx * dx + dy * dy).sqrt());
        }
        max_dist = max_dist.max(best);
    }
    max_dist
}

fn ring_area_abs(coords: &[Coord<f64>]) -> f64 {
    if coords.len() < 3 {
        return 0.0;
    }
    let mut area = 0.0_f64;
    for window in coords.windows(2) {
        area += window[0].x * window[1].y - window[1].x * window[0].y;
    }
    (area * 0.5).abs()
}

fn primary_ring(geometry: &geojson::Geometry) -> Option<LineString<f64>> {
    match &geometry.value {
        Value::Polygon(rings) => rings.first().map(|ring| ring_from_coords(ring)),
        Value::MultiPolygon(polygons) => polygons
            .first()
            .and_then(|poly| poly.first())
            .map(|ring| ring_from_coords(ring)),
        _ => None,
    }
}

fn ring_from_coords(coords: &[Vec<f64>]) -> LineString<f64> {
    LineString::from(
        coords
            .iter()
            .filter_map(|point| {
                if point.len() >= 2 {
                    Some(Coord {
                        x: point[0],
                        y: point[1],
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>(),
    )
}
