use anyhow::{bail, Context, Result};
use jpeg2k::DumpImage;
use tiff_core::PhotometricInterpretation;

#[derive(Debug, Clone, Copy)]
pub struct Jp2Raster {
    pub width: u32,
    pub height: u32,
    pub bands: u32,
    pub bits_per_sample: u8,
    pub photometric: PhotometricInterpretation,
}

impl Jp2Raster {
    pub fn open(data: &[u8]) -> Result<Self> {
        let dump = DumpImage::from_bytes(data).context("failed to read JP2 header")?;
        let width = dump.img.orig_width();
        let height = dump.img.orig_height();
        let bands = dump.img.num_components();
        let bits_per_sample = dump
            .img
            .components()
            .first()
            .map(|component| component.precision() as u8)
            .unwrap_or(8);

        let photometric = match bands {
            3 => PhotometricInterpretation::Rgb,
            _ => bail!(
                "JP2 streaming path currently requires 3-band RGB imagery (got {bands} bands)"
            ),
        };

        if bits_per_sample != 8 {
            bail!("JP2 fast path currently supports 8-bit imagery (got {bits_per_sample}-bit)");
        }

        Ok(Self {
            width,
            height,
            bands,
            bits_per_sample,
            photometric,
        })
    }
}
