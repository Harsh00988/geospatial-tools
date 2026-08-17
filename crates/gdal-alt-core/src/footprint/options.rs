use anyhow::{bail, Result};
use geotiff_reader::GeoTiffFile;

use crate::cog::mask::discover_dataset_masks;
use crate::cog::semantics::{associated_alpha_band_index, black_rgb_candidate, parse_nodata};
use crate::input::RasterProfile;
use tiff_core::PhotometricInterpretation;

use super::georef::FootprintGeorefChoice;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValiditySourceChoice {
    Auto,
    Mask,
    Alpha,
    Nodata,
    NonZero,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FootprintOutputFormat {
    #[default]
    GeoJson,
    Wkt,
    /// Space-separated `lon lat` pairs (no geometry header)
    WktFlat,
}

#[derive(Debug, Clone)]
pub struct FootprintOptions {
    pub source: ValiditySourceChoice,
    /// When true, associated alpha can be used in auto mode (COG parity).
    pub mask_from_alpha: bool,
    /// Treat RGB(0,0,0) as invalid when enabled.
    pub black_rgb_transparent: bool,
    /// Douglas–Peucker tolerance in map units (0 = no simplification).
    pub simplify_tolerance: f64,
    /// Tile size for parallel validity reads.
    pub tile_size: u32,
    /// In auto mode, treat single-band zero/near-zero pixels as invalid (SAR padding).
    pub nonzero_in_auto: bool,
    /// Absolute threshold for nonzero validity (`0.0` = exact zero only).
    pub zero_threshold: f64,
    /// Keep only the largest validity ring (drop interior holes / speckle rings).
    pub outer_only: bool,
    /// Keep all rings even for SAR/nonzero validity (disables auto outer-only).
    pub all_rings: bool,
    /// Douglas–Peucker tolerance in degrees after WGS84 reprojection (0 = none).
    pub simplify_degrees: f64,
    /// Constant elevation (meters) for RPC georeferencing instead of the model offset.
    pub rpc_height: Option<f64>,
    /// GeoTIFF DEM path for per-pixel RPC height refinement.
    pub dem_path: Option<String>,
    /// Force a specific georeferencing model (default: auto-detect).
    pub georef: Option<FootprintGeorefChoice>,
    /// Maximum GCP count for thin-plate spline fitting (subsample when denser).
    pub tps_max_points: usize,
    /// Output geometry encoding.
    pub output_format: FootprintOutputFormat,
}

impl Default for FootprintOptions {
    fn default() -> Self {
        Self {
            source: ValiditySourceChoice::Auto,
            mask_from_alpha: true,
            black_rgb_transparent: false,
            simplify_tolerance: 0.0,
            tile_size: 512,
            nonzero_in_auto: true,
            zero_threshold: 0.0,
            outer_only: false,
            all_rings: false,
            simplify_degrees: 0.0,
            rpc_height: None,
            dem_path: None,
            georef: None,
            tps_max_points: 400,
            output_format: FootprintOutputFormat::GeoJson,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedValiditySource {
    DatasetMask,
    AssociatedAlpha,
    Nodata,
    NonZero,
    BlackRgb,
    Full,
}

impl ResolvedValiditySource {
    pub fn label(self) -> &'static str {
        match self {
            Self::DatasetMask => "mask",
            Self::AssociatedAlpha => "alpha",
            Self::Nodata => "nodata",
            Self::NonZero => "nonzero",
            Self::BlackRgb => "black_rgb",
            Self::Full => "full",
        }
    }
}

pub fn resolve_validity_source(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    opts: &FootprintOptions,
) -> Result<ResolvedValiditySource> {
    match opts.source {
        ValiditySourceChoice::Mask => {
            if discover_dataset_masks(input).is_none() {
                bail!("--source mask requested but no dataset mask IFD was found");
            }
            Ok(ResolvedValiditySource::DatasetMask)
        }
        ValiditySourceChoice::Alpha => {
            if associated_alpha_band_index(profile).is_none() {
                bail!("--source alpha requested but no associated alpha band was found");
            }
            Ok(ResolvedValiditySource::AssociatedAlpha)
        }
        ValiditySourceChoice::Nodata => {
            if nodata_value_for_profile(profile).is_none() {
                bail!("--source nodata requested but the raster has no usable nodata value");
            }
            Ok(ResolvedValiditySource::Nodata)
        }
        ValiditySourceChoice::NonZero => Ok(ResolvedValiditySource::NonZero),
        ValiditySourceChoice::Full => Ok(ResolvedValiditySource::Full),
        ValiditySourceChoice::Auto => Ok(resolve_auto(input, profile, opts)),
    }
}

fn resolve_auto(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    opts: &FootprintOptions,
) -> ResolvedValiditySource {
    if discover_dataset_masks(input).is_some() {
        return ResolvedValiditySource::DatasetMask;
    }
    if opts.mask_from_alpha && associated_alpha_band_index(profile).is_some() {
        return ResolvedValiditySource::AssociatedAlpha;
    }
    if profile.nodata.is_some() && nodata_value_for_profile(profile).is_some() {
        return ResolvedValiditySource::Nodata;
    }
    if opts.nonzero_in_auto && nonzero_candidate(profile) {
        return ResolvedValiditySource::NonZero;
    }
    if opts.black_rgb_transparent && black_rgb_candidate(profile) {
        return ResolvedValiditySource::BlackRgb;
    }
    ResolvedValiditySource::Full
}

pub fn resolve_validity_source_jp2(
    profile: &RasterProfile,
    opts: &FootprintOptions,
) -> Result<ResolvedValiditySource> {
    match opts.source {
        ValiditySourceChoice::Mask => {
            if jp2_has_mask_channel(profile) || jp2_is_bitmask_dataset(profile) {
                Ok(if jp2_is_bitmask_dataset(profile) {
                    ResolvedValiditySource::DatasetMask
                } else {
                    ResolvedValiditySource::AssociatedAlpha
                })
            } else {
                bail!("--source mask requested but the JP2 has no opacity/mask channel");
            }
        }
        ValiditySourceChoice::Alpha => {
            if associated_alpha_band_index(profile).is_none() {
                bail!("--source alpha requested but no associated alpha band was found");
            }
            Ok(ResolvedValiditySource::AssociatedAlpha)
        }
        ValiditySourceChoice::Nodata => {
            if nodata_value_for_profile(profile).is_none() {
                bail!("--source nodata requested but the raster has no usable nodata value");
            }
            Ok(ResolvedValiditySource::Nodata)
        }
        ValiditySourceChoice::NonZero => Ok(ResolvedValiditySource::NonZero),
        ValiditySourceChoice::Full => Ok(ResolvedValiditySource::Full),
        ValiditySourceChoice::Auto => Ok(resolve_auto_jp2(profile, opts)),
    }
}

fn resolve_auto_jp2(profile: &RasterProfile, opts: &FootprintOptions) -> ResolvedValiditySource {
    if jp2_is_bitmask_dataset(profile) && opts.mask_from_alpha {
        return ResolvedValiditySource::DatasetMask;
    }
    if jp2_has_mask_channel(profile) && opts.mask_from_alpha {
        return ResolvedValiditySource::AssociatedAlpha;
    }
    if profile.nodata.is_some() && nodata_value_for_profile(profile).is_some() {
        return ResolvedValiditySource::Nodata;
    }
    if opts.nonzero_in_auto && nonzero_candidate(profile) {
        return ResolvedValiditySource::NonZero;
    }
    if opts.black_rgb_transparent && black_rgb_candidate(profile) {
        return ResolvedValiditySource::BlackRgb;
    }
    ResolvedValiditySource::Full
}

pub fn nonzero_candidate(profile: &RasterProfile) -> bool {
    profile.bands == 1 && profile.photometric == PhotometricInterpretation::MinIsBlack
}

pub fn jp2_has_mask_channel(profile: &RasterProfile) -> bool {
    associated_alpha_band_index(profile).is_some()
}

/// Single-band JP2 used as a GDAL-style bitmask dataset (not a greyscale image).
pub fn jp2_is_bitmask_dataset(profile: &RasterProfile) -> bool {
    profile.bands == 1
        && profile.photometric == PhotometricInterpretation::MinIsBlack
        && profile.sample.bits_per_sample <= 8
}

pub fn nodata_value_for_profile(profile: &RasterProfile) -> Option<NodataValue> {
    let layout = &profile.sample;
    if profile.nodata.is_none() {
        return None;
    }
    let nodata = &profile.nodata;
    match (layout.sample_format, layout.bits_per_sample) {
        (tiff_core::SampleFormat::Uint, 8) => parse_nodata::<u8>(layout, nodata).map(NodataValue::U8),
        (tiff_core::SampleFormat::Uint, 16) => parse_nodata::<u16>(layout, nodata).map(NodataValue::U16),
        (tiff_core::SampleFormat::Uint, 32) => parse_nodata::<u32>(layout, nodata).map(NodataValue::U32),
        (tiff_core::SampleFormat::Int, 8) => parse_nodata::<i8>(layout, nodata).map(NodataValue::I8),
        (tiff_core::SampleFormat::Int, 16) => parse_nodata::<i16>(layout, nodata).map(NodataValue::I16),
        (tiff_core::SampleFormat::Int, 32) => parse_nodata::<i32>(layout, nodata).map(NodataValue::I32),
        (tiff_core::SampleFormat::Float, 32) => parse_nodata::<f32>(layout, nodata).map(NodataValue::F32),
        (tiff_core::SampleFormat::Float, 64) => parse_nodata::<f64>(layout, nodata).map(NodataValue::F64),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
pub enum NodataValue {
    U8(u8),
    U16(u16),
    U32(u32),
    I8(i8),
    I16(i16),
    I32(i32),
    F32(f32),
    F64(f64),
}

impl NodataValue {
    pub fn matches_u8(self, value: u8) -> bool {
        match self {
            Self::U8(v) => value == v,
            Self::U16(v) => value as u16 == v,
            Self::I8(v) => value as i8 == v,
            Self::I16(v) => value as i16 == v,
            Self::F32(v) => (value as f32 - v).abs() < f32::EPSILON,
            Self::F64(v) => (value as f64 - v).abs() < f64::EPSILON,
            Self::U32(v) => value as u32 == v,
            Self::I32(v) => value as i32 == v,
        }
    }

    pub fn matches_u16(self, value: u16) -> bool {
        match self {
            Self::U8(v) => value == v as u16,
            Self::U16(v) => value == v,
            Self::I8(v) => value as i16 == v as i16,
            Self::I16(v) => value as i16 == v,
            Self::F32(v) => (value as f32 - v).abs() < f32::EPSILON,
            Self::F64(v) => (value as f64 - v).abs() < f64::EPSILON,
            Self::U32(v) => value as u32 == v,
            Self::I32(v) => value as i32 == v,
        }
    }

    pub fn matches_i16(self, value: i16) -> bool {
        match self {
            Self::U8(v) => value as u8 == v,
            Self::U16(v) => value as u16 == v,
            Self::I8(v) => value == v as i16,
            Self::I16(v) => value == v,
            Self::F32(v) => (value as f32 - v).abs() < f32::EPSILON,
            Self::F64(v) => (value as f64 - v).abs() < f64::EPSILON,
            Self::U32(v) => value as u32 == v,
            Self::I32(v) => value as i32 == v,
        }
    }

    pub fn matches_f32(self, value: f32) -> bool {
        match self {
            Self::F32(v) => (value - v).abs() < f32::EPSILON,
            Self::F64(v) => (value as f64 - v).abs() < f64::EPSILON,
            _ => false,
        }
    }

    pub fn matches_f64(self, value: f64) -> bool {
        match self {
            Self::F64(v) => (value - v).abs() < f64::EPSILON,
            Self::F32(v) => (value - v as f64).abs() < f64::EPSILON,
            _ => false,
        }
    }

    pub fn matches_u32(self, value: u32) -> bool {
        match self {
            Self::U32(v) => value == v,
            Self::F64(v) => (value as f64 - v).abs() < f64::EPSILON,
            Self::F32(v) => (value as f32 - v).abs() < f32::EPSILON,
            _ => false,
        }
    }

    pub fn matches_i32(self, value: i32) -> bool {
        match self {
            Self::I32(v) => value == v,
            Self::F64(v) => (value as f64 - v).abs() < f64::EPSILON,
            Self::F32(v) => (value as f32 - v).abs() < f32::EPSILON,
            _ => false,
        }
    }

    pub fn matches_i8(self, value: i8) -> bool {
        match self {
            Self::I8(v) => value == v,
            Self::I16(v) => value as i16 == v,
            Self::F32(v) => (value as f32 - v).abs() < f32::EPSILON,
            Self::F64(v) => (value as f64 - v).abs() < f64::EPSILON,
            _ => false,
        }
    }
}
