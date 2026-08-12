use anyhow::{Context, Result};
use geotiff_core::geokeys::GeoKeyDirectory;
use geotiff_core::tags::TAG_MODEL_TRANSFORMATION;
use geotiff_core::{CrsInfo, GeoTransform, HorizontalCrs, ModelType, RasterType};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::GeoTiffBuilder;
use tiff_core::{
    ColorMap, ColorModel, ExtraSample, InkSet, PhotometricInterpretation, PlanarConfiguration,
    SampleFormat, TagValue, YCbCrPositioning,
};
use tiff_reader::Ifd;

use crate::cog::{apply_compression, tiff_variant, CogOutputOptions};

#[derive(Debug, Clone)]
pub struct SampleLayout {
    pub bits_per_sample: u16,
    pub sample_format: SampleFormat,
}

#[derive(Debug, Clone)]
pub struct GeorefProfile {
    pub crs: CrsInfo,
    pub geokeys: GeoKeyDirectory,
    pub affine: Option<GeoTransform>,
    pub transformation_matrix: Option<[f64; 16]>,
    /// Full ModelTiepointTag payload when the source is GCP-based (no affine transform).
    pub model_tiepoints: Option<Vec<f64>>,
}

#[derive(Debug, Clone)]
pub struct RasterProfile {
    pub width: u32,
    pub height: u32,
    pub bands: u32,
    pub sample: SampleLayout,
    pub photometric: PhotometricInterpretation,
    pub planar_configuration: PlanarConfiguration,
    pub extra_samples: Vec<ExtraSample>,
    pub color_map: Option<ColorMap>,
    pub ink_set: Option<InkSet>,
    pub ycbcr_subsampling: Option<[u16; 2]>,
    pub ycbcr_positioning: Option<YCbCrPositioning>,
    pub nodata: Option<String>,
    pub georef: GeorefProfile,
}

impl RasterProfile {
    pub fn from_geotiff(input: &GeoTiffFile) -> Result<Self> {
        let ifd = input
            .tiff()
            .ifd(input.base_ifd_index())
            .context("failed to read base image metadata")?;
        let layout = ifd
            .raster_layout()
            .context("failed to parse input raster layout")?;
        let color_model = ifd.color_model().context("failed to read color model")?;
        let (extra_samples, color_map) = color_model_fields(color_model);

        Ok(Self {
            width: input.width(),
            height: input.height(),
            bands: input.band_count(),
            sample: SampleLayout {
                bits_per_sample: layout.bits_per_sample,
                sample_format: SampleFormat::from_code(layout.sample_format)
                    .unwrap_or(SampleFormat::Uint),
            },
            photometric: ifd
                .photometric_interpretation_enum()
                .unwrap_or(PhotometricInterpretation::MinIsBlack),
            planar_configuration: PlanarConfiguration::from_code(ifd.planar_configuration())
                .unwrap_or(PlanarConfiguration::Chunky),
            extra_samples,
            color_map,
            ink_set: ifd.ink_set()?,
            ycbcr_subsampling: ifd.ycbcr_subsampling()?,
            ycbcr_positioning: ifd.ycbcr_positioning()?,
            nodata: input.nodata().map(str::to_owned),
            georef: GeorefProfile {
                crs: input.crs().clone(),
                geokeys: input.geokeys().clone(),
                affine: input.transform().cloned(),
                transformation_matrix: read_transformation_matrix(ifd)?,
                model_tiepoints: read_model_tiepoints(input),
            },
        })
    }

    pub fn base_builder(&self, opts: &CogOutputOptions) -> GeoTiffBuilder {
        let mut builder = GeoTiffBuilder::new(self.width, self.height)
            .bands(self.bands)
            .tile_size(opts.blocksize, opts.blocksize)
            .photometric(self.photometric)
            .planar_configuration(self.planar_configuration)
            .tiff_variant(tiff_variant(
                self.width,
                self.height,
                self.bands,
                self.sample.bits_per_sample,
            ));

        if !self.extra_samples.is_empty() {
            builder = builder.extra_samples(self.extra_samples.clone());
        }
        if let Some(color_map) = &self.color_map {
            builder = builder.color_map(color_map.clone());
        }
        if let Some(ink_set) = self.ink_set {
            builder = builder.ink_set(ink_set);
        }
        if let Some(subsampling) = self.ycbcr_subsampling {
            builder = builder.ycbcr_subsampling(subsampling);
        }
        if let Some(positioning) = self.ycbcr_positioning {
            builder = builder.ycbcr_positioning(positioning);
        }

        builder = apply_georef(builder, &self.georef);

        if let Some(nodata) = &self.nodata {
            builder = builder.nodata(nodata);
        }

        apply_compression(builder, opts)
    }

    pub fn with_window(&self, window: &crate::crop::WriteWindow) -> Self {
        let mut profile = self.clone();
        profile.width = window.width as u32;
        profile.height = window.height as u32;
        if let Some(affine) = profile.georef.affine.as_mut() {
            *affine = crate::crop::shift_transform(affine, window.col_off, window.row_off);
        }
        if let Some(tiepoints) = profile.georef.model_tiepoints.as_mut() {
            shift_model_tiepoints(tiepoints, window.col_off, window.row_off);
        }
        profile
    }

    pub fn with_band_subset(&self, bands: &[usize]) -> Self {
        let mut profile = self.clone();
        profile.bands = bands.len() as u32;
        if profile.bands == 3 {
            profile.photometric = PhotometricInterpretation::Rgb;
        }
        profile.extra_samples.clear();
        profile
    }
}

pub fn apply_georef(mut builder: GeoTiffBuilder, georef: &GeorefProfile) -> GeoTiffBuilder {
    builder = builder.crs(georef.crs.clone());
    for key in &georef.geokeys.keys {
        builder = builder.geokey(key.id, key.value.clone());
    }

    if let Some(matrix) = georef.transformation_matrix {
        builder.transformation_matrix(matrix)
    } else if let Some(transform) = georef.affine {
        builder.transform(transform)
    } else if let Some(tiepoints) = &georef.model_tiepoints {
        builder.model_tiepoints(tiepoints.clone())
    } else {
        builder
    }
}

pub fn projected_georef(epsg: u16, transform: GeoTransform) -> GeorefProfile {
    let mut geokeys = GeoKeyDirectory::new();
    let crs = CrsInfo {
        model_type: ModelType::Projected.code(),
        raster_type: RasterType::PixelIsArea.code(),
        horizontal: Some(HorizontalCrs {
            projected_epsg: Some(epsg),
            ..Default::default()
        }),
        vertical: None,
    };
    crs.apply_to_geokeys(&mut geokeys);
    GeorefProfile {
        crs,
        geokeys,
        affine: Some(transform),
        transformation_matrix: None,
        model_tiepoints: None,
    }
}

fn color_model_fields(color_model: ColorModel) -> (Vec<ExtraSample>, Option<ColorMap>) {
    match color_model {
        ColorModel::Grayscale { extra_samples, .. }
        | ColorModel::Rgb { extra_samples }
        | ColorModel::Cmyk { extra_samples }
        | ColorModel::CieLab { extra_samples } => (extra_samples, None),
        ColorModel::Palette {
            color_map,
            extra_samples,
        } => (extra_samples, Some(color_map)),
        ColorModel::Separated { extra_samples, .. } | ColorModel::YCbCr { extra_samples, .. } => {
            (extra_samples, None)
        }
        ColorModel::TransparencyMask => (Vec::new(), None),
    }
}

fn read_transformation_matrix(ifd: &Ifd) -> Result<Option<[f64; 16]>> {
    let Some(tag) = ifd.tag(TAG_MODEL_TRANSFORMATION) else {
        return Ok(None);
    };
    let TagValue::Double(values) = &tag.value else {
        return Ok(None);
    };
    if values.len() != 16 {
        return Ok(None);
    }
    let mut matrix = [0.0; 16];
    matrix.copy_from_slice(values);
    Ok(Some(matrix))
}

fn read_model_tiepoints(input: &GeoTiffFile) -> Option<Vec<f64>> {
    if input.transform().is_some() || input.metadata().transformation.is_some() {
        return None;
    }
    let tiepoints = &input.metadata().tiepoints;
    if tiepoints.is_empty() {
        return None;
    }
    Some(
        tiepoints
            .iter()
            .flat_map(|tiepoint| tiepoint.iter().copied())
            .collect(),
    )
}

fn shift_model_tiepoints(tiepoints: &mut [f64], col_off: usize, row_off: usize) {
    for chunk in tiepoints.chunks_mut(6) {
        chunk[0] -= col_off as f64;
        chunk[1] -= row_off as f64;
    }
}
