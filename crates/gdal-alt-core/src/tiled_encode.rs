use anyhow::Result;
use geotiff_reader::GeoTiffFile;
use geotiff_writer::{RemuxCompressedBlock, RemuxTileEncoding};
use tiff_reader::TiffSample;

use crate::cog::{overview_levels, CogOutputOptions};
use crate::crop::WriteWindow;
use crate::input::RasterProfile;
use crate::remux::remux_encoded_layers;
use crate::strip_encode::{
    build_base_layer_from_rows, build_strip_overview_from_decoded,
    build_strip_overview_layer_with_cache, output_tile_encoding, StripTile,
};

pub fn convert_tiled_to_remux_cog<T>(
    pool: &rayon::ThreadPool,
    input: &GeoTiffFile,
    output: &std::path::Path,
    profile: &RasterProfile,
    opts: &CogOutputOptions,
    window: Option<WriteWindow>,
    band_map: Option<&[usize]>,
) -> Result<()>
where
    T: TiffSample + geotiff_writer::NumericSample + Copy + Default + Send + Sync,
{
    let width = profile.width;
    let height = profile.height;
    let out_bands = profile.bands as usize;
    let tile_size = opts.blocksize as usize;
    let levels = overview_levels(opts, width, height);
    let encoding = output_tile_encoding(opts, tile_size, out_bands as u16);

    let layers = pool.install(|| {
        let mut layers = Vec::with_capacity(1 + levels.len());
        layers.push(build_base_layer_from_rows::<T>(
            input,
            width,
            height,
            tile_size,
            out_bands,
            window,
            band_map,
            encoding,
        )?);

        let mut parent_decoded: Option<Vec<(usize, usize, StripTile<T>)>> = None;
        let mut parent_level = 1u32;
        for (level_idx, &level) in levels.iter().enumerate() {
            let chain_next = levels
                .iter()
                .skip(level_idx + 1)
                .any(|&next| next == level * 2);
            let (layer, decoded) = if let Some(parent) = parent_decoded.take() {
                if level == parent_level * 2 {
                    build_strip_overview_from_decoded::<T>(
                        parent,
                        width,
                        height,
                        level,
                        parent_level,
                        tile_size,
                        out_bands,
                        encoding,
                        opts,
                        chain_next,
                    )?
                } else {
                    build_strip_overview_layer_with_cache::<T>(
                        input,
                        width,
                        height,
                        level,
                        tile_size,
                        out_bands,
                        window,
                        band_map,
                        encoding,
                        opts,
                        chain_next,
                    )?
                }
            } else {
                build_strip_overview_layer_with_cache::<T>(
                    input,
                    width,
                    height,
                    level,
                    tile_size,
                    out_bands,
                    window,
                    band_map,
                    encoding,
                    opts,
                    chain_next,
                )?
            };

            if chain_next {
                parent_decoded = Some(decoded);
                parent_level = level;
            }
            layers.push(layer);
        }

        Ok::<_, anyhow::Error>(layers)
    })?;

    remux_encoded_layers(profile, opts, layers, output, Some(levels), None)
}
