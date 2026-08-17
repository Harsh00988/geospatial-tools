use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use gdal_alt_core::{
    extract_footprint, window_from_srcwin, FootprintGeorefChoice, FootprintOptions,
    FootprintOutputFormat, ValiditySourceChoice,
};

#[derive(Parser, Debug)]
#[command(
    name = "fastfootprint",
    version,
    about = "Extract the exact valid-pixel footprint as GeoJSON, WKT, or flat coordinates"
)]
struct Args {
    /// Input GeoTIFF or JP2 path
    pub input: PathBuf,

    /// Write output to this file (stdout when omitted)
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// Validity source: auto, mask, alpha, nodata, nonzero, or full
    #[arg(long, value_enum, default_value_t = SourceArg::Auto)]
    pub source: SourceArg,

    /// Output format: geojson, wkt, or flat
    #[arg(long, value_enum, default_value_t = FormatArg::GeoJson)]
    pub format: FormatArg,

    /// Force georef model: auto, affine, rpc, tps, gcp-grid, gcp-affine, pixel
    #[arg(long)]
    pub georef: Option<String>,

    /// Maximum GCP count for TPS fitting (subsample denser grids)
    #[arg(long, default_value_t = 400)]
    pub tps_max_points: usize,

    /// Douglas–Peucker simplification tolerance in map units (0 = none)
    #[arg(long, default_value_t = 0.0)]
    pub simplify: f64,

    /// Douglas–Peucker simplification tolerance in degrees after WGS84 output
    #[arg(long, default_value_t = 0.0)]
    pub simplify_degrees: f64,

    /// Keep only the largest validity ring (drop interior holes / speckle rings)
    #[arg(long)]
    pub outer_only: bool,

    /// Keep all rings for SAR/nonzero validity (disables auto outer-only)
    #[arg(long)]
    pub all_rings: bool,

    /// Constant elevation in meters for RPC georeferencing
    #[arg(long)]
    pub rpc_height: Option<f64>,

    /// GeoTIFF DEM path for RPC height refinement
    #[arg(long)]
    pub dem: Option<PathBuf>,

    /// Do not use associated alpha in auto mode
    #[arg(long)]
    pub no_mask_from_alpha: bool,

    /// Treat RGB(0,0,0) as invalid in auto mode
    #[arg(long)]
    pub black_rgb_transparent: bool,

    /// Do not treat single-band zero pixels as invalid in auto mode
    #[arg(long)]
    pub no_nonzero_in_auto: bool,

    /// Absolute threshold for nonzero validity (default: exact zero)
    #[arg(long, default_value_t = 0.0)]
    pub zero_threshold: f64,

    /// Pixel window: COL ROW WIDTH HEIGHT
    #[arg(long, value_name = "COL ROW WIDTH HEIGHT", num_args = 4)]
    pub srcwin: Option<Vec<usize>>,

    /// Use memory-mapped reads for local GeoTIFF files
    #[arg(long)]
    pub mmap: bool,

    /// Worker threads (0 = all CPUs)
    #[arg(short = 'j', long, default_value_t = 0)]
    pub jobs: usize,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SourceArg {
    Auto,
    Mask,
    Alpha,
    Nodata,
    NonZero,
    Full,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FormatArg {
    GeoJson,
    Wkt,
    Flat,
}

impl From<SourceArg> for ValiditySourceChoice {
    fn from(value: SourceArg) -> Self {
        match value {
            SourceArg::Auto => Self::Auto,
            SourceArg::Mask => Self::Mask,
            SourceArg::Alpha => Self::Alpha,
            SourceArg::Nodata => Self::Nodata,
            SourceArg::NonZero => Self::NonZero,
            SourceArg::Full => Self::Full,
        }
    }
}

impl From<FormatArg> for FootprintOutputFormat {
    fn from(value: FormatArg) -> Self {
        match value {
            FormatArg::GeoJson => Self::GeoJson,
            FormatArg::Wkt => Self::Wkt,
            FormatArg::Flat => Self::WktFlat,
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let input = args.input.to_string_lossy();
    let started = std::time::Instant::now();

    let window = if let Some(values) = &args.srcwin {
        let [col, row, width, height] = values.as_slice() else {
            anyhow::bail!("--srcwin requires four integers");
        };
        let (img_width, img_height, _) = gdal_alt_core::input_dimensions(&input, args.mmap)?;
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

    let georef = match args.georef.as_deref() {
        Some(value) => Some(
            FootprintGeorefChoice::parse(value)
                .ok_or_else(|| anyhow::anyhow!("unknown --georef value: {value}"))?,
        ),
        None => None,
    };

    let opts = FootprintOptions {
        source: args.source.into(),
        mask_from_alpha: !args.no_mask_from_alpha,
        black_rgb_transparent: args.black_rgb_transparent,
        simplify_tolerance: args.simplify,
        tile_size: 512,
        nonzero_in_auto: !args.no_nonzero_in_auto,
        zero_threshold: args.zero_threshold,
        outer_only: args.outer_only,
        all_rings: args.all_rings,
        simplify_degrees: args.simplify_degrees,
        rpc_height: args.rpc_height,
        dem_path: args.dem.as_ref().map(|path| path.to_string_lossy().into_owned()),
        georef,
        tps_max_points: args.tps_max_points,
        output_format: args.format.into(),
    };

    let result = extract_footprint(&input, args.mmap, window, &opts, args.jobs)?;
    if let Some(path) = &args.output {
        gdal_alt_core::util::ensure_parent_dir(path)?;
        std::fs::write(path, &result.body)?;
    } else {
        print!("{}", result.body);
    }

    eprintln!(
        "fastfootprint: {} ring(s), {} vertices, {} validity, {} georef in {:.3}s",
        result.ring_count,
        result.vertex_count,
        result.validity_source,
        result.georef_source,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
