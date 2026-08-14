use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{bail, Context, Result};
use geotiff_writer::RemuxCompressedBlock;
use tempfile::tempfile;

/// Spill encoded COG layers to disk so only one layer's blocks live in RAM at a time.
pub struct LayerBlockSpool {
    file: Arc<Mutex<File>>,
    layer_count: u32,
}

impl LayerBlockSpool {
    pub fn new() -> Result<Self> {
        Ok(Self {
            file: Arc::new(Mutex::new(tempfile()?)),
            layer_count: 0,
        })
    }

    pub fn layer_count(&self) -> usize {
        self.layer_count as usize
    }

    /// Begin a layer that will receive blocks out-of-order from parallel encoders.
    pub fn begin_streaming_layer(&mut self, block_count: usize) -> Result<StreamingLayerWriter> {
        let count = u32::try_from(block_count).context("layer block count overflow")?;
        self.file
            .lock()
            .expect("spool file lock")
            .write_all(&count.to_le_bytes())?;
        StreamingLayerWriter::start(Arc::clone(&self.file), block_count)
    }

    /// Finalize a streaming layer into the spool file.
    pub fn commit_streaming_layer(&mut self, layer: StreamingLayerWriter) -> Result<()> {
        layer.finish()?;
        self.layer_count = self
            .layer_count
            .checked_add(1)
            .context("spool layer count overflow")?;
        Ok(())
    }

    pub fn write_layer(&mut self, blocks: Vec<RemuxCompressedBlock>) -> Result<()> {
        let count = u32::try_from(blocks.len()).context("layer block count overflow")?;
        let mut file = self.file.lock().expect("spool file lock");
        file.write_all(&count.to_le_bytes())?;
        for block in blocks {
            Self::write_block_record(&mut file, &block)?;
        }
        self.layer_count = self
            .layer_count
            .checked_add(1)
            .context("spool layer count overflow")?;
        Ok(())
    }

    pub(crate) fn write_block_record(file: &mut File, block: &RemuxCompressedBlock) -> Result<()> {
        let sparse = u8::from(block.sparse);
        file.write_all(&[sparse])?;
        let len = u32::try_from(block.payload.len()).context("block payload length overflow")?;
        file.write_all(&len.to_le_bytes())?;
        if !block.sparse {
            file.write_all(&block.payload)?;
        }
        Ok(())
    }

    fn read_one_layer(file: &mut File) -> Result<Option<Vec<RemuxCompressedBlock>>> {
        let mut count_buf = [0u8; 4];
        match file.read_exact(&mut count_buf) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(err.into()),
        }
        let count = u32::from_le_bytes(count_buf) as usize;
        let mut blocks = Vec::with_capacity(count);
        for _ in 0..count {
            blocks.push(Self::read_one_block(file)?);
        }
        Ok(Some(blocks))
    }

    /// Read the next encoded layer without loading prior layers into memory.
    pub fn read_next_layer(&mut self) -> Result<Option<Vec<RemuxCompressedBlock>>> {
        let mut file = self.file.lock().expect("spool file lock");
        Self::read_one_layer(&mut file)
    }

    /// Sequential block reader for remux without loading whole layers into RAM.
    pub fn block_reader(&self) -> LayerBlockReader {
        LayerBlockReader {
            file: Arc::clone(&self.file),
            blocks_left: 0,
            layer_open: false,
        }
    }

    pub fn rewind(&mut self) -> Result<()> {
        self.file
            .lock()
            .expect("spool file lock")
            .seek(SeekFrom::Start(0))?;
        Ok(())
    }

    pub fn read_all_layers(mut self) -> Result<Vec<Vec<RemuxCompressedBlock>>> {
        self.rewind()?;
        let mut layers = Vec::with_capacity(self.layer_count());
        while let Some(layer) = self.read_next_layer()? {
            layers.push(layer);
        }
        Ok(layers)
    }

    pub(crate) fn read_one_block(file: &mut File) -> Result<RemuxCompressedBlock> {
        let mut flag = [0u8; 1];
        file.read_exact(&mut flag)?;
        let sparse = flag[0] != 0;
        let mut len_buf = [0u8; 4];
        file.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 {
            file.read_exact(&mut payload)?;
        }
        Ok(RemuxCompressedBlock { payload, sparse })
    }
}

type BlockMsg = Result<usize>;

struct PendingBlockStore {
    file: File,
    offsets: BTreeMap<usize, u64>,
}

impl PendingBlockStore {
    fn new() -> Result<Self> {
        Ok(Self {
            file: tempfile()?,
            offsets: BTreeMap::new(),
        })
    }

    fn insert(&mut self, index: usize, block: &RemuxCompressedBlock) -> Result<()> {
        if self.offsets.contains_key(&index) {
            bail!("duplicate block index {index}");
        }
        let offset = self.file.stream_position()?;
        LayerBlockSpool::write_block_record(&mut self.file, block)?;
        self.offsets.insert(index, offset);
        Ok(())
    }

    fn remove(&mut self, index: usize) -> Result<RemuxCompressedBlock> {
        let offset = self
            .offsets
            .remove(&index)
            .with_context(|| format!("missing pending block index {index}"))?;
        self.file.seek(SeekFrom::Start(offset))?;
        let block = LayerBlockSpool::read_one_block(&mut self.file)?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(block)
    }
}

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

/// Parallel-safe writer that orders blocks and writes once directly into the spool file.
pub struct StreamingLayerWriter {
    tx: Option<Sender<BlockMsg>>,
    pending: Arc<Mutex<PendingBlockStore>>,
    handle: Option<JoinHandle<Result<()>>>,
    block_count: usize,
}

impl StreamingLayerWriter {
    fn start(file: Arc<Mutex<File>>, block_count: usize) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<BlockMsg>();
        let pending = Arc::new(Mutex::new(PendingBlockStore::new()?));
        let pending_for_thread = Arc::clone(&pending);
        let handle = std::thread::spawn(move || {
            let mut file = file.lock().expect("spool file lock");
            let mut next = 0usize;

            fn drain_ready(
                pending: &Arc<Mutex<PendingBlockStore>>,
                file: &mut File,
                next: &mut usize,
            ) -> Result<()> {
                while {
                    let pending = pending.lock().expect("pending block store lock");
                    pending.offsets.contains_key(&*next)
                } {
                    let mut pending = pending.lock().expect("pending block store lock");
                    let block = pending.remove(*next)?;
                    LayerBlockSpool::write_block_record(file, &block)?;
                    *next += 1;
                }
                Ok(())
            }

            for msg in rx {
                let index = msg?;
                if index >= block_count {
                    bail!("block index {index} out of range (layer has {block_count} blocks)");
                }
                drain_ready(&pending_for_thread, &mut file, &mut next)?;
            }
            drain_ready(&pending_for_thread, &mut file, &mut next)?;
            if next != block_count {
                let pending_len = pending_for_thread
                    .lock()
                    .expect("pending block store lock")
                    .offsets
                    .len();
                bail!(
                    "streaming layer expected {block_count} blocks, got {next} ({pending_len} pending)"
                );
            }
            Ok(())
        });
        Ok(Self {
            tx: Some(tx),
            pending,
            handle: Some(handle),
            block_count,
        })
    }

    pub fn block_count(&self) -> usize {
        self.block_count
    }

    pub fn write_block(&self, index: usize, block: RemuxCompressedBlock) -> Result<()> {
        if index >= self.block_count {
            bail!(
                "block index {index} out of range (layer has {} blocks)",
                self.block_count
            );
        }
        {
            let mut pending = self.pending.lock().expect("pending block store lock");
            pending.insert(index, &block)?;
        }
        self.tx
            .as_ref()
            .expect("streaming layer writer closed")
            .send(Ok(index))
            .map_err(|_| anyhow::anyhow!("streaming layer writer thread stopped"))?;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.tx.take();
        let result = self
            .handle
            .take()
            .expect("streaming layer writer thread")
            .join()
            .map_err(|_| anyhow::anyhow!("streaming layer writer thread panicked"))?;
        result
    }
}

/// Reads one encoded block at a time from a [`LayerBlockSpool`].
pub struct LayerBlockReader {
    file: Arc<Mutex<File>>,
    blocks_left: usize,
    layer_open: bool,
}

impl LayerBlockReader {
    pub fn begin_layer(&mut self) -> Result<usize> {
        if self.layer_open {
            anyhow::bail!("previous layer still open");
        }
        let mut file = self.file.lock().expect("spool file lock");
        let mut count_buf = [0u8; 4];
        match file.read_exact(&mut count_buf) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                anyhow::bail!("unexpected end of spool while opening layer");
            }
            Err(err) => return Err(err.into()),
        }
        self.blocks_left = u32::from_le_bytes(count_buf) as usize;
        self.layer_open = true;
        Ok(self.blocks_left)
    }

    pub fn read_block(&mut self) -> Result<Option<RemuxCompressedBlock>> {
        if !self.layer_open {
            return Ok(None);
        }
        if self.blocks_left == 0 {
            self.layer_open = false;
            return Ok(None);
        }
        let mut file = self.file.lock().expect("spool file lock");
        let block = LayerBlockSpool::read_one_block(&mut file)?;
        self.blocks_left -= 1;
        if self.blocks_left == 0 {
            self.layer_open = false;
        }
        Ok(Some(block))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_reader_roundtrips_layers() {
        let mut spool = LayerBlockSpool::new().unwrap();
        spool
            .write_layer(vec![
                RemuxCompressedBlock {
                    payload: vec![1, 2, 3],
                    sparse: false,
                },
                RemuxCompressedBlock {
                    payload: Vec::new(),
                    sparse: true,
                },
            ])
            .unwrap();
        spool
            .write_layer(vec![RemuxCompressedBlock {
                payload: vec![9],
                sparse: false,
            }])
            .unwrap();

        spool.rewind().unwrap();
        let mut reader = spool.block_reader();
        assert_eq!(reader.begin_layer().unwrap(), 2);
        let b0 = reader.read_block().unwrap().unwrap();
        assert_eq!(b0.payload, vec![1, 2, 3]);
        let b1 = reader.read_block().unwrap().unwrap();
        assert!(b1.sparse);
        assert!(reader.read_block().unwrap().is_none());

        assert_eq!(reader.begin_layer().unwrap(), 1);
        let b2 = reader.read_block().unwrap().unwrap();
        assert_eq!(b2.payload, vec![9]);
        assert!(reader.read_block().unwrap().is_none());
    }

    #[test]
    fn streaming_layer_writes_out_of_order() {
        let mut spool = LayerBlockSpool::new().unwrap();
        let writer = spool.begin_streaming_layer(3).unwrap();
        writer
            .write_block(
                2,
                RemuxCompressedBlock {
                    payload: vec![3],
                    sparse: false,
                },
            )
            .unwrap();
        writer
            .write_block(
                0,
                RemuxCompressedBlock {
                    payload: vec![1],
                    sparse: false,
                },
            )
            .unwrap();
        writer
            .write_block(
                1,
                RemuxCompressedBlock {
                    payload: Vec::new(),
                    sparse: true,
                },
            )
            .unwrap();
        spool.commit_streaming_layer(writer).unwrap();

        spool.rewind().unwrap();
        let layer = spool.read_next_layer().unwrap().unwrap();
        assert_eq!(layer.len(), 3);
        assert_eq!(layer[0].payload, vec![1]);
        assert!(layer[1].sparse);
        assert_eq!(layer[2].payload, vec![3]);
    }
}
