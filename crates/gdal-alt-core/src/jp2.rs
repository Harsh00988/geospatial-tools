use anyhow::{Context, Result};
use jpeg2k::DumpImage;
use tiff_core::PhotometricInterpretation;

#[derive(Debug, Clone)]
pub struct Jp2Header {
    pub width: u32,
    pub height: u32,
    pub bands: u32,
    pub bits_per_sample: u8,
    pub photometric: PhotometricInterpretation,
}

impl Jp2Header {
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
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
            1 => PhotometricInterpretation::MinIsBlack,
            3 => PhotometricInterpretation::Rgb,
            4 => PhotometricInterpretation::Rgb,
            _ => PhotometricInterpretation::MinIsBlack,
        };

        Ok(Self {
            width,
            height,
            bands,
            bits_per_sample,
            photometric,
        })
    }
}
