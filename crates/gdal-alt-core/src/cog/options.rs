use anyhow::{bail, Result};
use geotiff_writer::cog::Resampling;
use geotiff_writer::Compression;

use super::grid::auto_overview_levels;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionChoice {
    None,
    Lzw,
    Deflate,
    Zstd,
    Jpeg,
}

impl CompressionChoice {
    pub fn to_compression(self) -> Compression {
        match self {
            Self::None => Compression::None,
            Self::Lzw => Compression::Lzw,
            Self::Deflate => Compression::Deflate,
            Self::Zstd => Compression::Zstd,
            Self::Jpeg => Compression::Jpeg,
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "lzw" => Ok(Self::Lzw),
            "deflate" => Ok(Self::Deflate),
            "zstd" => Ok(Self::Zstd),
            "jpeg" => Ok(Self::Jpeg),
            _ => bail!("unknown compression: {name}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResamplingChoice {
    Nearest,
    Average,
}

impl ResamplingChoice {
    pub fn to_resampling(self) -> Resampling {
        match self {
            Self::Nearest => Resampling::NearestNeighbor,
            Self::Average => Resampling::Average,
        }
    }

    pub fn from_name(name: &str) -> Result<Self> {
        match name.to_ascii_lowercase().as_str() {
            "nearest" => Ok(Self::Nearest),
            "average" => Ok(Self::Average),
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
}
