use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use anyhow::{Context, Result};
use geotiff_writer::RemuxCompressedBlock;
use tempfile::tempfile;

/// Spill encoded COG layers to disk so only one layer's blocks live in RAM at a time.
pub struct LayerBlockSpool {
    file: File,
}

impl LayerBlockSpool {
    pub fn new() -> Result<Self> {
        Ok(Self { file: tempfile()? })
    }

    pub fn write_layer(&mut self, blocks: Vec<RemuxCompressedBlock>) -> Result<()> {
        let count = u32::try_from(blocks.len()).context("layer block count overflow")?;
        self.file.write_all(&count.to_le_bytes())?;
        for block in blocks {
            let sparse = u8::from(block.sparse);
            self.file.write_all(&[sparse])?;
            let len = u32::try_from(block.payload.len()).context("block payload length overflow")?;
            self.file.write_all(&len.to_le_bytes())?;
            if !block.sparse {
                self.file.write_all(&block.payload)?;
            }
        }
        Ok(())
    }

    pub fn read_all_layers(self) -> Result<Vec<Vec<RemuxCompressedBlock>>> {
        let mut file = self.file;
        file.seek(SeekFrom::Start(0))?;
        let mut layers = Vec::new();
        loop {
            let mut count_buf = [0u8; 4];
            match file.read_exact(&mut count_buf) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(err) => return Err(err.into()),
            }
            let count = u32::from_le_bytes(count_buf) as usize;
            let mut blocks = Vec::with_capacity(count);
            for _ in 0..count {
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
                blocks.push(RemuxCompressedBlock { payload, sparse });
            }
            layers.push(blocks);
        }
        Ok(layers)
    }
}
