use anyhow::{bail, Context, Result};
use jpeg2k::DumpImage;
use tiff_core::{PhotometricInterpretation, SampleFormat};

#[derive(Debug, Clone, Copy)]
pub struct Jp2Raster {
    pub width: u32,
    pub height: u32,
    pub bands: u32,
    pub bits_per_sample: u8,
    pub sample_format: SampleFormat,
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
        if !matches!(bits_per_sample, 8 | 12 | 16) {
            bail!("JP2 supports 8/12/16-bit imagery (got {bits_per_sample}-bit)");
        }

        let sample_format = if signed {
            SampleFormat::Int
        } else {
            SampleFormat::Uint
        };

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
            sample_format,
            photometric,
        })
    }

    pub fn with_band_subset(&self, bands: &[usize]) -> Result<Self> {
        if bands.is_empty() {
            bail!("at least one band must be selected");
        }
        for band in bands {
            if *band == 0 || *band > self.bands as usize {
                bail!("band {band} is out of range (1..={})", self.bands);
            }
        }
        let mut subset = *self;
        subset.bands = bands.len() as u32;
        Ok(subset)
    }
}
