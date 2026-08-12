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
    loop {
        let w = width / factor;
        let h = height / factor;
        if w < blocksize && h < blocksize {
            break;
        }
        levels.push(factor);
        factor = factor.saturating_mul(2);
        if factor > 1024 {
            break;
        }
    }
    levels
}

pub fn overview_sizes(width: u32, height: u32, levels: &[u32]) -> Vec<(usize, usize)> {
    levels
        .iter()
        .map(|&level| {
            let level = level as usize;
            (
                (width as usize).div_ceil(level),
                (height as usize).div_ceil(level),
            )
        })
        .collect()
}
