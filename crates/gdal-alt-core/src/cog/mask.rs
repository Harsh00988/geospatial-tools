use anyhow::{bail, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{RemuxCompressedBlock, RemuxLayer, RemuxMaskDescriptor};
use tiff_core::{TAG_NEW_SUBFILE_TYPE, TAG_SUBFILE_TYPE};
use tiff_reader::{Ifd, TagValue};

use super::tile_payload::{input_compression, read_layer_blocks};

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
    rgb_layers: Vec<Vec<RemuxCompressedBlock>>,
) -> Result<(Vec<RemuxLayer>, bool)> {
    let Some(masks) = discover_dataset_masks(input) else {
        return Ok((
            rgb_layers.into_iter().map(RemuxLayer::rgb).collect(),
            false,
        ));
    };
    let mask_blocks = collect_mask_remux_layers(input, &masks)?;
    let mask_descriptors = mask_layer_descriptors(input, &masks)?;
    let layers = interleave_rgb_and_mask_layers(rgb_layers, mask_blocks, mask_descriptors)?;
    Ok((layers, true))
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
        compression: input_compression(ifd),
        overview,
    })
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
