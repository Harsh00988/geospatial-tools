use crate::config::Args;
use crate::jp2;
use gdal_alt_core::{convert_geotiff, detect, ConvertRequest, InputFormat};
use gdal_alt_core::util;
use anyhow::Result;

pub fn run(args: &Args) -> Result<()> {
    args.validate()?;

    let pool = util::thread_pool(args.jobs)?;
    let started = std::time::Instant::now();
    let opts = args.cog_options();

    match detect(&args.input) {
        InputFormat::Jp2 => jp2::convert(args, &pool)?,
        InputFormat::GeoTiff => convert_geotiff(
            &pool,
            &ConvertRequest {
                input: &args.input,
                output: &args.output,
                opts: &opts,
                mmap: args.mmap,
                show_progress: args.show_progress(),
                window: None,
                bands: None,
            },
        )?,
    }

    eprintln!(
        "fastcog: wrote {} in {:.2}s",
        args.output.display(),
        started.elapsed().as_secs_f64()
    );

    Ok(())
}
