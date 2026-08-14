use anyhow::Result;
use geotiff_reader::GeoTiffFile;
use tiff_reader::TiffSample;

use crate::cog::{configure_cog, overview_levels, CogOutputOptions};
use crate::crop::WriteWindow;
use crate::encode::mask_stream::{
    encode_with_streaming_masks, mask_streaming_plan_for_encode, open_streaming_masked_writer,
};
use crate::encode::overview::{encode_layers_with_spool, encode_overview_layers};
use crate::encode::sink::StreamingCogSink;
use crate::encode::strip::{encode_row_group_total, output_tile_encoding};
use crate::input::RasterProfile;
use crate::progress::ProgressTracker;
use crate::remux::{encode_output_needs_mask_remux, remux_encoded_layers_from_spool};

/// Encode a GeoTIFF (strip or tiled input) into a remux COG.
pub fn convert_to_remux_cog<T>(
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
    let encoding = output_tile_encoding(opts, tile_size, out_bands as u16, profile.sample.sample_format);
    let progress = ProgressTracker::new(show_progress);
    let encode_total = encode_row_group_total(width, height, tile_size, &levels);
    let encode_bar = progress.stage("Encode tiles", encode_total);

    if encode_output_needs_mask_remux(input, profile, opts) {
        if let Some(plan) = mask_streaming_plan_for_encode(
            input,
            profile,
            opts,
            window,
            width,
            height,
            &levels,
        )? {
            let stream =
                open_streaming_masked_writer::<T>(profile, opts, output, width, height, &levels, &plan)?;
            pool.install(|| {
                encode_with_streaming_masks::<T>(
                    input,
                    &stream,
                    profile,
                    &plan,
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
            stream.finish().map_err(|err| anyhow::anyhow!(err))?;
        } else {
            let spool = pool.install(|| {
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
            remux_encoded_layers_from_spool(
                input,
                profile,
                opts,
                spool,
                output,
                window.as_ref(),
                Some(levels),
                None,
                show_progress,
            )?;
        }
    } else {
        let cog = configure_cog(profile.base_builder(opts), opts, width, height);
        let stream = cog.open_streaming_rgb_writer::<T, _>(output, 1 + levels.len())?;
        pool.install(|| {
            let mut sink = StreamingCogSink(&stream);
            encode_overview_layers::<T, _>(
                input,
                &mut sink,
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
        stream.finish().map_err(|err| anyhow::anyhow!(err))?;
    }

    progress.finish();
    Ok(())
}
