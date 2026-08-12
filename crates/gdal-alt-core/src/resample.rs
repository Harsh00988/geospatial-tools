use ndarray::{Array2, Array3, s};

use crate::cog::ResamplingChoice;

pub fn downsample_2d<T>(
    src: &Array2<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
    method: ResamplingChoice,
    nodata: Option<T>,
) -> Array2<T>
where
    T: geotiff_writer::NumericSample + Copy + Default + PartialEq,
{
    match method {
        ResamplingChoice::Nearest => nearest_downsample_2d(src, out_rows, out_cols, scale, nodata),
        ResamplingChoice::Average => average_downsample_2d(src, out_rows, out_cols, scale, nodata),
        ResamplingChoice::Bilinear => bilinear_downsample_2d(src, out_rows, out_cols, scale, nodata),
        ResamplingChoice::Cubic | ResamplingChoice::Lanczos => {
            cubic_downsample_2d(src, out_rows, out_cols, scale, nodata)
        }
    }
}

pub fn downsample_3d<T>(
    src: &Array3<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
    method: ResamplingChoice,
    nodata: Option<T>,
) -> Array3<T>
where
    T: geotiff_writer::NumericSample + Copy + Default + PartialEq,
{
    let bands = src.shape()[2];
    let mut out = Array3::default((out_rows, out_cols, bands));
    for band in 0..bands {
        let plane = src.slice(s![.., .., band]).to_owned();
        let down = downsample_2d(&plane, out_rows, out_cols, scale, method, nodata);
        out.slice_mut(s![.., .., band]).assign(&down);
    }
    out
}

fn is_nodata<T: PartialEq>(value: T, nodata: Option<T>) -> bool {
    nodata.is_some_and(|nd| value == nd)
}

fn nearest_downsample_2d<T: Copy + Default + PartialEq>(
    src: &Array2<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
    nodata: Option<T>,
) -> Array2<T> {
    let mut out = Array2::default((out_rows, out_cols));
    let src_rows = src.shape()[0];
    let src_cols = src.shape()[1];
    for row in 0..out_rows {
        for col in 0..out_cols {
            let src_row = (row * scale).min(src_rows.saturating_sub(1));
            let src_col = (col * scale).min(src_cols.saturating_sub(1));
            let value = src[[src_row, src_col]];
            out[[row, col]] = if is_nodata(value, nodata) {
                nodata.unwrap_or_default()
            } else {
                value
            };
        }
    }
    out
}

fn average_downsample_2d<T>(src: &Array2<T>, out_rows: usize, out_cols: usize, scale: usize, nodata: Option<T>) -> Array2<T>
where
    T: geotiff_writer::NumericSample + Copy + Default + PartialEq,
{
    let mut out = Array2::default((out_rows, out_cols));
    for row in 0..out_rows {
        for col in 0..out_cols {
            let mut sum = 0.0;
            let mut count = 0usize;
            for dy in 0..scale {
                for dx in 0..scale {
                    let src_row = row * scale + dy;
                    let src_col = col * scale + dx;
                    if src_row < src.shape()[0] && src_col < src.shape()[1] {
                        let value = src[[src_row, src_col]];
                        if is_nodata(value, nodata) {
                            continue;
                        }
                        sum += value.to_f64();
                        count += 1;
                    }
                }
            }
            out[[row, col]] = if count == 0 {
                nodata.unwrap_or_default()
            } else {
                T::from_f64(sum / count as f64)
            };
        }
    }
    out
}

fn bilinear_downsample_2d<T>(
    src: &Array2<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
    nodata: Option<T>,
) -> Array2<T>
where
    T: geotiff_writer::NumericSample + Copy + Default + PartialEq,
{
    let mut out = Array2::default((out_rows, out_cols));
    let src_rows = src.shape()[0];
    let src_cols = src.shape()[1];
    for row in 0..out_rows {
        for col in 0..out_cols {
            let center_row = ((row * scale) as f64 + (scale as f64 - 1.0) * 0.5).min((src_rows - 1) as f64);
            let center_col = ((col * scale) as f64 + (scale as f64 - 1.0) * 0.5).min((src_cols - 1) as f64);
            let row0 = center_row.floor() as usize;
            let col0 = center_col.floor() as usize;
            let row1 = (row0 + 1).min(src_rows - 1);
            let col1 = (col0 + 1).min(src_cols - 1);
            let dy = center_row - row0 as f64;
            let dx = center_col - col0 as f64;
            let samples = [
                (src[[row0, col0]], (1.0 - dx) * (1.0 - dy)),
                (src[[row0, col1]], dx * (1.0 - dy)),
                (src[[row1, col0]], (1.0 - dx) * dy),
                (src[[row1, col1]], dx * dy),
            ];
            let mut sum = 0.0;
            let mut weight = 0.0;
            for (value, w) in samples {
                if is_nodata(value, nodata) {
                    continue;
                }
                sum += value.to_f64() * w;
                weight += w;
            }
            out[[row, col]] = if weight == 0.0 {
                nodata.unwrap_or_default()
            } else {
                T::from_f64(sum / weight)
            };
        }
    }
    out
}

fn cubic_downsample_2d<T>(
    src: &Array2<T>,
    out_rows: usize,
    out_cols: usize,
    scale: usize,
    nodata: Option<T>,
) -> Array2<T>
where
    T: geotiff_writer::NumericSample + Copy + Default + PartialEq,
{
    let mut out = Array2::default((out_rows, out_cols));
    let src_rows = src.shape()[0];
    let src_cols = src.shape()[1];
    for row in 0..out_rows {
        for col in 0..out_cols {
            let center_row = ((row * scale) as f64 + (scale as f64 - 1.0) * 0.5).min((src_rows - 1) as f64);
            let center_col = ((col * scale) as f64 + (scale as f64 - 1.0) * 0.5).min((src_cols - 1) as f64);
            let value = cubic_sample_2d(src, center_row, center_col, src_rows, src_cols, nodata);
            out[[row, col]] = if value.is_nan() {
                nodata.unwrap_or_default()
            } else {
                T::from_f64(value)
            };
        }
    }
    out
}

fn cubic_sample_2d<T>(
    src: &Array2<T>,
    row: f64,
    col: f64,
    src_rows: usize,
    src_cols: usize,
    nodata: Option<T>,
) -> f64
where
    T: geotiff_writer::NumericSample + Copy + PartialEq,
{
    let row_i = row.floor() as isize - 1;
    let col_i = col.floor() as isize - 1;
    let mut acc = 0.0;
    let mut weight = 0.0;
    for dy in 0..4 {
        for dx in 0..4 {
            let sy = (row_i + dy as isize).clamp(0, src_rows as isize - 1) as usize;
            let sx = (col_i + dx as isize).clamp(0, src_cols as isize - 1) as usize;
            let value = src[[sy, sx]];
            if is_nodata(value, nodata) {
                continue;
            }
            let wy = cubic_kernel(row - sy as f64);
            let wx = cubic_kernel(col - sx as f64);
            let w = wy * wx;
            acc += value.to_f64() * w;
            weight += w;
        }
    }
    if weight == 0.0 {
        f64::NAN
    } else {
        acc / weight
    }
}

fn cubic_kernel(t: f64) -> f64 {
    let x = t.abs();
    if x <= 1.0 {
        1.5 * x.powi(3) - 2.5 * x.powi(2) + 1.0
    } else if x < 2.0 {
        -0.5 * x.powi(3) + 2.5 * x.powi(2) - 4.0 * x + 2.0
    } else {
        0.0
    }
}
