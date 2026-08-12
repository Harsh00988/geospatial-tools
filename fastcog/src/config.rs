use anyhow::Result;
use clap::{Parser, ValueEnum};
use gdal_alt_core::{CogOutputOptions, CompressionChoice, ResamplingChoice};
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
    #[arg(short = 'c', long, value_enum, default_value_t = CompressionArg::Deflate)]
    pub compress: CompressionArg,

    /// Deflate compression level (0-9)
    #[arg(long, default_value_t = 6)]
    pub deflate_level: u32,

    /// Overview resampling method
    #[arg(short = 'r', long, value_enum, default_value_t = ResamplingArg::Average)]
    pub resampling: ResamplingArg,

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
        self.cog_options().validate()
    }

    pub fn show_progress(&self) -> bool {
        !self.quiet && std::io::IsTerminal::is_terminal(&std::io::stderr())
    }

    pub fn cog_options(&self) -> CogOutputOptions {
        CogOutputOptions {
            blocksize: self.blocksize,
            compression: match self.compress {
                CompressionArg::None => CompressionChoice::None,
                CompressionArg::Lzw => CompressionChoice::Lzw,
                CompressionArg::Deflate => CompressionChoice::Deflate,
                CompressionArg::Zstd => CompressionChoice::Zstd,
                CompressionArg::Jpeg => CompressionChoice::Jpeg,
            },
            deflate_level: self.deflate_level,
            resampling: match self.resampling {
                ResamplingArg::Nearest => ResamplingChoice::Nearest,
                ResamplingArg::Average => ResamplingChoice::Average,
            },
            overview_levels: self.overviews.clone(),
            no_overviews: self.no_overviews,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CompressionArg {
    None,
    Lzw,
    Deflate,
    Zstd,
    Jpeg,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum ResamplingArg {
    Nearest,
    Average,
}
