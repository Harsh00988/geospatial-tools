use anyhow::Result;
use clap::Parser;
use gdal_alt_core::{format_text, gather};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "fastinfo", version, about = "Read raster metadata without decoding pixels")]
struct Args {
    /// Input raster path (GeoTIFF or JP2)
    pub input: PathBuf,

    /// Use memory-mapped reads for local GeoTIFF files
    #[arg(long)]
    pub mmap: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let input = args.input.to_string_lossy();
    let started = std::time::Instant::now();
    let info = gather(&input, args.mmap)?;
    print!("{}", format_text(&info));
    eprintln!(
        "fastinfo: read metadata in {:.3}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
