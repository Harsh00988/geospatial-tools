use anyhow::Result;
use clap::{Parser, ValueEnum};
use gdal_alt_core::{
    convert_geotiff, CompressionChoice, ConvertRequest, CogOutputOptions,
    LercAdditionalCompressionChoice, ResamplingChoice,
};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "fastband", version, about = "Subset or reorder bands into a new COG")]
struct Args {
    pub input: PathBuf,
    pub output: PathBuf,

    /// Bands to copy, 1-based (repeatable, gdal_translate -b)
    #[arg(short = 'b', long = "band", required = true)]
    pub bands: Vec<usize>,

    #[arg(short = 'B', long = "blocksize", default_value_t = 512)]
    pub blocksize: u32,

    #[arg(short = 'c', long, value_enum, default_value_t = CompressionArg::Deflate)]
    pub compress: CompressionArg,

    #[arg(long, default_value_t = 6)]
    pub deflate_level: u32,

    #[arg(short = 'r', long, value_enum, default_value_t = ResamplingArg::Average)]
    pub resampling: ResamplingArg,

    #[arg(short = 'o', long, value_delimiter = ' ')]
    pub overviews: Option<Vec<u32>>,

    #[arg(long)]
    pub no_overviews: bool,

    #[arg(long)]
    pub mmap: bool,

    #[arg(short = 'j', long, default_value_t = 0)]
    pub jobs: usize,

    #[arg(short = 'q', long)]
    pub quiet: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CompressionArg {
    None,
    Lzw,
    Deflate,
    Zstd,
    Jpeg,
    Lerc,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ResamplingArg {
    Nearest,
    Average,
    Bilinear,
    Cubic,
    Lanczos,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let opts = cog_options(&args);
    opts.validate()?;

    let input = args.input.to_string_lossy();
    let pool = gdal_alt_core::util::thread_pool(args.jobs)?;
    let started = std::time::Instant::now();
    let _result = convert_geotiff(
        &pool,
        &ConvertRequest {
            input: &input,
            output: &args.output,
            opts: &opts,
            mmap: args.mmap,
            show_progress: !args.quiet && std::io::IsTerminal::is_terminal(&std::io::stderr()),
            window: None,
            bands: Some(args.bands.clone()),
        },
    )?;
    eprintln!(
        "fastband: wrote {} in {:.2}s",
        args.output.display(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn cog_options(args: &Args) -> CogOutputOptions {
    CogOutputOptions {
        blocksize: args.blocksize,
        compression: match args.compress {
            CompressionArg::None => CompressionChoice::None,
            CompressionArg::Lzw => CompressionChoice::Lzw,
            CompressionArg::Deflate => CompressionChoice::Deflate,
            CompressionArg::Zstd => CompressionChoice::Zstd,
            CompressionArg::Jpeg => CompressionChoice::Jpeg,
            CompressionArg::Lerc => CompressionChoice::Lerc,
        },
        deflate_level: args.deflate_level,
        jpeg_quality: 75,
        lerc_max_z_error: 0.0,
        lerc_additional_compression: LercAdditionalCompressionChoice::None,
        resampling: match args.resampling {
            ResamplingArg::Nearest => ResamplingChoice::Nearest,
            ResamplingArg::Average => ResamplingChoice::Average,
            ResamplingArg::Bilinear => ResamplingChoice::Bilinear,
            ResamplingArg::Cubic => ResamplingChoice::Cubic,
            ResamplingArg::Lanczos => ResamplingChoice::Lanczos,
        },
        overview_levels: args.overviews.clone(),
        no_overviews: args.no_overviews,
        mask_from_alpha: true,
        black_rgb_transparent: false,
    }
}
