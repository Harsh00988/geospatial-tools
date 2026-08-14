use anyhow::Result;
use geotiff_writer::RemuxCompressedBlock;

use crate::spool::StreamingLayerWriter;

/// Target for parallel out-of-order encoded COG tile blocks.
pub trait StreamingEncodeSink: Sync {
    fn block_count(&self) -> usize;
    fn write_block(&self, index: usize, block: RemuxCompressedBlock) -> Result<()>;
}

impl StreamingEncodeSink for StreamingLayerWriter {
    fn block_count(&self) -> usize {
        self.block_count()
    }

    fn write_block(&self, index: usize, block: RemuxCompressedBlock) -> Result<()> {
        StreamingLayerWriter::write_block(self, index, block)
    }
}

impl StreamingEncodeSink for geotiff_writer::StreamingRgbCogLayerWriter {
    fn block_count(&self) -> usize {
        self.block_count()
    }

    fn write_block(&self, index: usize, block: RemuxCompressedBlock) -> Result<()> {
        self.write_block(index, block)
            .map_err(|err| anyhow::anyhow!(err))
    }
}

/// Abstraction over spool-backed and direct COG streaming layer writers.
pub trait OverviewEncodeSink {
    type LayerWriter: StreamingEncodeSink;

    fn begin_overview_layer(&mut self, block_count: usize) -> Result<Self::LayerWriter>;
    fn commit_overview_layer(&mut self, layer: Self::LayerWriter) -> Result<()>;
}

/// Spool-backed overview sink (mask path and legacy remux).
pub struct LayerBlockSpoolSink<'a>(pub &'a mut crate::spool::LayerBlockSpool);

impl OverviewEncodeSink for LayerBlockSpoolSink<'_> {
    type LayerWriter = StreamingLayerWriter;

    fn begin_overview_layer(&mut self, block_count: usize) -> Result<Self::LayerWriter> {
        self.0.begin_streaming_layer(block_count)
    }

    fn commit_overview_layer(&mut self, layer: Self::LayerWriter) -> Result<()> {
        self.0.commit_streaming_layer(layer)
    }
}

/// Direct COG writer sink (fused encode → output, no spool).
pub struct StreamingCogSink<'a>(pub &'a geotiff_writer::StreamingRgbCogWriter);

impl OverviewEncodeSink for StreamingCogSink<'_> {
    type LayerWriter = geotiff_writer::StreamingRgbCogLayerWriter;

    fn begin_overview_layer(&mut self, block_count: usize) -> Result<Self::LayerWriter> {
        self.0.begin_layer(block_count).map_err(|err| anyhow::anyhow!(err))
    }

    fn commit_overview_layer(&mut self, layer: Self::LayerWriter) -> Result<()> {
        self.0.commit_layer(layer).map_err(|err| anyhow::anyhow!(err))
    }
}
