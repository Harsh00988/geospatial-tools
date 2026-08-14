use anyhow::Context;
use ndarray::{Array2, Array3};
use tiff_reader::TiffSample;
use tempfile::tempfile;

use super::tile::{DecodedTileSpool, StripTile};
use super::tiles::clone_strip_tile;

pub(super) enum DecodedTileStorage<T> {
    Memory(std::collections::HashMap<(usize, usize), StripTile<T>>),
    Disk {
        file: std::fs::File,
        index: std::collections::HashMap<(usize, usize), u64>,
    },
}

const DECODED_TILE_MEMORY_LIMIT: usize = 256 * 1024 * 1024;

impl<T> DecodedTileSpool<T>
where
    T: TiffSample + Copy + Default,
{
    #[cfg(test)]
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(DecodedTileStorage::Disk {
                file: tempfile()?,
                index: std::collections::HashMap::new(),
            })),
            _marker: std::marker::PhantomData,
        })
    }

    pub fn new_for_layer(
        width: u32,
        height: u32,
        level: u32,
        tile_size: usize,
        out_bands: usize,
    ) -> anyhow::Result<Self> {
        let ov_w = (width / level).max(1) as usize;
        let ov_h = (height / level).max(1) as usize;
        let tile_count = ov_w.div_ceil(tile_size) * ov_h.div_ceil(tile_size);
        let bytes_per_tile = tile_size
            .saturating_mul(tile_size)
            .saturating_mul(out_bands)
            .saturating_mul(std::mem::size_of::<T>());
        let estimated = tile_count.saturating_mul(bytes_per_tile);
        let storage = if estimated <= DECODED_TILE_MEMORY_LIMIT {
            DecodedTileStorage::Memory(std::collections::HashMap::new())
        } else {
            DecodedTileStorage::Disk {
                file: tempfile()?,
                index: std::collections::HashMap::new(),
            }
        };
        Ok(Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(storage)),
            _marker: std::marker::PhantomData,
        })
    }

    pub fn insert(&self, col: usize, row: usize, tile: &StripTile<T>) -> anyhow::Result<()> {
        use std::io::Seek;
        let mut inner = self.inner.lock().expect("decoded tile spool lock");
        match &mut *inner {
            DecodedTileStorage::Memory(tiles) => {
                tiles.insert((col, row), clone_strip_tile(tile));
                Ok(())
            }
            DecodedTileStorage::Disk { file, index } => {
                let offset = file.stream_position()?;
                spool_write_tile(file, tile)?;
                index.insert((col, row), offset);
                Ok(())
            }
        }
    }

    #[cfg(test)]
    pub fn get(&self, col: usize, row: usize) -> anyhow::Result<Option<StripTile<T>>> {
        self.try_get(col, row)
    }

    pub fn try_get(&self, col: usize, row: usize) -> anyhow::Result<Option<StripTile<T>>> {
        use std::io::Seek;
        let mut inner = self.inner.lock().expect("decoded tile spool lock");
        match &mut *inner {
            DecodedTileStorage::Memory(tiles) => Ok(tiles.get(&(col, row)).map(clone_strip_tile)),
            DecodedTileStorage::Disk { file, index } => {
                let offset = match index.get(&(col, row)) {
                    Some(&offset) => offset,
                    None => return Ok(None),
                };
                file.seek(std::io::SeekFrom::Start(offset))?;
                spool_read_tile(file)
            }
        }
    }
}

impl<T> Clone for DecodedTileSpool<T> {
    fn clone(&self) -> Self {
        Self {
            inner: std::sync::Arc::clone(&self.inner),
            _marker: std::marker::PhantomData,
        }
    }
}

fn spool_write_tile<T: TiffSample + Copy>(
    file: &mut std::fs::File,
    tile: &StripTile<T>,
) -> anyhow::Result<()> {
    use std::io::Write;
    match tile {
        StripTile::Single(data) => {
            file.write_all(&[0u8])?;
            file.write_all(&(data.nrows() as u32).to_le_bytes())?;
            file.write_all(&(data.ncols() as u32).to_le_bytes())?;
            spool_write_slice(file, data.as_slice().context("contiguous single-band tile")?)?;
        }
        StripTile::Multi(data) => {
            file.write_all(&[1u8])?;
            file.write_all(&(data.shape()[0] as u32).to_le_bytes())?;
            file.write_all(&(data.shape()[1] as u32).to_le_bytes())?;
            file.write_all(&(data.shape()[2] as u16).to_le_bytes())?;
            spool_write_slice(file, data.as_slice().context("contiguous multi-band tile")?)?;
        }
    }
    Ok(())
}

fn spool_read_tile<T: TiffSample + Copy + Default>(
    file: &mut std::fs::File,
) -> anyhow::Result<Option<StripTile<T>>> {
    use std::io::Read;
    let mut tag = [0u8; 1];
    if file.read_exact(&mut tag).is_err() {
        return Ok(None);
    }
    let rows = spool_read_u32(file)? as usize;
    let cols = spool_read_u32(file)? as usize;
    match tag[0] {
        0 => Ok(Some(StripTile::Single(spool_read_array2(file, rows, cols)?))),
        1 => {
            let bands = spool_read_u16(file)? as usize;
            Ok(Some(StripTile::Multi(spool_read_array3(
                file, rows, cols, bands,
            )?)))
        }
        _ => anyhow::bail!("invalid decoded tile spool tag {}", tag[0]),
    }
}

fn spool_write_slice<T: TiffSample + Copy>(
    file: &mut std::fs::File,
    data: &[T],
) -> anyhow::Result<()> {
    use std::io::Write;
    let bytes = unsafe {
        std::slice::from_raw_parts(
            data.as_ptr().cast::<u8>(),
            data.len() * std::mem::size_of::<T>(),
        )
    };
    file.write_all(bytes)?;
    Ok(())
}

fn spool_read_array2<T: TiffSample + Copy + Default>(
    file: &mut std::fs::File,
    rows: usize,
    cols: usize,
) -> anyhow::Result<Array2<T>> {
    use std::io::Read;
    let len = rows
        .checked_mul(cols)
        .context("tile element count overflow")?;
    let mut data = vec![T::default(); len];
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            data.as_mut_ptr().cast::<u8>(),
            len * std::mem::size_of::<T>(),
        )
    };
    file.read_exact(bytes)?;
    Array2::from_shape_vec((rows, cols), data).context("invalid 2D tile shape")
}

fn spool_read_array3<T: TiffSample + Copy + Default>(
    file: &mut std::fs::File,
    rows: usize,
    cols: usize,
    bands: usize,
) -> anyhow::Result<Array3<T>> {
    use std::io::Read;
    let len = rows
        .checked_mul(cols)
        .and_then(|v| v.checked_mul(bands))
        .context("tile element count overflow")?;
    let mut data = vec![T::default(); len];
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            data.as_mut_ptr().cast::<u8>(),
            len * std::mem::size_of::<T>(),
        )
    };
    file.read_exact(bytes)?;
    Array3::from_shape_vec((rows, cols, bands), data).context("invalid 3D tile shape")
}

fn spool_read_u32(file: &mut std::fs::File) -> anyhow::Result<u32> {
    use std::io::Read;
    let mut buf = [0u8; 4];
    file.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

fn spool_read_u16(file: &mut std::fs::File) -> anyhow::Result<u16> {
    use std::io::Read;
    let mut buf = [0u8; 2];
    file.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}
