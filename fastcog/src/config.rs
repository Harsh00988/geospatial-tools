use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};
use geotiff_writer::cog::Resampling;
use geotiff_writer::Compression;
use std::path::PathBuf;

/// Convert rasters to Cloud Optimized GeoTIFF (COG) format.
///
/// GeoTIFF inputs use the pure-Rust pipeline. JP2 inputs use OpenJPEG via `jpeg2k`.
#[derive(Parser, Debug, Clone)]
#[command(name = "fastcog", version, about)]
pub struct Args {
    /// Input raster path (GeoTIFF or JP2)
    pub input: PathBuf,

    /// Output COG path
    pub output: PathBuf,

    /// Tile block size in pixels (must be a multiple of 16)
    #[arg(short = 'b', long, default_value_t = 512)]
    pub blocksize: u32,

    /// Compression algorithm
    #[arg(short = 'c', long, value_enum, default_value_t = CompressionChoice::Deflate)]
    pub compress: CompressionChoice,

    /// Deflate compression level (0-9)
    #[arg(long, default_value_t = 6)]
    pub deflate_level: u32,

    /// Overview resampling method
    #[arg(short = 'r', long, value_enum, default_value_t = ResamplingChoice::Average)]
    pub resampling: ResamplingChoice,

    /// Explicit overview factors (e.g. 2 4 8). Auto-computed when omitted.
    #[arg(short = 'o', long, value_delimiter = ' ')]
    pub overviews: Option<Vec<u32>>,

    /// Skip overview generation (still writes COG layout)
    #[arg(long)]
    pub no_overviews: bool,

    /// Use memory-mapped reads for local input files
    #[arg(long)]
    pub mmap: bool,

    /// Number of threads for parallel tile decoding (0 = all CPUs)
    #[arg(short = 'j', long, default_value_t = 0)]
    pub jobs: usize,

    /// Suppress progress output (enabled by default on interactive terminals)
    #[arg(short = 'q', long)]
    pub quiet: bool,
}

impl Args {
    pub fn validate(&self) -> Result<()> {
        if !self.blocksize.is_multiple_of(16) {
            bail!(
                "blocksize must be a multiple of 16 (got {})",
                self.blocksize
            );
        }
        if self.deflate_level > 9 {
            bail!(
                "deflate-level must be between 0 and 9 (got {})",
                self.deflate_level
            );
        }
        Ok(())
    }

    pub fn show_progress(&self) -> bool {
        !self.quiet && std::io::IsTerminal::is_terminal(&std::io::stderr())
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
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
}

#[derive(Clone, Copy, Debug, ValueEnum)]
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
}
