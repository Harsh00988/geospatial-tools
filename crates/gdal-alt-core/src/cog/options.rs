use anyhow::{bail, Result};
use geotiff_writer::cog::Resampling;
use geotiff_writer::{Compression, JpegOptions, LercOptions};
use tiff_core::{LercAdditionalCompression, Predictor, SampleFormat};

use super::grid::auto_overview_levels;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionChoice {
    None,
    Lzw,
    Deflate,
    Zstd,
    Jpeg,
    Lerc,
}

impl CompressionChoice {
    pub fn to_compression(self) -> Compression {
        match self {
            Self::None => Compression::None,
            Self::Lzw => Compression::Lzw,
            Self::Deflate => Compression::Deflate,
            Self::Zstd => Compression::Zstd,
            Self::Jpeg => Compression::Jpeg,
            Self::Lerc => Compression::Lerc,
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "lzw" => Ok(Self::Lzw),
            "deflate" => Ok(Self::Deflate),
            "zstd" => Ok(Self::Zstd),
            "jpeg" => Ok(Self::Jpeg),
            "lerc" => Ok(Self::Lerc),
            _ => bail!("unknown compression: {name}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LercAdditionalCompressionChoice {
    None,
    Deflate,
    Zstd,
}

impl LercAdditionalCompressionChoice {
    pub fn to_lerc_additional(self) -> LercAdditionalCompression {
        match self {
            Self::None => LercAdditionalCompression::None,
            Self::Deflate => LercAdditionalCompression::Deflate,
            Self::Zstd => LercAdditionalCompression::Zstd,
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "deflate" => Ok(Self::Deflate),
            "zstd" => Ok(Self::Zstd),
            _ => bail!("unknown LERC additional compression: {name}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResamplingChoice {
    Nearest,
    Average,
    Bilinear,
    Cubic,
    Lanczos,
}

impl ResamplingChoice {
    pub fn to_resampling(self) -> Resampling {
        match self {
            Self::Nearest => Resampling::NearestNeighbor,
            Self::Average => Resampling::Average,
            Self::Bilinear => Resampling::Bilinear,
            Self::Cubic => Resampling::Cubic,
            Self::Lanczos => Resampling::Lanczos,
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "nearest" => Ok(Self::Nearest),
            "average" | "avg" | "mean" => Ok(Self::Average),
            "bilinear" | "linear" => Ok(Self::Bilinear),
            "cubic" => Ok(Self::Cubic),
            "lanczos" => Ok(Self::Lanczos),
            _ => bail!("unknown resampling: {name}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CogOutputOptions {
    pub blocksize: u32,
    pub compression: CompressionChoice,
    pub deflate_level: u32,
    pub resampling: ResamplingChoice,
    pub overview_levels: Option<Vec<u32>>,
    pub no_overviews: bool,
    /// Synthesize a GDAL mask IFD from an associated alpha band when no dataset mask exists.
    pub mask_from_alpha: bool,
    /// Treat RGB(0,0,0) as transparent when no dataset mask or alpha band exists.
    pub black_rgb_transparent: bool,
    /// JPEG quality (1–100) when compression is JPEG.
    pub jpeg_quality: u8,
    /// Maximum per-sample error for LERC (0 = lossless).
    pub lerc_max_z_error: f64,
    /// Extra compression applied to LERC tile payloads.
    pub lerc_additional_compression: LercAdditionalCompressionChoice,
}

impl Default for CogOutputOptions {
    fn default() -> Self {
        Self {
            blocksize: 512,
            compression: CompressionChoice::Deflate,
            deflate_level: 6,
            resampling: ResamplingChoice::Average,
            overview_levels: None,
            no_overviews: false,
            mask_from_alpha: true,
            black_rgb_transparent: false,
            jpeg_quality: 75,
            lerc_max_z_error: 0.0,
            lerc_additional_compression: LercAdditionalCompressionChoice::None,
        }
    }
}

impl CogOutputOptions {
    pub fn validate(&self) -> Result<()> {
        if !self.blocksize.is_multiple_of(16) {
            bail!("blocksize must be a multiple of 16 (got {})", self.blocksize);
        }
        if self.deflate_level > 9 {
            bail!("deflate-level must be between 0 and 9 (got {})", self.deflate_level);
        }
        if self.jpeg_quality == 0 || self.jpeg_quality > 100 {
            bail!("jpeg-quality must be between 1 and 100 (got {})", self.jpeg_quality);
        }
        if self.lerc_max_z_error < 0.0 {
            bail!("lerc-max-z-error must be non-negative (got {})", self.lerc_max_z_error);
        }
        Ok(())
    }

    pub fn effective_overview_levels(&self, width: u32, height: u32) -> Vec<u32> {
        if self.no_overviews {
            return Vec::new();
        }
        self.overview_levels.clone().unwrap_or_else(|| {
            auto_overview_levels(width, height, self.blocksize)
        })
    }

    pub fn lerc_options(&self) -> LercOptions {
        LercOptions {
            max_z_error: self.lerc_max_z_error,
            additional_compression: self.lerc_additional_compression.to_lerc_additional(),
        }
    }

    pub fn jpeg_options(&self) -> JpegOptions {
        JpegOptions {
            quality: self.jpeg_quality,
        }
    }

    pub fn encode_predictor_for(&self, sample_format: SampleFormat) -> Predictor {
        if sample_format == SampleFormat::Float {
            return Predictor::None;
        }
        match self.compression {
            CompressionChoice::Deflate | CompressionChoice::Zstd | CompressionChoice::Lzw => {
                Predictor::Horizontal
            }
            _ => Predictor::None,
        }
    }

    pub fn encode_predictor(&self) -> Predictor {
        self.encode_predictor_for(SampleFormat::Uint)
    }
}
