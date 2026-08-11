use crate::config::Args;
use crate::geotiff;
use crate::input::{detect, InputFormat};
use crate::jp2;
use crate::util;
use anyhow::Result;

pub fn run(args: &Args) -> Result<()> {
    args.validate()?;

    let pool = util::thread_pool(args.jobs)?;
    let started = std::time::Instant::now();

    match detect(&args.input) {
        InputFormat::Jp2 => jp2::convert(args, &pool)?,
        InputFormat::GeoTiff => geotiff::convert(args, &pool)?,
    }

    eprintln!(
        "fastcog: wrote {} in {:.2}s",
        args.output.display(),
        started.elapsed().as_secs_f64()
    );

    Ok(())
}
