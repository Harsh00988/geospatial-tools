use anyhow::{bail, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{remux_compress_tile, RemuxCompressedBlock, RemuxLayer, RemuxMaskDescriptor, RemuxTileEncoding};
use ndarray::{Array2, Array3};
use rayon::prelude::*;
use tiff_core::{Predictor, TAG_NEW_SUBFILE_TYPE, TAG_SUBFILE_TYPE};
use tiff_reader::{Ifd, TagValue};

use super::options::CogOutputOptions;
use super::semantics::{
    associated_alpha_band_index, detect_transparency, TransparencySource,
};
use super::tile_payload::{input_compression, read_layer_blocks};
use crate::cog::{tile_jobs, TileJob};
use crate::crop::{scale_window, WriteWindow};
use crate::input::RasterProfile;

const MASK_FLAG: u64 = 0x4;
const REDUCED_RESOLUTION_FLAG: u64 = 0x1;
const PHOTOMETRIC_MASK: u16 = 4;

#[derive(Debug, Clone)]
pub struct DatasetMasks {
    pub base_ifd_index: usize,
    pub overview_ifd_indices: Vec<usize>,
}

pub fn discover_dataset_masks(input: &GeoTiffFile) -> Option<DatasetMasks> {
    let tiff = input.tiff();
    let base_index = input.base_ifd_index();
    let base = tiff.ifd(base_index).ok()?;
    let base_w = base.width();
    let base_h = base.height();

    let mut base_mask_index = None;
    let mut overview_masks = Vec::new();

    for (index, ifd) in tiff.ifds().iter().enumerate() {
        if index == base_index || !is_transparency_mask_ifd(ifd) {
            continue;
        }

        if has_reduced_resolution(ifd) {
            overview_masks.push((index, ifd.width(), ifd.height()));
        } else if ifd.width() == base_w && ifd.height() == base_h {
            if base_mask_index.is_some() {
                return None;
            }
            base_mask_index = Some(index);
        }
    }

    let base_mask_index = base_mask_index?;
    if input.overview_count() == 0 {
        return Some(DatasetMasks {
            base_ifd_index: base_mask_index,
            overview_ifd_indices: Vec::new(),
        });
    }

    if overview_masks.len() != input.overview_count() {
        return None;
    }

    let mut ordered = Vec::with_capacity(input.overview_count());
    for index in 0..input.overview_count() {
        let rgb = input.overview_ifd(index).ok()?;
        let pos = overview_masks
            .iter()
            .position(|(_, w, h)| *w == rgb.width() && *h == rgb.height())?;
        ordered.push(overview_masks.remove(pos).0);
    }

    Some(DatasetMasks {
        base_ifd_index: base_mask_index,
        overview_ifd_indices: ordered,
    })
}

pub fn collect_mask_remux_layers(
    input: &GeoTiffFile,
    masks: &DatasetMasks,
) -> Result<Vec<Vec<RemuxCompressedBlock>>> {
    let tiff = input.tiff();
    let indices = std::iter::once(masks.base_ifd_index)
        .chain(masks.overview_ifd_indices.iter().copied());
    indices
        .map(|index| {
            let ifd = tiff
                .ifd(index)
                .map_err(|err| anyhow::anyhow!("mask IFD {index}: {err}"))?;
            read_layer_blocks(tiff, ifd)
        })
        .collect()
}

pub fn mask_layer_descriptors(
    input: &GeoTiffFile,
    masks: &DatasetMasks,
) -> Result<Vec<RemuxMaskDescriptor>> {
    let tiff = input.tiff();
    let indices = std::iter::once(masks.base_ifd_index)
        .chain(masks.overview_ifd_indices.iter().copied());
    indices
        .enumerate()
        .map(|(layer_index, ifd_index)| {
            let ifd = tiff
                .ifd(ifd_index)
                .map_err(|err| anyhow::anyhow!("mask IFD {ifd_index}: {err}"))?;
            mask_descriptor_from_ifd(ifd, layer_index > 0)
        })
        .collect()
}

pub fn interleave_rgb_and_mask_layers(
    rgb_layers: Vec<Vec<RemuxCompressedBlock>>,
    mask_blocks: Vec<Vec<RemuxCompressedBlock>>,
    mask_descriptors: Vec<RemuxMaskDescriptor>,
) -> Result<Vec<RemuxLayer>> {
    if mask_blocks.len() != mask_descriptors.len() {
        bail!("mask block/descriptor count mismatch");
    }
    if rgb_layers.is_empty() || mask_blocks.is_empty() {
        bail!("cannot interleave empty rgb/mask layers");
    }
    if mask_blocks.len() != 1 && mask_blocks.len() - 1 != rgb_layers.len() - 1 {
        bail!("unexpected mask layer count for rgb layers");
    }

    let mut layers =
        Vec::with_capacity(rgb_layers.len() + mask_blocks.len());
    layers.push(RemuxLayer::rgb(rgb_layers[0].clone()));
    layers.push(RemuxLayer::mask(
        mask_descriptors[0].clone(),
        mask_blocks[0].clone(),
    ));
    for rgb in rgb_layers.into_iter().skip(1) {
        layers.push(RemuxLayer::rgb(rgb));
    }
    for (blocks, descriptor) in mask_blocks
        .into_iter()
        .skip(1)
        .zip(mask_descriptors.into_iter().skip(1))
    {
        layers.push(RemuxLayer::mask(descriptor, blocks));
    }
    Ok(layers)
}

pub fn prepare_remux_layers(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    rgb_layers: Vec<Vec<RemuxCompressedBlock>>,
    window: Option<&WriteWindow>,
    out_width: u32,
    out_height: u32,
    overview_levels: &[u32],
    opts: &CogOutputOptions,
) -> Result<(Vec<RemuxLayer>, bool)> {
    if let Some(masks) = discover_dataset_masks(input) {
        let (mask_blocks, mask_descriptors) = if let Some(window) = window {
            build_cropped_mask_layers(
                input,
                &masks,
                window,
                out_width,
                out_height,
                overview_levels,
                opts,
            )?
        } else {
            let blocks = collect_mask_remux_layers(input, &masks)?;
            let descriptors = mask_layer_descriptors(input, &masks)?;
            align_masks_to_rgb_layers(blocks, &descriptors, rgb_layers.len())?
        };
        let layers = interleave_rgb_and_mask_layers(rgb_layers, mask_blocks, mask_descriptors)?;
        return Ok((layers, true));
    }

    let transparency = detect_transparency(
        input,
        profile,
        opts.mask_from_alpha,
        opts.black_rgb_transparent,
    );
    if matches!(
        transparency,
        TransparencySource::AssociatedAlpha | TransparencySource::BlackRgb
    ) {
        if let Some((mask_blocks, mask_descriptors)) = build_synthesized_mask_layers(
            input,
            profile,
            transparency,
            window,
            out_width,
            out_height,
            overview_levels,
            opts,
        )? {
            let layers =
                interleave_rgb_and_mask_layers(rgb_layers, mask_blocks, mask_descriptors)?;
            return Ok((layers, true));
        }
    }

    Ok((
        rgb_layers.into_iter().map(RemuxLayer::rgb).collect(),
        false,
    ))
}

/// When RGB overview count differs from source mask overview count, keep masks that
/// match the output RGB layer dimensions.
fn align_masks_to_rgb_layers(
    mask_blocks: Vec<Vec<RemuxCompressedBlock>>,
    mask_descriptors: &[RemuxMaskDescriptor],
    rgb_layer_count: usize,
) -> Result<(Vec<Vec<RemuxCompressedBlock>>, Vec<RemuxMaskDescriptor>)> {
    if mask_blocks.len() != mask_descriptors.len() {
        bail!("mask block/descriptor count mismatch");
    }
    let expected_masks = if rgb_layer_count == 0 {
        0
    } else {
        1 + rgb_layer_count - 1
    };
    if mask_blocks.len() < expected_masks {
        bail!(
            "source has {} mask layers but output needs {}",
            mask_blocks.len(),
            expected_masks
        );
    }
    if mask_blocks.len() == expected_masks {
        return Ok((mask_blocks, mask_descriptors.to_vec()));
    }
    Ok((
        mask_blocks.into_iter().take(expected_masks).collect(),
        mask_descriptors.iter().take(expected_masks).cloned().collect(),
    ))
}

pub fn build_cropped_mask_layers(
    input: &GeoTiffFile,
    masks: &DatasetMasks,
    window: &WriteWindow,
    out_width: u32,
    out_height: u32,
    output_levels: &[u32],
    opts: &CogOutputOptions,
) -> Result<(Vec<Vec<RemuxCompressedBlock>>, Vec<RemuxMaskDescriptor>)> {
    let tiff = input.tiff();
    let tile_size = opts.blocksize as usize;
    let encoding = mask_tile_encoding(opts, tile_size);

    let base_ifd = tiff.ifd(masks.base_ifd_index)?;
    let base_blocks = encode_cropped_mask_layer(
        tiff,
        base_ifd,
        window,
        out_width,
        out_height,
        tile_size,
        encoding,
    )?;
    let base_descriptor = cropped_mask_descriptor(
        base_ifd,
        out_width,
        out_height,
        tile_size,
        opts.blocksize,
        false,
    );

    let mut blocks = vec![base_blocks];
    let mut descriptors = vec![base_descriptor];

    for (index, &level) in output_levels.iter().enumerate() {
        let ifd_index = masks
            .overview_ifd_indices
            .get(index)
            .copied()
            .unwrap_or(masks.base_ifd_index);
        let ifd = tiff.ifd(ifd_index)?;
        let scaled = scale_window(window, level);
        let layer_w = (out_width / level).max(1);
        let layer_h = (out_height / level).max(1);
        let layer_tile = if index < masks.overview_ifd_indices.len() {
            ifd.tile_width().unwrap_or(opts.blocksize)
        } else {
            opts.blocksize
        };
        let layer_tile_size = layer_tile as usize;
        let layer_encoding = mask_tile_encoding(opts, layer_tile_size);
        blocks.push(encode_cropped_mask_layer(
            tiff,
            ifd,
            &scaled,
            layer_w,
            layer_h,
            layer_tile_size,
            layer_encoding,
        )?);
        descriptors.push(cropped_mask_descriptor(
            ifd,
            layer_w,
            layer_h,
            layer_tile_size,
            layer_tile,
            true,
        ));
    }

    Ok((blocks, descriptors))
}

fn encode_cropped_mask_layer(
    tiff: &tiff_reader::TiffFile,
    ifd: &Ifd,
    src_win: &WriteWindow,
    out_width: u32,
    out_height: u32,
    tile_size: usize,
    encoding: RemuxTileEncoding,
) -> Result<Vec<RemuxCompressedBlock>> {
    let jobs = tile_jobs(out_width, out_height, tile_size as u32);
    let mut blocks = jobs
        .par_iter()
        .enumerate()
        .map(|(block_index, job)| {
            let tile = read_mask_tile(tiff, ifd, src_win, job)?;
            let padded = pad_mask_tile(&tile, job.rows, job.cols, tile_size);
            let block = remux_compress_tile(&padded, block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            Ok((block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;
    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

fn read_mask_tile(
    tiff: &tiff_reader::TiffFile,
    ifd: &Ifd,
    src_win: &WriteWindow,
    job: &TileJob,
) -> Result<Vec<u8>> {
    let src_col = src_win.col_off + job.col_off;
    let src_row = src_win.row_off + job.row_off;
    let data = tiff.read_window_from_ifd::<u8>(ifd, src_row, src_col, job.rows, job.cols)?;
    let array = data
        .into_dimensionality::<ndarray::Ix2>()
        .map_err(|err| anyhow::anyhow!("mask window must be 2D: {err}"))?;
    Ok(binarize_mask_array(&array))
}

fn binarize_mask_array(array: &Array2<u8>) -> Vec<u8> {
    array
        .iter()
        .map(|&value| if value != 0 { 255 } else { 0 })
        .collect()
}

fn pad_mask_tile(samples: &[u8], rows: usize, cols: usize, tile_size: usize) -> Vec<u8> {
    if rows == tile_size && cols == tile_size {
        return samples.to_vec();
    }
    let mut padded = vec![0u8; tile_size * tile_size];
    for row in 0..rows {
        let src_start = row * cols;
        let dst_start = row * tile_size;
        padded[dst_start..dst_start + cols].copy_from_slice(&samples[src_start..src_start + cols]);
    }
    padded
}

fn mask_tile_encoding(opts: &CogOutputOptions, tile_size: usize) -> RemuxTileEncoding {
    crate::cog::tile_encoding_from_opts(opts, tile_size, 1, Some(Predictor::None))
}

fn cropped_mask_descriptor(
    source_ifd: &Ifd,
    width: u32,
    height: u32,
    _tile_size: usize,
    tile_u32: u32,
    overview: bool,
) -> RemuxMaskDescriptor {
    RemuxMaskDescriptor {
        width,
        height,
        tile_width: tile_u32,
        tile_height: tile_u32,
        bits_per_sample: 8,
        compression: input_compression(source_ifd),
        overview,
    }
}

fn mask_descriptor_from_ifd(ifd: &Ifd, overview: bool) -> Result<RemuxMaskDescriptor> {
    let (tile_w, tile_h) = match (ifd.tile_width(), ifd.tile_height()) {
        (Some(w), Some(h)) => (w, h),
        _ => bail!("dataset mask IFD must be tiled"),
    };
    Ok(RemuxMaskDescriptor {
        width: ifd.width(),
        height: ifd.height(),
        tile_width: tile_w,
        tile_height: tile_h,
        bits_per_sample: ifd.bits_per_sample().unwrap_or_else(|_| vec![1]).first().copied().unwrap_or(1),
        compression: input_compression(ifd),
        overview,
    })
}

fn build_synthesized_mask_layers(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    source: TransparencySource,
    window: Option<&WriteWindow>,
    out_width: u32,
    out_height: u32,
    overview_levels: &[u32],
    opts: &CogOutputOptions,
) -> Result<Option<(Vec<Vec<RemuxCompressedBlock>>, Vec<RemuxMaskDescriptor>)>> {
    let tile_size = opts.blocksize as usize;
    let encoding = mask_tile_encoding(opts, tile_size);
    let base_ifd = input.tiff().ifd(input.base_ifd_index())?;

    let base_blocks = encode_synthesized_mask_layer(
        input,
        profile,
        source,
        window,
        out_width,
        out_height,
        tile_size,
        encoding,
    )?;
    let mut blocks = vec![base_blocks];
    let mut descriptors = vec![synthesized_mask_descriptor(
        out_width,
        out_height,
        opts.blocksize,
        false,
        base_ifd,
    )];

    for (index, &level) in overview_levels.iter().enumerate() {
        let scaled = window.map(|w| scale_window(w, level));
        let layer_w = (out_width / level).max(1);
        let layer_h = (out_height / level).max(1);
        blocks.push(encode_synthesized_mask_layer(
            input,
            profile,
            source,
            scaled.as_ref(),
            layer_w,
            layer_h,
            tile_size,
            encoding,
        )?);
        descriptors.push(synthesized_mask_descriptor(
            layer_w,
            layer_h,
            opts.blocksize,
            true,
            base_ifd,
        ));
        let _ = index;
    }

    Ok(Some((blocks, descriptors)))
}

fn synthesized_mask_descriptor(
    width: u32,
    height: u32,
    tile_size: u32,
    overview: bool,
    source_ifd: &Ifd,
) -> RemuxMaskDescriptor {
    RemuxMaskDescriptor {
        width,
        height,
        tile_width: tile_size,
        tile_height: tile_size,
        bits_per_sample: 8,
        compression: input_compression(source_ifd),
        overview,
    }
}

fn encode_synthesized_mask_layer(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    source: TransparencySource,
    window: Option<&WriteWindow>,
    out_width: u32,
    out_height: u32,
    tile_size: usize,
    encoding: RemuxTileEncoding,
) -> Result<Vec<RemuxCompressedBlock>> {
    let jobs = tile_jobs(out_width, out_height, tile_size as u32);
    let mut blocks = jobs
        .par_iter()
        .enumerate()
        .map(|(block_index, job)| {
            let tile = read_synthesized_mask_tile(input, profile, source, window, job)?;
            let padded = pad_mask_tile(&tile, job.rows, job.cols, tile_size);
            let block = remux_compress_tile(&padded, block_index, encoding)
                .map_err(|err| anyhow::anyhow!(err))?;
            Ok((block_index, block))
        })
        .collect::<Result<Vec<_>>>()?;
    blocks.sort_by_key(|(index, _)| *index);
    Ok(blocks.into_iter().map(|(_, block)| block).collect())
}

fn read_synthesized_mask_tile(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    source: TransparencySource,
    window: Option<&WriteWindow>,
    job: &TileJob,
) -> Result<Vec<u8>> {
    let src_col = window.map(|w| w.col_off + job.col_off).unwrap_or(job.col_off);
    let src_row = window.map(|w| w.row_off + job.row_off).unwrap_or(job.row_off);

    match source {
        TransparencySource::AssociatedAlpha => {
            let band = associated_alpha_band_index(profile)
                .ok_or_else(|| anyhow::anyhow!("missing associated alpha band"))?;
            let band_index = band - 1;
            if profile.sample.bits_per_sample <= 8 {
                let data = input.read_band_window::<u8>(
                    band_index,
                    src_row,
                    src_col,
                    job.rows,
                    job.cols,
                )?;
                let array = data
                    .into_dimensionality::<ndarray::Ix2>()
                    .map_err(|err| anyhow::anyhow!("alpha window must be 2D: {err}"))?;
                Ok(alpha_to_mask_array(&array))
            } else {
                let data = input.read_band_window::<u16>(
                    band_index,
                    src_row,
                    src_col,
                    job.rows,
                    job.cols,
                )?;
                let array = data
                    .into_dimensionality::<ndarray::Ix2>()
                    .map_err(|err| anyhow::anyhow!("alpha window must be 2D: {err}"))?;
                Ok(alpha_u16_to_mask_array(&array))
            }
        }
        TransparencySource::BlackRgb => {
            let data = input.read_window::<u8>(src_row, src_col, job.rows, job.cols)?;
            let array = data
                .into_dimensionality::<ndarray::Ix3>()
                .map_err(|err| anyhow::anyhow!("RGB window must be 3D: {err}"))?;
            Ok(black_rgb_to_mask_array(&array))
        }
        _ => bail!("unsupported synthesized mask source"),
    }
}

fn alpha_to_mask_array(array: &Array2<u8>) -> Vec<u8> {
    array
        .iter()
        .map(|&value| if value > 0 { 255 } else { 0 })
        .collect()
}

fn alpha_u16_to_mask_array(array: &Array2<u16>) -> Vec<u8> {
    array
        .iter()
        .map(|&value| if value > 0 { 255 } else { 0 })
        .collect()
}

fn black_rgb_to_mask_array(array: &Array3<u8>) -> Vec<u8> {
    let rows = array.shape()[0];
    let cols = array.shape()[1];
    let mut out = Vec::with_capacity(rows * cols);
    for row in 0..rows {
        for col in 0..cols {
            let valid = !(array[[row, col, 0]] == 0
                && array[[row, col, 1]] == 0
                && array[[row, col, 2]] == 0);
            out.push(if valid { 255 } else { 0 });
        }
    }
    out
}

pub fn validate_dataset_masks(input: &GeoTiffFile, issues: &mut Vec<crate::validate::ValidationIssue>) {
    use crate::validate::{issue, ValidationLevel};

    let tiff = input.tiff();
    let base = match tiff.ifd(input.base_ifd_index()) {
        Ok(ifd) => ifd,
        Err(err) => {
            issues.push(issue(
                ValidationLevel::Error,
                format!("failed to read base IFD for mask validation: {err}"),
            ));
            return;
        }
    };

    let mut base_mask = None;
    let mut overview_masks = Vec::new();
    for (index, ifd) in tiff.ifds().iter().enumerate() {
        if index == input.base_ifd_index() || !is_transparency_mask_ifd(ifd) {
            continue;
        }
        if has_reduced_resolution(ifd) {
            overview_masks.push((index, ifd.width(), ifd.height()));
        } else if ifd.width() == base.width() && ifd.height() == base.height() {
            if base_mask.is_some() {
                issues.push(issue(
                    ValidationLevel::Error,
                    "multiple full-resolution transparency mask IFDs found",
                ));
            }
            base_mask = Some(index);
        }
    }

    let Some(base_mask_index) = base_mask else {
        return;
    };

    let base_mask_ifd = &tiff.ifds()[base_mask_index];
    if !base_mask_ifd.is_tiled() {
        issues.push(issue(
            ValidationLevel::Error,
            "dataset transparency mask must be tiled",
        ));
    }

    if input.overview_count() > 0 && overview_masks.len() != input.overview_count() {
        issues.push(issue(
            ValidationLevel::Warning,
            format!(
                "found {} mask overview IFDs but {} RGB overview IFDs",
                overview_masks.len(),
                input.overview_count()
            ),
        ));
    }

    for index in 0..input.overview_count() {
        let rgb = match input.overview_ifd(index) {
            Ok(ifd) => ifd,
            Err(err) => {
                issues.push(issue(
                    ValidationLevel::Warning,
                    format!("overview {index} unavailable during mask validation: {err}"),
                ));
                continue;
            }
        };
        if !overview_masks
            .iter()
            .any(|(_, w, h)| *w == rgb.width() && *h == rgb.height())
        {
            issues.push(issue(
                ValidationLevel::Warning,
                format!(
                    "no mask overview matches RGB overview {index} ({}x{})",
                    rgb.width(),
                    rgb.height()
                ),
            ));
        }
    }
}

fn is_transparency_mask_ifd(ifd: &Ifd) -> bool {
    if new_subfile_type(ifd) & MASK_FLAG != 0 {
        return true;
    }
    if matches!(
        ifd.tag(TAG_SUBFILE_TYPE)
            .and_then(|tag| tag.value.as_u16()),
        Some(4)
    ) {
        return true;
    }

    ifd.samples_per_pixel() == 1
        && ifd.photometric_interpretation() == Some(PHOTOMETRIC_MASK)
}

fn new_subfile_type(ifd: &Ifd) -> u64 {
    let Some(tag) = ifd.tag(TAG_NEW_SUBFILE_TYPE) else {
        return 0;
    };
    match &tag.value {
        TagValue::Long(values) => values.first().copied().unwrap_or(0) as u64,
        TagValue::Short(values) => values.first().copied().unwrap_or(0) as u64,
        TagValue::Long8(values) => values.first().copied().unwrap_or(0),
        _ => 0,
    }
}

fn has_reduced_resolution(ifd: &Ifd) -> bool {
    if new_subfile_type(ifd) & REDUCED_RESOLUTION_FLAG != 0 {
        return true;
    }
    matches!(
        ifd.tag(TAG_SUBFILE_TYPE)
            .and_then(|tag| tag.value.as_u16()),
        Some(2)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open::open_geotiff;
    use std::path::Path;

    #[test]
    fn discovers_sn33_dataset_masks() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test_data/sn33_mask_fixture.tif");
        if !path.exists() {
            return;
        }
        let input = open_geotiff(&path, false).expect("open fixture");
        let masks = discover_dataset_masks(&input).expect("masks");
        assert_eq!(masks.overview_ifd_indices.len(), 4);
    }
}
