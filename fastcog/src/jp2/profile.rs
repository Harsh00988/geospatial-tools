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
        if bands == 0 {
            bail!("JP2 image has no components");
        }

        let components: Vec<_> = dump.img.components().iter().collect();
        let bits_per_sample = components[0].precision() as u8;
        let signed = components[0].is_signed();
        for component in &components {
            if component.precision() as u8 != bits_per_sample {
                bail!("JP2 components must share the same bit depth");
            }
            if component.is_signed() != signed {
                bail!("JP2 components must share the same signedness");
            }
        }
        if signed {
            bail!("signed JP2 components are not supported yet");
        }
        if !matches!(bits_per_sample, 8 | 12 | 16) {
            bail!("JP2 fast path supports 8/12/16-bit unsigned imagery (got {bits_per_sample}-bit)");
        }

        let photometric = match bands {
            1 => PhotometricInterpretation::MinIsBlack,
            3 => PhotometricInterpretation::Rgb,
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
