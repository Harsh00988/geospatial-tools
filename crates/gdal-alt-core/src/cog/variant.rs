use geotiff_writer::TiffVariant;

const BIGTIFF_THRESHOLD_BYTES: u64 = 3_500_000_000;

pub fn tiff_variant(width: u32, height: u32, bands: u32, bits_per_sample: u16) -> TiffVariant {
    let bytes_per_sample = u64::from(bits_per_sample.max(1));
    let pixels = u64::from(width) * u64::from(height) * u64::from(bands);
    let uncompressed = pixels.saturating_mul(bytes_per_sample);
    if uncompressed > BIGTIFF_THRESHOLD_BYTES {
        TiffVariant::BigTiff
    } else {
        TiffVariant::Auto
    }
}
