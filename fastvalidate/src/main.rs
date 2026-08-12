use anyhow::Result;
use clap::Parser;
use gdal_alt_core::{format_report, validate_cog};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(name = "fastvalidate", version, about = "Validate COG layout (tiled, overviews, compression)")]
struct Args {
    /// Input COG path
    pub input: PathBuf,

    /// Use memory-mapped reads
    #[arg(long)]
    pub mmap: bool,
}

fn main() -> Result<ExitCode> {
    let args = Args::parse();
    let started = std::time::Instant::now();
    let report = validate_cog(&args.input, args.mmap)?;
    print!("{}", format_report(&report));
    eprintln!(
        "fastvalidate: checked in {:.3}s",
        started.elapsed().as_secs_f64()
    );
    if report.is_valid() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}
