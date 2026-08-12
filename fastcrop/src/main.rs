use anyhow::Result;
use clap::{Parser, ValueEnum};
use gdal_alt_core::{
    convert_geotiff, window_from_projwin, window_from_srcwin, CompressionChoice,
    ConvertRequest, CogOutputOptions, LercAdditionalCompressionChoice, ResamplingChoice,
};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "fastcrop", version, about = "Extract a geographic or pixel window to COG")]
struct Args {
    pub input: PathBuf,
    pub output: PathBuf,

    /// Pixel window: col row width height (gdal_translate -srcwin)
    #[arg(long, value_name = "COL ROW WIDTH HEIGHT", num_args = 4)]
    pub srcwin: Option<Vec<usize>>,

    /// Geographic window: ulx uly lrx lry (gdal_translate -projwin)
    #[arg(long, value_name = "ULX ULY LRX LRY", num_args = 4)]
    pub projwin: Option<Vec<f64>>,

    #[arg(short = 'b', long, default_value_t = 512)]
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
    if args.srcwin.is_some() && args.projwin.is_some() {
        anyhow::bail!("use only one of --srcwin or --projwin");
    }
    if args.srcwin.is_none() && args.projwin.is_none() {
        anyhow::bail!("specify --srcwin or --projwin");
    }

    let opts = cog_options(&args);
    opts.validate()?;

    let input = args.input.to_string_lossy();
    let handle = gdal_alt_core::open::open_input(&input, args.mmap)?;
    let geotiff = handle.as_file();
    let window = if let Some(values) = &args.srcwin {
        let [col, row, width, height] = values.as_slice() else {
            anyhow::bail!("--srcwin requires four integers");
        };
        window_from_srcwin(geotiff.width(), geotiff.height(), *col, *row, *width, *height)?
    } else {
        let values = args.projwin.as_ref().unwrap();
        let [ulx, uly, lrx, lry] = values.as_slice() else {
            anyhow::bail!("--projwin requires four coordinates");
        };
        window_from_projwin(geotiff, *ulx, *uly, *lrx, *lry)?
    };

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
            window: Some(window),
            bands: None,
        },
    )?;
    eprintln!(
        "fastcrop: wrote {} in {:.2}s",
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
