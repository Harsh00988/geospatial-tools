use anyhow::{bail, Result};
use geotiff_core::GeoTransform;
use geotiff_reader::GeoTiffFile;

#[derive(Debug, Clone, Copy)]
pub struct WriteWindow {
    pub col_off: usize,
    pub row_off: usize,
    pub width: usize,
    pub height: usize,
}

pub fn shift_transform(transform: &GeoTransform, col_off: usize, row_off: usize) -> GeoTransform {
    GeoTransform {
        origin_x: transform.origin_x
            + col_off as f64 * transform.pixel_width
            + row_off as f64 * transform.skew_x,
        origin_y: transform.origin_y
            + col_off as f64 * transform.skew_y
            + row_off as f64 * transform.pixel_height,
        pixel_width: transform.pixel_width,
        skew_x: transform.skew_x,
        skew_y: transform.skew_y,
        pixel_height: transform.pixel_height,
    }
}

pub fn window_from_srcwin(
    image_width: u32,
    image_height: u32,
    col: usize,
    row: usize,
    width: usize,
    height: usize,
) -> Result<WriteWindow> {
    if col >= image_width as usize || row >= image_height as usize {
        bail!("srcwin origin ({col}, {row}) is outside image bounds");
    }
    let max_w = image_width as usize - col;
    let max_h = image_height as usize - row;
    if width == 0 || height == 0 {
        bail!("srcwin width and height must be positive");
    }
    if width > max_w || height > max_h {
        bail!(
            "srcwin {width}x{height} at ({col}, {row}) exceeds image bounds ({image_width}x{image_height})"
        );
    }
    Ok(WriteWindow {
        col_off: col,
        row_off: row,
        width,
        height,
    })
}

/// Scale a pixel window down for overview-level reads.
pub(crate) fn scale_window(window: &WriteWindow, scale: u32) -> WriteWindow {
    let scale = scale.max(1) as usize;
    WriteWindow {
        col_off: window.col_off / scale,
        row_off: window.row_off / scale,
        width: window.width / scale,
        height: window.height / scale,
    }
}

pub fn window_from_projwin(
    input: &GeoTiffFile,
    ulx: f64,
    uly: f64,
    lrx: f64,
    lry: f64,
) -> Result<WriteWindow> {
    let transform = input
        .transform()
        .ok_or_else(|| anyhow::anyhow!("input has no georeferencing for -projwin"))?;
    let width = input.width();
    let height = input.height();

    let (min_x, max_x) = if ulx <= lrx { (ulx, lrx) } else { (lrx, ulx) };
    let (min_y, max_y) = if uly <= lry { (uly, lry) } else { (lry, uly) };

    let (col0, row0) = transform
        .geo_to_pixel(min_x, max_y)
        .ok_or_else(|| anyhow::anyhow!("failed to map upper-left corner to pixels"))?;
    let (col1, row1) = transform
        .geo_to_pixel(max_x, min_y)
        .ok_or_else(|| anyhow::anyhow!("failed to map lower-right corner to pixels"))?;

    let col_off = col0.floor().max(0.0) as usize;
    let row_off = row0.floor().max(0.0) as usize;
    let col_end = col1.ceil().min(width as f64) as usize;
    let row_end = row1.ceil().min(height as f64) as usize;

    if col_end <= col_off || row_end <= row_off {
        bail!("projwin does not intersect the raster extent");
    }

    Ok(WriteWindow {
        col_off,
        row_off,
        width: col_end - col_off,
        height: row_end - row_off,
    })
}
