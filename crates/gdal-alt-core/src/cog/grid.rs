#[derive(Clone, Copy, Debug)]
pub struct TileJob {
    pub col_off: usize,
    pub row_off: usize,
    pub cols: usize,
    pub rows: usize,
}

pub fn tile_jobs(width: u32, height: u32, tile_size: u32) -> Vec<TileJob> {
    let tile = tile_size as usize;
    let w = width as usize;
    let h = height as usize;
    let mut jobs = Vec::new();

    let mut row_off = 0;
    while row_off < h {
        let rows = (h - row_off).min(tile);
        let mut col_off = 0;
        while col_off < w {
            let cols = (w - col_off).min(tile);
            jobs.push(TileJob {
                col_off,
                row_off,
                cols,
                rows,
            });
            col_off += tile;
        }
        row_off += tile;
    }

    jobs
}

pub fn auto_overview_levels(width: u32, height: u32, blocksize: u32) -> Vec<u32> {
    let mut levels = Vec::new();
    let mut factor = 2u32;
    let mut w = width;
    let mut h = height;
    while w.max(h) > blocksize {
        w /= 2;
        h /= 2;
        levels.push(factor);
        factor = factor.saturating_mul(2);
        if factor > 1024 {
            break;
        }
    }
    levels
}

pub fn overview_dimensions(width: u32, height: u32, level: u32) -> (u32, u32) {
    ((width / level).max(1), (height / level).max(1))
}

pub fn overview_sizes(width: u32, height: u32, levels: &[u32]) -> Vec<(usize, usize)> {
    levels
        .iter()
        .map(|&level| {
            let (w, h) = overview_dimensions(width, height, level);
            (w as usize, h as usize)
        })
        .collect()
}
