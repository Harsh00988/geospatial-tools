use anyhow::{bail, Context, Result};
use geotiff_reader::GeoTiffFile;
use geotiff_writer::RemuxCompressedBlock;
use rayon::prelude::*;
use tiff_core::{ByteOrder, Compression, PlanarConfiguration, SampleFormat};
use tiff_reader::source::TiffSource;
use tiff_reader::{Ifd, TiffFile};

#[derive(Debug, Clone, Copy)]
struct GdalStructuralMetadata {
    block_leader_size_as_u32: bool,
    block_trailer_repeats_last_4_bytes: bool,
}

const GDAL_STRUCTURAL_METADATA_PREFIX: &str = "GDAL_STRUCTURAL_METADATA_SIZE=";
const MAX_BLOCK_BYTES: usize = 64 * 1024 * 1024;

pub fn read_layer_block_at(
    tiff: &TiffFile,
    ifd: &Ifd,
    block_index: usize,
) -> Result<RemuxCompressedBlock> {
    let offsets = ifd
        .tile_offsets()
        .context("remux requires tiled IFD with tile offsets")?;
    let counts = ifd
        .tile_byte_counts()
        .context("remux requires tiled IFD with tile byte counts")?;
    if offsets.len() != counts.len() {
        bail!("tile offset/count length mismatch");
    }
    let (&offset, &count) = offsets
        .get(block_index)
        .zip(counts.get(block_index))
        .ok_or_else(|| anyhow::anyhow!("block index {block_index} out of range"))?;

    if offset == 0 || count == 0 {
        return Ok(RemuxCompressedBlock {
            payload: Vec::new(),
            sparse: true,
        });
    }

    let gdal_meta = parse_gdal_structural_metadata(tiff.source());
    let payload = read_gdal_block_payload(
        tiff.source(),
        gdal_meta.as_ref(),
        tiff.byte_order(),
        offset,
        count,
        block_index,
    )?;
    Ok(RemuxCompressedBlock {
        payload,
        sparse: false,
    })
}


pub fn read_layer_blocks(tiff: &TiffFile, ifd: &Ifd) -> Result<Vec<RemuxCompressedBlock>> {
    let offsets = ifd
        .tile_offsets()
        .context("remux requires tiled IFD with tile offsets")?;
    let counts = ifd
        .tile_byte_counts()
        .context("remux requires tiled IFD with tile byte counts")?;
    if offsets.len() != counts.len() {
        bail!("tile offset/count length mismatch");
    }

    let gdal_meta = parse_gdal_structural_metadata(tiff.source());
    let byte_order = tiff.byte_order();
    let source = tiff.source();

    offsets
        .par_iter()
        .zip(counts.par_iter())
        .enumerate()
        .map(|(index, (&offset, &count))| {
            if offset == 0 || count == 0 {
                return Ok(RemuxCompressedBlock {
                    payload: Vec::new(),
                    sparse: true,
                });
            }
            let payload = read_gdal_block_payload(
                source,
                gdal_meta.as_ref(),
                byte_order,
                offset,
                count,
                index,
            )?;
            Ok(RemuxCompressedBlock {
                payload,
                sparse: false,
            })
        })
        .collect()
}

fn parse_gdal_structural_metadata(source: &dyn TiffSource) -> Option<GdalStructuralMetadata> {
    let available_len = usize::try_from(source.len().checked_sub(8)?).ok()?;
    if available_len == 0 {
        return None;
    }

    let probe_len = available_len.min(64);
    let probe = source.read_exact_at(8, probe_len).ok()?;
    let total_len = parse_gdal_structural_metadata_len(&probe)?;
    if total_len == 0 || total_len > available_len {
        return None;
    }

    let bytes = source.read_exact_at(8, total_len).ok()?;
    GdalStructuralMetadata::from_prefix(&bytes)
}

fn parse_gdal_structural_metadata_len(bytes: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(bytes).ok()?;
    let newline_index = text.find('\n')?;
    let header = &text[..newline_index];
    let value = header.strip_prefix(GDAL_STRUCTURAL_METADATA_PREFIX)?;
    let digits: String = value.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let payload_len: usize = digits.parse().ok()?;
    newline_index.checked_add(1)?.checked_add(payload_len)
}

impl GdalStructuralMetadata {
    fn from_prefix(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        if !text.contains("GDAL_STRUCTURAL_METADATA_SIZE=") {
            return None;
        }

        Some(Self {
            block_leader_size_as_u32: text.contains("BLOCK_LEADER=SIZE_AS_UINT4"),
            block_trailer_repeats_last_4_bytes: text
                .contains("BLOCK_TRAILER=LAST_4_BYTES_REPEATED"),
        })
    }

    fn unwrap_block<'a>(&self, raw: &'a [u8], byte_order: ByteOrder) -> Result<&'a [u8]> {
        if self.block_leader_size_as_u32 {
            if raw.len() < 4 {
                return Ok(raw);
            }
            let payload_len = match byte_order {
                ByteOrder::LittleEndian => u32::from_le_bytes(raw[..4].try_into().unwrap()),
                ByteOrder::BigEndian => u32::from_be_bytes(raw[..4].try_into().unwrap()),
            } as usize;
            let payload_end = 4usize.checked_add(payload_len).unwrap_or(raw.len());
            if payload_end <= raw.len() {
                if self.block_trailer_repeats_last_4_bytes {
                    let trailer_end = payload_end.checked_add(4);
                    if let Some(trailer_end) = trailer_end {
                        if trailer_end <= raw.len() {
                            let expected = &raw[payload_end - 4..payload_end];
                            let trailer = &raw[payload_end..trailer_end];
                            if expected == trailer {
                                return Ok(&raw[4..payload_end]);
                            }
                        }
                    }
                }
                return Ok(&raw[4..payload_end]);
            }
        }

        if self.block_trailer_repeats_last_4_bytes && raw.len() >= 8 {
            let split = raw.len() - 4;
            if raw[split - 4..split] == raw[split..] {
                return Ok(&raw[..split]);
            }
        }

        Ok(raw)
    }
}

fn read_gdal_block_payload(
    source: &dyn TiffSource,
    metadata: Option<&GdalStructuralMetadata>,
    byte_order: ByteOrder,
    offset: u64,
    byte_count: u64,
    index: usize,
) -> Result<Vec<u8>> {
    let payload_len = usize::try_from(byte_count)
        .map_err(|_| anyhow::anyhow!("tile {index} byte count overflows usize"))?;
    if payload_len > MAX_BLOCK_BYTES {
        bail!("tile {index} byte count {payload_len} exceeds limit");
    }

    if let Some(metadata) = metadata {
        if metadata.block_leader_size_as_u32 && offset >= 4 {
            if let Ok(payload) = read_wrapped_gdal_block(
                source,
                metadata,
                byte_order,
                offset,
                byte_count,
                index,
            ) {
                if payload.len() == payload_len {
                    return Ok(payload);
                }
            }
        }
    }

    let raw = source
        .read_exact_at(offset, payload_len)
        .with_context(|| format!("failed to read tile {index} at offset {offset}"))?;
    if let Some(metadata) = metadata {
        Ok(metadata.unwrap_block(&raw, byte_order)?.to_vec())
    } else {
        Ok(raw)
    }
}

fn read_wrapped_gdal_block(
    source: &dyn TiffSource,
    metadata: &GdalStructuralMetadata,
    byte_order: ByteOrder,
    offset: u64,
    byte_count: u64,
    index: usize,
) -> Result<Vec<u8>> {
    let wrapper_extra = if metadata.block_trailer_repeats_last_4_bytes {
        8u64
    } else {
        4u64
    };
    let wrapped_offset = offset - 4;
    let wrapped_len = byte_count
        .checked_add(wrapper_extra)
        .ok_or_else(|| anyhow::anyhow!("tile {index} wrapped length overflow"))?;
    let len = usize::try_from(wrapped_len)
        .map_err(|_| anyhow::anyhow!("tile {index} wrapped length overflows usize"))?;
    if len > MAX_BLOCK_BYTES.saturating_add(8) {
        bail!("tile {index} wrapped byte count too large");
    }
    let raw = source
        .read_exact_at(wrapped_offset, len)
        .with_context(|| format!("failed to read wrapped tile {index}"))?;
    Ok(metadata.unwrap_block(&raw, byte_order)?.to_vec())
}

pub fn input_compression(ifd: &Ifd) -> Compression {
    Compression::from_code(ifd.compression()).unwrap_or(Compression::None)
}

pub fn ifd_planar(ifd: &Ifd) -> PlanarConfiguration {
    PlanarConfiguration::from_code(ifd.planar_configuration())
        .unwrap_or(PlanarConfiguration::Chunky)
}

pub fn ifd_sample_format(ifd: &Ifd) -> Result<SampleFormat> {
    let codes = ifd.sample_format()?;
    let code = *codes.first().unwrap_or(&1);
    SampleFormat::from_code(code).ok_or_else(|| anyhow::anyhow!("unsupported sample format {code}"))
}

/// True when compressed tile/strip payloads can be copied verbatim into the output COG layer.
pub fn layer_blocks_copyable(
    ifd: &Ifd,
    opts: &crate::cog::CogOutputOptions,
    sample_format: SampleFormat,
) -> bool {
    if !compression_matches_ifd(ifd, opts) {
        return false;
    }
    if !predictor_matches_ifd(ifd, opts, sample_format) {
        return false;
    }
    let (tile_w, tile_h) = match (ifd.tile_width(), ifd.tile_height()) {
        (Some(w), Some(h)) => (w, h),
        _ => return false,
    };
    tile_w == opts.blocksize && tile_h == opts.blocksize
}

pub(crate) fn compression_matches_ifd(
    ifd: &Ifd,
    opts: &crate::cog::CogOutputOptions,
) -> bool {
    opts.compression.to_compression() == input_compression(ifd)
}

pub(crate) fn predictor_matches_ifd(
    ifd: &Ifd,
    opts: &crate::cog::CogOutputOptions,
    sample_format: SampleFormat,
) -> bool {
    use tiff_core::Predictor;
    let src = Predictor::from_code(ifd.predictor()).unwrap_or(Predictor::None);
    let dst = opts.encode_predictor_for(sample_format);
    src == dst
}

#[cfg(test)]
mod copyable_tests {
    use crate::cog::{CompressionChoice, CogOutputOptions, LercAdditionalCompressionChoice, ResamplingChoice};
    use tiff_core::{Compression, Predictor, SampleFormat};

    fn opts(compression: CompressionChoice, blocksize: u32) -> CogOutputOptions {
        CogOutputOptions {
            blocksize,
            compression,
            deflate_level: 6,
            resampling: ResamplingChoice::Average,
            overview_levels: None,
            no_overviews: false,
            mask_from_alpha: true,
            black_rgb_transparent: false,
            jpeg_quality: 75,
            lerc_max_z_error: 0.0,
            lerc_additional_compression: LercAdditionalCompressionChoice::None,
        }
    }

    #[test]
    fn deflate_opts_match_deflate_compression() {
        assert_eq!(
            opts(CompressionChoice::Deflate, 512).compression.to_compression(),
            Compression::Deflate
        );
    }

    #[test]
    fn encode_predictor_none_for_float_even_with_deflate() {
        let o = opts(CompressionChoice::Deflate, 512);
        assert_eq!(o.encode_predictor_for(SampleFormat::Float), Predictor::None);
        assert_eq!(
            o.encode_predictor_for(SampleFormat::Uint),
            Predictor::Horizontal
        );
    }
}

pub fn collect_remux_layers(
    input: &GeoTiffFile,
    progress: Option<&crate::progress::StageBar>,
) -> Result<Vec<Vec<RemuxCompressedBlock>>> {
    let tiff = input.tiff();
    let layer_count = 1 + input.overview_count();
    (0..layer_count)
        .into_par_iter()
        .map(|layer_index| {
            let ifd = if layer_index == 0 {
                tiff.ifd(input.base_ifd_index())?
            } else {
                input.overview_ifd(layer_index - 1)?
            };
            let blocks = read_layer_blocks(tiff, ifd)?;
            if let Some(bar) = progress {
                bar.inc(1);
            }
            Ok(blocks)
        })
        .collect()
}
