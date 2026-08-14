use geotiff_reader::GeoTiffFile;
use tiff_core::{ExtraSample, PhotometricInterpretation};

use crate::input::{RasterProfile, SampleLayout};

/// How transparency should be represented in the output COG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransparencySource {
    None,
    DatasetMask,
    AssociatedAlpha,
    BlackRgb,
}

pub fn detect_transparency(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    mask_from_alpha: bool,
    black_rgb_transparent: bool,
) -> TransparencySource {
    if crate::cog::mask::discover_dataset_masks(input).is_some() {
        return TransparencySource::DatasetMask;
    }
    if mask_from_alpha {
        if associated_alpha_band_index(profile).is_some() {
            return TransparencySource::AssociatedAlpha;
        }
    }
    if black_rgb_transparent && black_rgb_candidate(profile) {
        return TransparencySource::BlackRgb;
    }
    TransparencySource::None
}

/// 1-based band index of the associated/unassociated alpha channel, if present.
pub fn associated_alpha_band_index(profile: &RasterProfile) -> Option<usize> {
    if profile.extra_samples.is_empty() {
        return None;
    }
    let alpha_offset = profile
        .extra_samples
        .iter()
        .position(|sample| {
            matches!(
                sample,
                ExtraSample::AssociatedAlpha | ExtraSample::UnassociatedAlpha
            )
        })?;
    let color_bands = match profile.photometric {
        PhotometricInterpretation::Rgb => 3,
        PhotometricInterpretation::MinIsBlack | PhotometricInterpretation::Palette => 1,
        _ => profile.bands.saturating_sub(profile.extra_samples.len() as u32) as usize,
    };
    Some(color_bands + alpha_offset + 1)
}

fn black_rgb_candidate(profile: &RasterProfile) -> bool {
    profile.bands == 3
        && profile.photometric == PhotometricInterpretation::Rgb
        && profile.extra_samples.is_empty()
        && profile.sample.bits_per_sample == 8
        && profile.sample.sample_format == tiff_core::SampleFormat::Uint
}

pub fn parse_nodata<T>(_layout: &SampleLayout, nodata: &Option<String>) -> Option<T>
where
    T: geotiff_writer::NumericSample + PartialEq,
{
    let text = nodata.as_ref()?;
    let trimmed = text.trim();
    if let Some(value) = T::parse_exact(trimmed) {
        return Some(value);
    }
    let value = trimmed.parse::<f64>().ok()?;
    T::try_from_f64(value)
}
