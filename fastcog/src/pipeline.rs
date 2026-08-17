use crate::config::Args;
use anyhow::Result;
use gdal_alt_core::util;
use gdal_alt_core::{
    convert_geotiff, print_json, ConvertPath, ConvertRequest, ConvertStats,
};

pub fn run(args: &Args) -> Result<()> {
    args.validate()?;

    let pool = util::thread_pool(args.jobs)?;
    let started = std::time::Instant::now();
    let opts = args.cog_options();
    let input = args.input.to_string_lossy();
    let mmap = args.mmap || auto_mmap_input(&args.input);

    let result: Result<ConvertPath, anyhow::Error> = convert_geotiff(
        &pool,
        &ConvertRequest {
            input: &input,
            output: &args.output,
            opts: &opts,
            mmap,
            show_progress: args.show_progress(),
            window: None,
            bands: None,
        },
    )
    .map(|convert| convert.path);

    let elapsed = started.elapsed().as_secs_f64();
    match result {
        Ok(path) => {
            if args.json {
                print_json(&ConvertStats::success(
                    input.as_ref(),
                    args.output.display().to_string(),
                    path,
                    elapsed,
                ))?;
            } else {
                eprintln!(
                    "fastcog: wrote {} in {:.2}s (path: {path})",
                    args.output.display(),
                    elapsed
                );
            }
            Ok(())
        }
        Err(err) => {
            if args.json {
                print_json(&ConvertStats::failure(
                    input.as_ref(),
                    args.output.display().to_string(),
                    ConvertPath::StripEncode,
                    elapsed,
                    err.to_string(),
                ))?;
            }
            Err(err)
        }
    }
}

/// Memory-map large local inputs to reduce read syscall overhead.
fn auto_mmap_input(path: &std::path::Path) -> bool {
    const THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() >= THRESHOLD_BYTES)
        .unwrap_or(false)
}
