use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use fastcog::{run as run_cog, Args as CogArgs};
use gdal_alt_core::{
    convert_geotiff, extract_footprint, format_report, format_text, gather, validate_cog,
    window_from_projwin, window_from_srcwin, CompressionChoice, ConvertRequest, CogOutputOptions,
    FootprintOptions, LercAdditionalCompressionChoice, ResamplingChoice, ValiditySourceChoice,
};

#[derive(Parser, Debug)]
#[command(
    name = "fasttranslate",
    version,
    about = "Unified GDAL-free raster toolkit",
    long_about = "Wraps fastcog, fastcrop, fastband, fastfootprint, fastinfo, and fastvalidate in one binary."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Convert GeoTIFF or JP2 to Cloud Optimized GeoTIFF (fastcog)
    #[command(visible_alias = "translate")]
    Cog {
        #[command(flatten)]
        args: CogArgs,
    },
    /// Print raster metadata (fastinfo)
    Info {
        /// Input raster path (GeoTIFF or JP2)
        input: PathBuf,
        #[arg(long)]
        mmap: bool,
    },
    /// Extract valid-pixel footprint as GeoJSON (fastfootprint)
    Footprint {
        /// Input GeoTIFF or JP2 path
        input: PathBuf,
        /// Write GeoJSON to this file (stdout when omitted)
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = FootprintSourceArg::Auto)]
        source: FootprintSourceArg,
        #[arg(long, default_value_t = 0.0)]
        simplify: f64,
        #[arg(long)]
        no_mask_from_alpha: bool,
        #[arg(long)]
        black_rgb_transparent: bool,
        #[arg(long)]
        no_nonzero_in_auto: bool,
        #[arg(long, default_value_t = 0.0)]
        zero_threshold: f64,
        #[arg(long, value_name = "COL ROW WIDTH HEIGHT", num_args = 4)]
        srcwin: Option<Vec<usize>>,
        #[arg(long)]
        mmap: bool,
        #[arg(short = 'j', long, default_value_t = 0)]
        jobs: usize,
    },
    /// Validate COG layout (fastvalidate)
    Validate {
        /// Input COG path
        input: PathBuf,
        #[arg(long)]
        mmap: bool,
    },
    /// Extract a pixel or geographic window to COG (fastcrop)
    Crop {
        input: PathBuf,
        output: PathBuf,
        #[arg(long, value_name = "COL ROW WIDTH HEIGHT", num_args = 4)]
        srcwin: Option<Vec<usize>>,
        #[arg(long, value_name = "ULX ULY LRX LRY", num_args = 4)]
        projwin: Option<Vec<f64>>,
        #[arg(short = 'b', long, default_value_t = 512)]
        blocksize: u32,
        #[arg(short = 'c', long, value_enum, default_value_t = CompressionArg::Deflate)]
        compress: CompressionArg,
        #[arg(long, default_value_t = 6)]
        deflate_level: u32,
        /// JPEG quality when using JPEG compression (1-100)
        #[arg(long, default_value_t = 75)]
        jpeg_quality: u8,
        /// Maximum per-sample error for LERC compression (0 = lossless)
        #[arg(long, default_value_t = 0.0)]
        lerc_max_z_error: f64,
        /// Additional compression for LERC payloads: none, deflate, or zstd
        #[arg(long, value_enum, default_value_t = LercAdditionalCompressionArg::None)]
        lerc_additional_compression: LercAdditionalCompressionArg,
        #[arg(short = 'r', long, value_enum, default_value_t = ResamplingArg::Average)]
        resampling: ResamplingArg,
        #[arg(short = 'o', long, value_delimiter = ' ')]
        overviews: Option<Vec<u32>>,
        #[arg(long)]
        no_overviews: bool,
        #[arg(long)]
        mmap: bool,
        #[arg(short = 'j', long, default_value_t = 0)]
        jobs: usize,
        #[arg(short = 'q', long)]
        quiet: bool,
        /// Do not synthesize a GDAL mask IFD from an associated alpha band
        #[arg(long)]
        no_mask_from_alpha: bool,
        /// Treat RGB(0,0,0) as transparent and emit a mask IFD when none exists
        #[arg(long)]
        black_rgb_transparent: bool,
    },
    /// Subset or reorder bands into a new COG (fastband)
    Band {
        input: PathBuf,
        output: PathBuf,
        #[arg(short = 'b', long = "band", required = true)]
        bands: Vec<usize>,
        #[arg(short = 'B', long = "blocksize", default_value_t = 512)]
        blocksize: u32,
        #[arg(short = 'c', long, value_enum, default_value_t = CompressionArg::Deflate)]
        compress: CompressionArg,
        #[arg(long, default_value_t = 6)]
        deflate_level: u32,
        /// JPEG quality when using JPEG compression (1-100)
        #[arg(long, default_value_t = 75)]
        jpeg_quality: u8,
        /// Maximum per-sample error for LERC compression (0 = lossless)
        #[arg(long, default_value_t = 0.0)]
        lerc_max_z_error: f64,
        /// Additional compression for LERC payloads: none, deflate, or zstd
        #[arg(long, value_enum, default_value_t = LercAdditionalCompressionArg::None)]
        lerc_additional_compression: LercAdditionalCompressionArg,
        #[arg(short = 'r', long, value_enum, default_value_t = ResamplingArg::Average)]
        resampling: ResamplingArg,
        #[arg(short = 'o', long, value_delimiter = ' ')]
        overviews: Option<Vec<u32>>,
        #[arg(long)]
        no_overviews: bool,
        #[arg(long)]
        mmap: bool,
        #[arg(short = 'j', long, default_value_t = 0)]
        jobs: usize,
        #[arg(short = 'q', long)]
        quiet: bool,
        /// Do not synthesize a GDAL mask IFD from an associated alpha band
        #[arg(long)]
        no_mask_from_alpha: bool,
        /// Treat RGB(0,0,0) as transparent and emit a mask IFD when none exists
        #[arg(long)]
        black_rgb_transparent: bool,
    },
    /// Convert every raster in a directory to COG (parallel jobs)
    Batch {
        /// Input directory containing GeoTIFF/JP2 files
        input_dir: PathBuf,
        /// Output directory for COG files (created if missing)
        output_dir: PathBuf,
        #[arg(short = 'b', long, default_value_t = 512)]
        blocksize: u32,
        #[arg(short = 'c', long, value_enum, default_value_t = CompressionArg::Deflate)]
        compress: CompressionArg,
        #[arg(long, default_value_t = 6)]
        deflate_level: u32,
        #[arg(short = 'r', long, value_enum, default_value_t = ResamplingArg::Average)]
        resampling: ResamplingArg,
        #[arg(short = 'o', long, value_delimiter = ' ')]
        overviews: Option<Vec<u32>>,
        #[arg(long)]
        no_overviews: bool,
        #[arg(long)]
        mmap: bool,
        /// Worker threads per file (0 = all CPUs)
        #[arg(long, default_value_t = 0)]
        jobs: usize,
        /// Number of files to convert concurrently (0 = all CPUs)
        #[arg(short = 'j', long, default_value_t = 0)]
        parallel: usize,
        #[arg(long)]
        skip_existing: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FootprintSourceArg {
    Auto,
    Mask,
    Alpha,
    Nodata,
    NonZero,
    Full,
}

impl From<FootprintSourceArg> for ValiditySourceChoice {
    fn from(value: FootprintSourceArg) -> Self {
        match value {
            FootprintSourceArg::Auto => Self::Auto,
            FootprintSourceArg::Mask => Self::Mask,
            FootprintSourceArg::Alpha => Self::Alpha,
            FootprintSourceArg::Nodata => Self::Nodata,
            FootprintSourceArg::NonZero => Self::NonZero,
            FootprintSourceArg::Full => Self::Full,
        }
    }
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
enum LercAdditionalCompressionArg {
    None,
    Deflate,
    Zstd,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ResamplingArg {
    Nearest,
    Average,
    Bilinear,
    Cubic,
    Lanczos,
}

fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Cog { args } => {
            run_cog(&args)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Info { input, mmap } => {
            let input = input.to_string_lossy();
            let started = std::time::Instant::now();
            let info = gather(&input, mmap)?;
            print!("{}", format_text(&info));
            eprintln!(
                "fasttranslate info: read metadata in {:.3}s",
                started.elapsed().as_secs_f64()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Footprint {
            input,
            output,
            source,
            simplify,
            no_mask_from_alpha,
            black_rgb_transparent,
            no_nonzero_in_auto,
            zero_threshold,
            srcwin,
            mmap,
            jobs,
        } => {
            let input_str = input.to_string_lossy();
            let started = std::time::Instant::now();
            let window = if let Some(values) = &srcwin {
                let [col, row, width, height] = values.as_slice() else {
                    anyhow::bail!("--srcwin requires four integers");
                };
                let (img_width, img_height, _) =
                    gdal_alt_core::input_dimensions(&input_str, mmap)?;
                Some(window_from_srcwin(
                    img_width,
                    img_height,
                    *col,
                    *row,
                    *width,
                    *height,
                )?)
            } else {
                None
            };
            let opts = FootprintOptions {
                source: source.into(),
                mask_from_alpha: !no_mask_from_alpha,
                black_rgb_transparent,
                simplify_tolerance: simplify,
                tile_size: 512,
                nonzero_in_auto: !no_nonzero_in_auto,
                zero_threshold,
            };
            let result = extract_footprint(&input_str, mmap, window, &opts, jobs)?;
            if let Some(path) = &output {
                gdal_alt_core::util::ensure_parent_dir(path)?;
                std::fs::write(path, &result.geojson)?;
            } else {
                print!("{}", result.geojson);
            }
            eprintln!(
                "fasttranslate footprint: {} ring(s) from {} validity / {} georef in {:.3}s",
                result.ring_count,
                result.validity_source,
                result.georef_source,
                started.elapsed().as_secs_f64()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Validate { input, mmap } => {
            let started = std::time::Instant::now();
            let report = validate_cog(&input, mmap)?;
            print!("{}", format_report(&report));
            eprintln!(
                "fasttranslate validate: checked in {:.3}s",
                started.elapsed().as_secs_f64()
            );
            if report.is_valid() {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(1))
            }
        }
        Command::Crop {
            input,
            output,
            srcwin,
            projwin,
            blocksize,
            compress,
            deflate_level,
            jpeg_quality,
            lerc_max_z_error,
            lerc_additional_compression,
            resampling,
            overviews,
            no_overviews,
            mmap,
            jobs,
            quiet,
            no_mask_from_alpha,
            black_rgb_transparent,
        } => {
            if srcwin.is_some() && projwin.is_some() {
                anyhow::bail!("use only one of --srcwin or --projwin");
            }
            if srcwin.is_none() && projwin.is_none() {
                anyhow::bail!("specify --srcwin or --projwin");
            }

            let opts = simple_cog_options(
                blocksize,
                compress,
                deflate_level,
                jpeg_quality,
                lerc_max_z_error,
                lerc_additional_compression,
                resampling,
                overviews,
                no_overviews,
                no_mask_from_alpha,
                black_rgb_transparent,
            );
            opts.validate()?;

            let input_str = input.to_string_lossy();
            let (img_width, img_height, _) = gdal_alt_core::input_dimensions(&input_str, mmap)?;
            let window = if let Some(values) = &srcwin {
                let [col, row, width, height] = values.as_slice() else {
                    anyhow::bail!("--srcwin requires four integers");
                };
                window_from_srcwin(img_width, img_height, *col, *row, *width, *height)?
            } else {
                let values = projwin.as_ref().unwrap();
                let [ulx, uly, lrx, lry] = values.as_slice() else {
                    anyhow::bail!("--projwin requires four coordinates");
                };
                let handle = gdal_alt_core::open::open_input(&input_str, mmap)?;
                window_from_projwin(handle.as_file(), *ulx, *uly, *lrx, *lry)?
            };

            let pool = gdal_alt_core::util::thread_pool(jobs)?;
            let started = std::time::Instant::now();
            let show_progress =
                !quiet && std::io::IsTerminal::is_terminal(&std::io::stderr());
            convert_geotiff(
                &pool,
                &ConvertRequest {
                    input: &input_str,
                    output: &output,
                    opts: &opts,
                    mmap,
                    show_progress,
                    window: Some(window),
                    bands: None,
                },
            )?;
            eprintln!(
                "fasttranslate crop: wrote {} in {:.2}s",
                output.display(),
                started.elapsed().as_secs_f64()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Band {
            input,
            output,
            bands,
            blocksize,
            compress,
            deflate_level,
            jpeg_quality,
            lerc_max_z_error,
            lerc_additional_compression,
            resampling,
            overviews,
            no_overviews,
            mmap,
            jobs,
            quiet,
            no_mask_from_alpha,
            black_rgb_transparent,
        } => {
            let opts = simple_cog_options(
                blocksize,
                compress,
                deflate_level,
                jpeg_quality,
                lerc_max_z_error,
                lerc_additional_compression,
                resampling,
                overviews,
                no_overviews,
                no_mask_from_alpha,
                black_rgb_transparent,
            );
            opts.validate()?;

            let input_str = input.to_string_lossy();
            let pool = gdal_alt_core::util::thread_pool(jobs)?;
            let started = std::time::Instant::now();
            let show_progress =
                !quiet && std::io::IsTerminal::is_terminal(&std::io::stderr());
            convert_geotiff(
                &pool,
                &ConvertRequest {
                    input: &input_str,
                    output: &output,
                    opts: &opts,
                    mmap,
                    show_progress,
                    window: None,
                    bands: Some(bands),
                },
            )?;
            eprintln!(
                "fasttranslate band: wrote {} in {:.2}s",
                output.display(),
                started.elapsed().as_secs_f64()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Batch {
            input_dir,
            output_dir,
            blocksize,
            compress,
            deflate_level,
            resampling,
            overviews,
            no_overviews,
            mmap,
            jobs,
            parallel,
            skip_existing,
        } => {
            run_batch(
                &input_dir,
                &output_dir,
                blocksize,
                compress,
                deflate_level,
                resampling,
                overviews,
                no_overviews,
                mmap,
                jobs,
                parallel,
                skip_existing,
            )?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn simple_cog_options(
    blocksize: u32,
    compress: CompressionArg,
    deflate_level: u32,
    jpeg_quality: u8,
    lerc_max_z_error: f64,
    lerc_additional_compression: LercAdditionalCompressionArg,
    resampling: ResamplingArg,
    overviews: Option<Vec<u32>>,
    no_overviews: bool,
    no_mask_from_alpha: bool,
    black_rgb_transparent: bool,
) -> CogOutputOptions {
    CogOutputOptions {
        blocksize,
        compression: match compress {
            CompressionArg::None => CompressionChoice::None,
            CompressionArg::Lzw => CompressionChoice::Lzw,
            CompressionArg::Deflate => CompressionChoice::Deflate,
            CompressionArg::Zstd => CompressionChoice::Zstd,
            CompressionArg::Jpeg => CompressionChoice::Jpeg,
            CompressionArg::Lerc => CompressionChoice::Lerc,
        },
        deflate_level,
        jpeg_quality,
        lerc_max_z_error,
        lerc_additional_compression: match lerc_additional_compression {
            LercAdditionalCompressionArg::None => LercAdditionalCompressionChoice::None,
            LercAdditionalCompressionArg::Deflate => LercAdditionalCompressionChoice::Deflate,
            LercAdditionalCompressionArg::Zstd => LercAdditionalCompressionChoice::Zstd,
        },
        resampling: match resampling {
            ResamplingArg::Nearest => ResamplingChoice::Nearest,
            ResamplingArg::Average => ResamplingChoice::Average,
            ResamplingArg::Bilinear => ResamplingChoice::Bilinear,
            ResamplingArg::Cubic => ResamplingChoice::Cubic,
            ResamplingArg::Lanczos => ResamplingChoice::Lanczos,
        },
        overview_levels: overviews,
        no_overviews,
        mask_from_alpha: !no_mask_from_alpha,
        black_rgb_transparent,
    }
}

fn run_batch(
    input_dir: &Path,
    output_dir: &Path,
    blocksize: u32,
    compress: CompressionArg,
    deflate_level: u32,
    resampling: ResamplingArg,
    overviews: Option<Vec<u32>>,
    no_overviews: bool,
    mmap: bool,
    jobs: usize,
    parallel: usize,
    skip_existing: bool,
) -> Result<()> {
    if !input_dir.is_dir() {
        bail!("input is not a directory: {}", input_dir.display());
    }
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("failed to create {}", output_dir.display()))?;

    let mut inputs = Vec::new();
    for entry in std::fs::read_dir(input_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if is_raster_input(&path) {
            inputs.push(path);
        }
    }
    inputs.sort();

    if inputs.is_empty() {
        bail!("no raster files found in {}", input_dir.display());
    }

    let workers = if parallel == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        parallel.max(1)
    };

    let started = std::time::Instant::now();
    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    rayon::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut in_flight = 0usize;

        for input in inputs {
            let output = output_dir.join(cog_output_name(&input));
            if skip_existing && output.exists() {
                skipped += 1;
                eprintln!("skip existing {}", output.display());
                continue;
            }

            while in_flight >= workers {
                match rx.recv() {
                    Ok(true) => ok += 1,
                    Ok(false) => failed += 1,
                    Err(_) => break,
                }
                in_flight -= 1;
            }

            let tx = tx.clone();
            let args = CogArgs {
                input: input.clone(),
                output: output.clone(),
                blocksize,
                compress: compress.into(),
                deflate_level,
                jpeg_quality: 75,
                lerc_max_z_error: 0.0,
                lerc_additional_compression: fastcog::config::LercAdditionalCompressionArg::None,
                resampling: resampling.into(),
                overviews: overviews.clone(),
                no_overviews,
                mmap,
                jobs,
                quiet: true,
                no_mask_from_alpha: false,
                black_rgb_transparent: false,
                json: false,
            };

            scope.spawn(move |_| {
                let success = run_cog(&args).is_ok();
                if success {
                    eprintln!("ok {}", output.display());
                } else {
                    eprintln!("fail {}", input.display());
                }
                let _ = tx.send(success);
            });
            in_flight += 1;
        }

        for _ in 0..in_flight {
            match rx.recv() {
                Ok(true) => ok += 1,
                Ok(false) => failed += 1,
                Err(_) => break,
            }
        }
    });

    eprintln!(
        "fasttranslate batch: {ok} ok, {skipped} skipped, {failed} failed in {:.2}s",
        started.elapsed().as_secs_f64()
    );
    if failed > 0 {
        bail!("{failed} file(s) failed");
    }
    Ok(())
}

fn is_raster_input(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "tif" | "tiff" | "jp2" | "j2k" | "j2c"
            )
        })
        .unwrap_or(false)
}

fn cog_output_name(input: &Path) -> std::ffi::OsString {
    let mut name = input
        .file_stem()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("output"));
    name.push(".tif");
    name
}

impl From<LercAdditionalCompressionArg> for fastcog::config::LercAdditionalCompressionArg {
    fn from(value: LercAdditionalCompressionArg) -> Self {
        match value {
            LercAdditionalCompressionArg::None => Self::None,
            LercAdditionalCompressionArg::Deflate => Self::Deflate,
            LercAdditionalCompressionArg::Zstd => Self::Zstd,
        }
    }
}

impl From<CompressionArg> for fastcog::config::CompressionArg {
    fn from(value: CompressionArg) -> Self {
        match value {
            CompressionArg::None => Self::None,
            CompressionArg::Lzw => Self::Lzw,
            CompressionArg::Deflate => Self::Deflate,
            CompressionArg::Zstd => Self::Zstd,
            CompressionArg::Jpeg => Self::Jpeg,
            CompressionArg::Lerc => Self::Lerc,
        }
    }
}

impl From<ResamplingArg> for fastcog::config::ResamplingArg {
    fn from(value: ResamplingArg) -> Self {
        match value {
            ResamplingArg::Nearest => Self::Nearest,
            ResamplingArg::Average => Self::Average,
            ResamplingArg::Bilinear => Self::Bilinear,
            ResamplingArg::Cubic => Self::Cubic,
            ResamplingArg::Lanczos => Self::Lanczos,
        }
    }
}
