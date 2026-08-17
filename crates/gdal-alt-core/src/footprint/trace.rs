use std::collections::HashMap;

/// Trace polygon rings from a row-major validity mask (`1` = valid, `0` = invalid).
pub fn trace_rings(valid: &[u8], width: usize, height: usize) -> Vec<Vec<(f64, f64)>> {
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let pw = width + 2;
    let mut padded = vec![0u8; pw * (height + 2)];
    for row in 0..height {
        for col in 0..width {
            if valid[row * width + col] != 0 {
                padded[(row + 1) * pw + (col + 1)] = 1;
            }
        }
    }

    let mut segments = Vec::new();
    for row in 0..height + 1 {
        for col in 0..width + 1 {
            let idx = row * pw + col;
            let a = padded[idx];
            let b = padded[idx + 1];
            let c = padded[idx + pw];
            let d = padded[idx + pw + 1];
            let case = resolve_case(a, b, c, d);
            for &(e0, e1) in marching_squares_edges(case) {
                let p0 = edge_point(e0, col, row);
                let p1 = edge_point(e1, col, row);
                segments.push((p0, p1));
            }
        }
    }

    chain_segments(segments)
        .into_iter()
        .fold(Vec::new(), |mut unique, ring| {
            let signature = ring_signature(&ring);
            if !unique.iter().any(|(sig, _)| *sig == signature) {
                unique.push((signature, ring));
            }
            unique
        })
        .into_iter()
        .map(|(_, ring)| ring)
        .collect()
}

fn ring_area(ring: &[(f64, f64)]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut area = 0.0;
    for index in 0..ring.len() {
        let (x0, y0) = ring[index];
        let (x1, y1) = ring[(index + 1) % ring.len()];
        area += x0 * y1 - x1 * y0;
    }
    area.abs() * 0.5
}

fn ring_signature(ring: &[(f64, f64)]) -> (usize, i64) {
    (
        ring.len(),
        (ring_area(ring) * 1_000_000.0).round() as i64,
    )
}

fn resolve_case(a: u8, b: u8, c: u8, d: u8) -> u8 {
    let mut case = (a << 3) | (b << 2) | (d << 1) | c;
    if case == 5 || case == 10 {
        let center = (a as u16 + b as u16 + c as u16 + d as u16) >= 2;
        case = match (case, center) {
            (5, true) => 6,
            (5, false) => 3,
            (10, true) => 9,
            (10, false) => 12,
            _ => case,
        };
    }
    case
}

fn edge_point(edge: u8, col: usize, row: usize) -> (f64, f64) {
    match edge {
        0 => (col as f64 + 0.5, row as f64),
        1 => (col as f64 + 1.0, row as f64 + 0.5),
        2 => (col as f64 + 0.5, row as f64 + 1.0),
        3 => (col as f64, row as f64 + 0.5),
        _ => unreachable!("invalid marching-squares edge"),
    }
}

fn marching_squares_edges(case: u8) -> &'static [(u8, u8)] {
    match case {
        0 | 15 => &[],
        1 => &[(3, 2)],
        2 => &[(2, 1)],
        3 => &[(3, 1)],
        4 => &[(0, 1)],
        5 => &[(0, 3), (2, 1)],
        6 => &[(0, 2)],
        7 => &[(0, 3)],
        8 => &[(3, 0)],
        9 => &[(0, 2)],
        10 => &[(0, 1), (3, 2)],
        11 => &[(0, 1)],
        12 => &[(3, 1)],
        13 => &[(2, 1)],
        14 => &[(3, 2)],
        _ => &[],
    }
}

fn quantize(point: (f64, f64)) -> (i64, i64) {
    ((point.0 * 2.0).round() as i64, (point.1 * 2.0).round() as i64)
}

fn dequantize(key: (i64, i64)) -> (f64, f64) {
    (key.0 as f64 / 2.0, key.1 as f64 / 2.0)
}

fn chain_segments(mut segments: Vec<((f64, f64), (f64, f64))>) -> Vec<Vec<(f64, f64)>> {
    let mut adjacency: HashMap<(i64, i64), Vec<(f64, f64)>> = HashMap::new();
    for (p0, p1) in segments.drain(..) {
        adjacency.entry(quantize(p0)).or_default().push(p1);
        adjacency.entry(quantize(p1)).or_default().push(p0);
    }

    let mut rings = Vec::new();
    while let Some(start_key) = adjacency
        .iter()
        .find(|(_, neighbors)| !neighbors.is_empty())
        .map(|(key, _)| *key)
    {
        let mut ring = vec![dequantize(start_key)];
        let mut current = start_key;
        let mut prev: Option<(i64, i64)> = None;

        loop {
            let neighbors = adjacency.get_mut(&current).expect("segment endpoint missing");
            let next_idx = neighbors
                .iter()
                .position(|point| prev.map(|p| quantize(*point) != p).unwrap_or(true))
                .expect("open contour chain");
            let next_point = neighbors.swap_remove(next_idx);
            let next_key = quantize(next_point);
            if next_key == start_key {
                break;
            }
            ring.push(next_point);
            prev = Some(current);
            current = next_key;
        }

        adjacency.retain(|_, neighbors| !neighbors.is_empty());
        if ring.len() >= 3 {
            rings.push(ring);
        }
    }

    rings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_full_block_is_rectangle() {
        let valid = vec![1u8; 4 * 4];
        let rings = trace_rings(&valid, 4, 4);
        assert_eq!(rings.len(), 1);
        assert!(rings[0].len() >= 4);
    }

    #[test]
    fn trace_hollow_center_produces_ring() {
        let mut valid = vec![1u8; 6 * 6];
        for row in 2..4 {
            for col in 2..4 {
                valid[row * 6 + col] = 0;
            }
        }
        let rings = trace_rings(&valid, 6, 6);
        assert!(!rings.is_empty());
    }
}
