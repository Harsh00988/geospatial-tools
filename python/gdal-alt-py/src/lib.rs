use std::path::Path;

use gdal_alt_core::{
    convert_geotiff, format_text, gather, validate_cog, CogOutputOptions, CompressionChoice,
    ConvertRequest, LercAdditionalCompressionChoice, ResamplingChoice,
};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

#[pyfunction]
#[pyo3(signature = (input, output, *, blocksize=512, compression="deflate", deflate_level=6, resampling="average", mmap=false, jobs=0))]
fn convert_to_cog(
    input: &str,
    output: &str,
    blocksize: u32,
    compression: &str,
    deflate_level: u32,
    resampling: &str,
    mmap: bool,
    jobs: usize,
) -> PyResult<String> {
    let opts = CogOutputOptions {
        blocksize,
        compression: parse_compression(compression)?,
        deflate_level,
        resampling: parse_resampling(resampling)?,
        overview_levels: None,
        no_overviews: false,
        mask_from_alpha: true,
        black_rgb_transparent: false,
        jpeg_quality: 75,
        lerc_max_z_error: 0.0,
        lerc_additional_compression: LercAdditionalCompressionChoice::None,
    };
    opts.validate()
        .map_err(|err| PyValueError::new_err(err.to_string()))?;

    let pool = gdal_alt_core::util::thread_pool(jobs)
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
    let result = convert_geotiff(
        &pool,
        &ConvertRequest {
            input,
            output: Path::new(output),
            opts: &opts,
            mmap,
            show_progress: false,
            window: None,
            bands: None,
        },
    )
    .map_err(|err| PyValueError::new_err(err.to_string()))?;

    Ok(result.path.to_string())
}

#[pyfunction]
#[pyo3(signature = (input, *, mmap=false))]
fn info(input: &str, mmap: bool) -> PyResult<String> {
    let raster = gather(input, mmap).map_err(|err| PyValueError::new_err(err.to_string()))?;
    Ok(format_text(&raster))
}

#[pyfunction]
#[pyo3(signature = (input, *, mmap=false))]
fn validate(input: &str, mmap: bool) -> PyResult<bool> {
    let report = validate_cog(Path::new(input), mmap)
        .map_err(|err| PyValueError::new_err(err.to_string()))?;
    Ok(report.is_valid())
}

#[pymodule]
fn gdal_alt(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(convert_to_cog, m)?)?;
    m.add_function(wrap_pyfunction!(info, m)?)?;
    m.add_function(wrap_pyfunction!(validate, m)?)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

fn parse_compression(name: &str) -> PyResult<CompressionChoice> {
    CompressionChoice::from_name(name).map_err(|err| PyValueError::new_err(err.to_string()))
}

fn parse_resampling(name: &str) -> PyResult<ResamplingChoice> {
    ResamplingChoice::from_name(name).map_err(|err| PyValueError::new_err(err.to_string()))
}
