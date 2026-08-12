use anyhow::Result;
use geotiff_reader::GeoTiffFile;
use tiff_reader::TiffSample;

use crate::cog::{overview_levels, CogOutputOptions};
use crate::crop::WriteWindow;
use crate::encode_overview::encode_layers_with_spool;
use crate::input::RasterProfile;
use crate::progress::ProgressTracker;
use crate::remux::remux_encoded_layers;
use crate::strip_encode::{encode_row_group_total, output_tile_encoding};

pub fn convert_tiled_to_remux_cog<T>(
    pool: &rayon::ThreadPool,
    input: &GeoTiffFile,
    output: &std::path::Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    show_progress: bool,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
{
    let nodata = crate::cog::semantics::parse_nodata::<T>(&profile.sample, &profile.nodata);
    let width = profile.width;
    let height = profile.height;
    let out_bands = profile.bands as usize;
    let tile_size = opts.blocksize as usize;
    let levels = overview_levels(opts, width, height);
    let encoding = output_tile_encoding(opts, tile_size, out_bands as u16);
    let progress = ProgressTracker::new(show_progress);
    let encode_total = encode_row_group_total(width, height, tile_size, &levels);
    let encode_bar = progress.stage("Encode tiles", encode_total);

    let layers = pool.install(|| {
        encode_layers_with_spool::<T>(
            input,
            width,
            height,
            tile_size,
            out_bands,
            window,
            band_map,
            encoding,
            opts,
            &levels,
            nodata,
            Some(&encode_bar),
        )
    })?;

    encode_bar.done("done");
    remux_encoded_layers(
        input,
        profile,
        opts,
        layers,
        output,
        window.as_ref(),
        Some(levels),
        None,
        show_progress,
    )?;
    progress.finish();
    Ok(())
}
