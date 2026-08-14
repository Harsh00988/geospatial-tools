use anyhow::Result;
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{RemuxCompressedBlock, RemuxTileEncoding, StreamingRgbCogWriter};
use tiff_reader::TiffSample;

use crate::cog::mask::{
    encode_mask_streaming_layer, resolve_mask_streaming_plan, streaming_mask_layer_specs,
    MaskStreamingPlan,
};
use crate::cog::{configure_cog, CogOutputOptions};
use crate::crop::WriteWindow;
use crate::encode::overview::encode_overview_layers;
use crate::encode::sink::OverviewEncodeSink;
use crate::input::RasterProfile;
use geotiff_writer::OverviewStorage;

/// Encode RGB + mask layers directly into the output COG (no spool / `read_all_layers`).
pub(crate) fn encode_with_streaming_masks<T>(
    input: &GeoTiffFile,
    writer: &StreamingRgbCogWriter,
    profile: &RasterProfile,
    plan: &MaskStreamingPlan,
    width: u32,
    height: u32,
    tile_size: usize,
    out_bands: usize,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
    encoding: RemuxTileEncoding,
    opts: &CogOutputOptions,
    levels: &[u32],
    nodata: Option<T>,
    progress: Option<&crate::progress::StageBar>,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync + PartialEq,
{
    let mut sink = StreamingMaskedCogSink {
        writer,
        input,
        profile,
        plan,
        width,
        height,
        levels,
        opts,
        rgb_committed: 0,
    };
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
        levels,
        nodata,
        progress,
    )?;

    for mask_index in 1..plan.descriptors.len() {
        sink.commit_mask_layer(mask_index)?;
    }
    Ok(())
}

struct StreamingMaskedCogSink<'a> {
    writer: &'a StreamingRgbCogWriter,
    input: &'a GeoTiffFile,
    profile: &'a RasterProfile,
    plan: &'a MaskStreamingPlan,
    width: u32,
    height: u32,
    levels: &'a [u32],
    opts: &'a CogOutputOptions,
    rgb_committed: usize,
}

impl StreamingMaskedCogSink<'_> {
    fn commit_mask_layer(&mut self, mask_index: usize) -> Result<()> {
        let blocks = encode_mask_streaming_layer(
            self.input,
            self.profile,
            self.plan,
            mask_index,
            self.width,
            self.height,
            self.levels,
            self.opts,
        )?;
        commit_mask_blocks(self.writer, blocks)
    }
}

impl OverviewEncodeSink for StreamingMaskedCogSink<'_> {
    type LayerWriter = geotiff_writer::StreamingRgbCogLayerWriter;

    fn begin_overview_layer(&mut self, block_count: usize) -> Result<Self::LayerWriter> {
        self.writer
            .begin_layer(block_count)
            .map_err(|err| anyhow::anyhow!(err))
    }

    fn commit_overview_layer(&mut self, layer: Self::LayerWriter) -> Result<()> {
        self.writer
            .commit_layer(layer)
            .map_err(|err| anyhow::anyhow!(err))?;
        self.rgb_committed += 1;
        if self.rgb_committed == 1 {
            self.commit_mask_layer(0)?;
        }
        Ok(())
    }
}

fn commit_mask_blocks(
    writer: &StreamingRgbCogWriter,
    blocks: Vec<RemuxCompressedBlock>,
) -> Result<()> {
    let block_count = blocks.len();
    let layer_writer = writer
        .begin_layer(block_count)
        .map_err(|err| anyhow::anyhow!(err))?;
    for (index, block) in blocks.into_iter().enumerate() {
        layer_writer
            .write_block(index, block)
            .map_err(|err| anyhow::anyhow!(err))?;
    }
    writer
        .commit_layer(layer_writer)
        .map_err(|err| anyhow::anyhow!(err))
}

/// Build a streaming COG writer for an RGB+mask encode plan.
pub(crate) fn open_streaming_masked_writer<T>(
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    output: &std::path::Path,
    width: u32,
    height: u32,
    levels: &[u32],
    plan: &MaskStreamingPlan,
) -> Result<StreamingRgbCogWriter>
where
    T: geotiff_writer::NumericSample,
{
    let rgb_layer_count = 1 + levels.len();
    let specs = streaming_mask_layer_specs(rgb_layer_count, &plan.descriptors);
    let cog = configure_cog(profile.base_builder(opts), opts, width, height)
        .overview_storage(OverviewStorage::TopLevelIfds);
    cog.open_streaming_cog_writer::<T, _>(output, &specs)
        .map_err(|err| anyhow::anyhow!(err))
}

/// Resolve a mask streaming plan when masks are required for this encode.
pub(crate) fn mask_streaming_plan_for_encode(
    input: &GeoTiffFile,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    window: Option<WriteWindow>,
    width: u32,
    height: u32,
    levels: &[u32],
) -> Result<Option<MaskStreamingPlan>> {
    resolve_mask_streaming_plan(
        input,
        profile,
        opts,
        window,
        width,
        height,
        levels,
        1 + levels.len(),
    )
}